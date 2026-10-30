//! Streaming segmented proving.
//!
//! Instead of materializing the full [`Trace`] and then proving it in a second
//! phase (see [`Prover::chunked_prove_local`]), the streaming coordinator
//! splits the model into segments *before* the run (from the graph structure
//! and an input shape only — no trace needed), and proves each segment as
//! soon as all of its nodes have been executed. The segment's tensors are
//! pinned in memory while its proof is pending and released right after, so
//! the peak memory is bounded by the largest segment (plus the prove working
//! set) instead of the whole trace.
//!
//! The produced [`ChunkProof`]s are merged into a [`Proof`] identical in
//! structure to the one of the chunked prover: all chunks share one common
//! challenge and each chunk transcript is seeded with it (see
//! `docs/zkml_bootcamp/K_流式证明协议审计与挑战值派生.md`); cross-segment
//! consistency is enforced at verification time via the boundary commitment
//! equality, so no changes to the verifier are required.
//!
//! Phase 2 of the streaming project replaces the fixed common challenge with
//! a global sequential transcript (each segment's seed derived after all
//! previous segments' commitments are absorbed).

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::mpsc::Receiver;

use ark_ff::PrimeField;
use dp_crypto::arkyper::{CommitmentScheme, transcript::Transcript};
use tenstore::StorageKey;
use tracing::debug;

use crate::{
    Element, InitTranscript, Proof, measure,
    graph::NodeId,
    iop::{
        ChunkProof, ProverContext,
        chunking::{ChunkID, ModelChunk, initialize_transcript_for_chunk},
        prover::Prover,
        verifier::{
            IO, bind_chunk_count_to_transcript, bind_io_inputs_to_transcript,
            bind_proven_chunk_to_transcript,
        },
    },
    model::{Model, ToStorageKey, Trace, trace::Step},
    quantization::ToField,
    tensor::TensorHandle,
};

/// Stack size for the dedicated proving thread.
///
/// The prove recursion (sumcheck/expression evaluation and recursive drops of
/// large per-chunk structures) needs more stack than small test threads
/// provide; a dedicated thread with an explicit stack makes the prove
/// independent of the caller's thread stack. This thread is also the natural
/// place where a future pipeline prover (or an FPGA bridge) will plug in.
const PROVE_THREAD_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Orchestrates the segmented proving of a trace that is being generated.
///
/// The coordinator is driven by the model run loop (see
/// [`crate::Model::run_and_prove_streaming`]):
///
/// 1. `chunk_of` + `keys_to_pin` let the run loop pin a segment's tensors
///    before its first node executes, so that intermediate tensors consumed
///    inside the segment are not dried before the segment's proof runs;
/// 2. `ingest_steps` receives every trace step recorded by the `TraceRunner`
///    tap;
/// 3. `on_node_complete` detects the completion of a segment and, in the
///    synchronous mode, immediately proves it;
/// 4. `finish` merges all the segment proofs and builds the verifier IO from
///    the model input/output of the fully-run trace.
pub struct StreamingProver<'a, 'ctx, F, T, PCS>
where
    F: PrimeField,
    T: Transcript,
    PCS: CommitmentScheme<Field = F> + 'static,
{
    ctx: &'ctx ProverContext<'a, F, PCS>,
    model: &'a Model<Element>,
    chunks: Vec<ModelChunk>,
    /// Inner nodes still to be executed, per chunk id.
    remaining: HashMap<ChunkID, usize>,
    /// Chunk each inner node belongs to.
    node_to_chunk: HashMap<NodeId, ChunkID>,
    /// Storage keys a chunk's proof reads: input and output tensors of all its
    /// nodes. Proving data is never dried by the lifetime runner (it holds no
    /// usage counting), so it needs no pinning.
    chunk_keys: HashMap<ChunkID, Vec<StorageKey<Vec<Element>>>>,
    /// Trace steps accumulated per chunk, as sent by the `TraceRunner` tap.
    steps: HashMap<ChunkID, HashMap<NodeId, Step<Element>>>,
    /// Step tap receiver.
    step_rx: Receiver<(NodeId, Step<Element>)>,
    /// Global sequential transcript of the run (plan A of the streaming
    /// protocol audit): absorbs the setup, the model input binding and the
    /// chunk count before deriving the first seed, then the identity of every
    /// proven chunk before deriving the next seed. Verifier counterpart:
    /// `Verifier::verify_streaming`.
    global_transcript: T,
    /// Seed of the transcript of the chunk currently being proven.
    current_seed: F,
    /// Completed proofs, in completion order.
    proofs: Vec<(ChunkID, ChunkProof<F, PCS>)>,
    /// The transcript type is only used to derive the common challenge.
    _phantom: PhantomData<T>,
}

impl<'a, 'ctx, F, T, PCS> StreamingProver<'a, 'ctx, F, T, PCS>
where
    F: PrimeField,
    T: Transcript + InitTranscript,
    PCS: CommitmentScheme<Field = F> + 'static,
{
    /// Builds the streaming plan from precomputed chunks and derives the
    /// first chunk seed from the global sequential transcript:
    /// `setup ‖ input binding ‖ chunk count` (plan A of the streaming
    /// protocol audit, docs/zkml_bootcamp/K_*.md §5.2).
    ///
    /// `input_handles` are the raw model input handles (they must still be
    /// wet); they are padded and fieldized exactly like `to_verifier_io` does,
    /// so that the prover-side input binding matches the verifier-side one
    /// over `io.input`.
    ///
    /// Returns the coordinator and the storage keys of the model inputs, which
    /// the run loop must pin for the whole run (they are needed wet at the end
    /// to build the verifier IO without touching the disk store).
    pub fn new(
        ctx: &'ctx ProverContext<'a, F, PCS>,
        model: &'a Model<Element>,
        chunks: Vec<ModelChunk>,
        input_handles: &[TensorHandle<Element>],
        step_rx: Receiver<(NodeId, Step<Element>)>,
    ) -> anyhow::Result<(Self, Vec<StorageKey<Vec<Element>>>)> {
        let mut remaining = HashMap::new();
        let mut node_to_chunk = HashMap::new();
        let mut chunk_keys = HashMap::new();
        for chunk in &chunks {
            let nodes: Vec<NodeId> = chunk.subgraph.inner_nodes().map(|(id, _)| id).collect();
            remaining.insert(chunk.chunk_id, nodes.len());
            for node_id in &nodes {
                node_to_chunk.insert(*node_id, chunk.chunk_id);
            }
            // input keys: sources of every node's incoming feeds; output keys:
            // every node's outgoing ports. Keys are deterministic from the
            // graph, so the pin sets are fully computable before the run.
            let mut keys = HashSet::new();
            for node_id in &nodes {
                for feed in model.graph().incoming_feeds(*node_id) {
                    keys.insert(feed.source.to_storage_key());
                }
                for port in model.graph().outgoing_ports(*node_id) {
                    keys.insert(port.to_storage_key());
                }
            }
            chunk_keys.insert(chunk.chunk_id, keys.into_iter().collect());
        }
        // model inputs stay wet for the whole run: they feed the first chunk
        // and, at the end, the verifier IO.
        let model_input_keys = model
            .graph()
            .input_node_ids()
            .iter()
            .map(|id| id.output_at(0).to_storage_key())
            .collect();

        let mut global_transcript = Prover::<F, T, PCS>::initialise_transcript(ctx)?;
        // bind this run's model input (W1): the padded fieldized inputs, the
        // same transformation `to_verifier_io` applies to `trace.input`
        let bound_inputs = input_handles
            .iter()
            .map(|handle| {
                let tensor_guard = handle.tensor()?;
                let padded = (*tensor_guard).pad_next_power_of_two();
                Ok(padded.to_field())
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        bind_io_inputs_to_transcript::<F, T>(&mut global_transcript, &bound_inputs);
        // bind the number of chunks (W3)
        bind_chunk_count_to_transcript(&mut global_transcript, chunks.len());
        let current_seed = global_transcript.challenge_scalar();

        Ok((
            Self {
                ctx,
                model,
                chunks,
                remaining,
                node_to_chunk,
                chunk_keys,
                steps: HashMap::new(),
                step_rx,
                global_transcript,
                current_seed,
                proofs: Vec::new(),
                _phantom: PhantomData,
            },
            model_input_keys,
        ))
    }

    /// Returns the chunk `node_id` belongs to, if any.
    pub fn chunk_of(&self, node_id: &NodeId) -> Option<ChunkID> {
        self.node_to_chunk.get(node_id).copied()
    }

    /// The storage keys whose in-memory copies must be kept while the given
    /// chunk is being generated and proven.
    pub fn keys_to_pin(&self, chunk_id: ChunkID) -> &[StorageKey<Vec<Element>>] {
        self.chunk_keys
            .get(&chunk_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Consumes the steps sent by the `TraceRunner` tap.
    pub fn ingest_steps(&mut self) {
        while let Ok((node_id, step)) = self.step_rx.try_recv() {
            let Some(chunk_id) = self.node_to_chunk.get(&node_id).copied() else {
                debug!("Streaming: step for node {node_id} outside any chunk, ignoring");
                continue;
            };
            self.steps.entry(chunk_id).or_default().insert(node_id, step);
        }
    }

    /// To be called after every executed node: completes and proves the chunk
    /// the node belongs to when it was its last missing node. On completion
    /// the caller must release the chunk's pins via
    /// [`StreamingProver::keys_to_pin`].
    pub fn on_node_complete(&mut self, node_id: &NodeId) -> anyhow::Result<Option<ChunkID>> {
        let Some(chunk_id) = self.chunk_of(node_id) else {
            return Ok(None);
        };
        let remaining = self
            .remaining
            .get_mut(&chunk_id)
            .ok_or(anyhow::anyhow!("Chunk {chunk_id} not tracked"))?;
        *remaining = remaining
            .checked_sub(1)
            .ok_or(anyhow::anyhow!("Node {node_id} counted twice for chunk {chunk_id}"))?;
        if *remaining > 0 {
            return Ok(None);
        }

        // prove the chunk
        let chunk_index = self
            .chunks
            .iter()
            .position(|c| c.chunk_id == chunk_id)
            .ok_or(anyhow::anyhow!("Chunk {chunk_id} not found"))?;
        let chunk = self.chunks.swap_remove(chunk_index);
        let steps = self
            .steps
            .remove(&chunk_id)
            .ok_or(anyhow::anyhow!("No steps recorded for chunk {chunk_id}"))?;
        debug!(
            "== [streaming] chunk {chunk_id} complete: {} nodes, proving... ==",
            steps.len()
        );
        let chunk_trace = Trace {
            steps,
            input: vec![],  // unused by a chunk prover
            output: vec![], // unused by a chunk prover
            ..Default::default()
        };
        let chunk_layers = chunk.chunk_layers(self.model);
        let seed = self.current_seed;
        let ctx = self.ctx;
        // prove on a dedicated thread with an explicit stack: the prove
        // recursion needs more stack than the caller's thread may provide
        // (e.g. small test threads), and this thread is where a future
        // pipeline prover (or an FPGA bridge) will plug in
        let proof = measure::r("streaming_chunk_prove", || {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .stack_size(PROVE_THREAD_STACK_SIZE)
                    .spawn_scoped(scope, move || {
                        let mut transcript: T = initialize_transcript_for_chunk(seed);
                        let prover = Prover::new(ctx, &mut transcript);
                        prover.prove_chunk(chunk, &chunk_trace, &chunk_layers)
                    })
                    .expect("Failed to spawn the proving thread")
                    .join()
                    .expect("Proving thread panicked")
            })
        })?;
        // absorb the proven chunk identity into the global transcript and
        // derive the seed of the next chunk (W2): the chunk's structure and
        // its IO commitments are exactly what the verifier replays
        let model_chunk = proof.chunk_data.model_chunk.clone();
        let commitments = proof.chunk_data.commitments.clone();
        bind_proven_chunk_to_transcript::<F, PCS, T>(
            &mut self.global_transcript,
            &model_chunk,
            &commitments,
        )?;
        self.current_seed = self.global_transcript.challenge_scalar();
        self.proofs.push((chunk_id, proof));
        Ok(Some(chunk_id))
    }

    /// Merges all the collected chunk proofs (in chunk-id order) and builds
    /// the verifier IO from the model input/output of the completed trace.
    pub fn finish(mut self, trace: &Trace<Element>) -> anyhow::Result<(Proof<F, PCS>, IO<F>)> {
        // drain any pending steps so that a missing chunk is detected as such
        self.ingest_steps();
        let missing: Vec<_> = self
            .remaining
            .iter()
            .filter(|(_, remaining)| **remaining > 0)
            .map(|(id, remaining)| (id, remaining))
            .collect();
        anyhow::ensure!(
            missing.is_empty(),
            "Cannot finalize streaming proof: incomplete chunks {missing:?}"
        );
        // NOTE: the proofs are kept in **completion order**: it is the order
        // in which the chunks were absorbed into the global transcript, and
        // the streaming verifier replays the absorption in exactly this
        // (Vec) order to derive the same seed chain. Chunks are
        // self-contained, so — just like for the classic chunked prover — no
        // chunk-id ordering is required.
        let proofs = self
            .proofs
            .into_iter()
            .map(|(_, proof)| proof)
            .collect::<Vec<_>>();
        debug!("== [streaming] merging {} chunk proofs ==", proofs.len());
        let proof = proofs.into();
        let io = trace.to_verifier_io::<F>()?;
        Ok((proof, io))
    }
}
