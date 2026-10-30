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
//! The produced [`ChunkProof`]s are merged into a [`Proof`] with the same
//! structure as the chunked prover's one; the per-chunk seeds are derived
//! from the global sequential transcript (plan A of
//! `docs/zkml_bootcamp/K_流式证明协议审计与挑战值派生.md` §5.2), replayed by
//! the verifier in [`crate::verify_streaming`]. Segments are absorbed in
//! **completion order**, which is exactly the order of the final
//! `Proof.chunk_proofs` Vec.
//!
//! Two execution modes ([`StreamingMode`]):
//!
//! * `Sync`: the run loop blocks until each completed chunk is proven
//!   (minimal memory, no overlap between generation and proving);
//! * `Pipeline`: a resident proving thread consumes the completed segments
//!   from a bounded channel while the run loop keeps generating — the total
//!   time approaches `max(trace generation, proving)`. The channel capacity
//!   (window) bounds the number of completed-but-unproven chunks, hence the
//!   memory. This thread is also the natural place where an FPGA/ASIC bridge
//!   will plug in: it receives `(chunk structure, sub-trace tensors, seed)`
//!   per segment — all serializable — and returns serialized `ChunkProof`s
//!   (see `docs/zkml_bootcamp/J_流式Trace证明机制.md` for the contract).

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::ScopedJoinHandle;

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
        prover::{ModelLayersRef, Prover},
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

/// How the completed segments are proven.
#[derive(Clone, Copy, Debug)]
pub enum StreamingMode {
    /// The run loop blocks until each completed chunk is proven: minimal
    /// memory, no overlap between generation and proving.
    Sync,
    /// A resident proving thread consumes the completed segments while the
    /// run loop keeps generating. `window` is the maximum number of
    /// completed-but-unproven segments kept in memory (backpressure: the run
    /// loop blocks when the window is full).
    Pipeline { window: usize },
}

/// The sequential seed chain of the streaming derivation (plan A).
///
/// Holds the global transcript `S` and the seed of the chunk currently being
/// proven: `S = H(setup ‖ input binding ‖ chunk count)`, then every proven
/// chunk (structure + IO commitments) is absorbed before deriving the next
/// seed. The same chain is replayed by `Verifier::verify_streaming`.
pub(crate) struct SeedChain<F: PrimeField, T: Transcript> {
    global: T,
    seed: F,
}

/// Builds the seed chain from the prover context, the padded fieldized model
/// inputs (the same transformation `to_verifier_io` applies to
/// `trace.input`) and the number of chunks.
fn build_seed_chain<F, T, PCS>(
    ctx: &ProverContext<F, PCS>,
    bound_inputs: &[crate::tensor::Tensor<crate::SerializableField<F>>],
    num_chunks: usize,
) -> anyhow::Result<SeedChain<F, T>>
where
    F: PrimeField,
    T: Transcript + InitTranscript,
    PCS: CommitmentScheme<Field = F> + 'static,
{
    let mut global = Prover::<F, T, PCS>::initialise_transcript(ctx)?;
    bind_io_inputs_to_transcript::<F, T>(&mut global, bound_inputs);
    bind_chunk_count_to_transcript(&mut global, num_chunks);
    let seed = global.challenge_scalar();
    Ok(SeedChain { global, seed })
}

impl<F: PrimeField, T: Transcript> SeedChain<F, T> {
    /// The seed of the chunk to be proven next.
    fn seed(&self) -> F {
        self.seed
    }

    /// Absorbs a proven chunk and derives the seed of the next one.
    fn absorb<PCS>(
        &mut self,
        model_chunk: &ModelChunk,
        commitments: &crate::iop::chunking::ChunkIOCommitments<
            crate::poly_commit::verifier::VerifierCommitment<PCS>,
        >,
    ) -> anyhow::Result<F>
    where
        PCS: CommitmentScheme<Field = F>,
    {
        bind_proven_chunk_to_transcript::<F, PCS, T>(&mut self.global, model_chunk, commitments)?;
        self.seed = self.global.challenge_scalar();
        Ok(self.seed)
    }
}

/// A completed segment waiting to be proven.
struct ProveTask<'a> {
    chunk_id: ChunkID,
    chunk: ModelChunk,
    chunk_trace: Trace<Element>,
    chunk_layers: ModelLayersRef<'a>,
}

/// Proves one chunk task. Must be called on a thread with a sufficiently
/// large stack (see [`PROVE_THREAD_STACK_SIZE`]).
fn prove_task<F, T, PCS>(
    ctx: &ProverContext<F, PCS>,
    seed: F,
    chunk: ModelChunk,
    chunk_trace: &Trace<Element>,
    chunk_layers: &ModelLayersRef,
) -> anyhow::Result<ChunkProof<F, PCS>>
where
    F: PrimeField,
    T: Transcript + InitTranscript,
    PCS: CommitmentScheme<Field = F> + 'static,
{
    let mut transcript: T = initialize_transcript_for_chunk(seed);
    let prover = Prover::new(ctx, &mut transcript);
    prover.prove_chunk(chunk, chunk_trace, chunk_layers)
}

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
/// 3. `on_node_complete` detects the completion of a segment and hands it to
///    the proving backend ([`StreamingMode::Sync`] proves it right away,
///    [`StreamingMode::Pipeline`] submits it to the resident proving thread);
/// 4. `take_released` returns the chunk ids whose proofs completed and whose
///    memory pins can be released;
/// 5. `finish` waits for the backend, merges all the segment proofs and
///    builds the verifier IO from the model input/output of the fully-run
///    trace.
pub struct StreamingProver<'a, 'ctx, 'scope, F, T, PCS>
where
    F: PrimeField,
    T: Transcript,
    PCS: CommitmentScheme<Field = F> + 'static,
{
    ctx: &'ctx ProverContext<'a, F, PCS>,
    model: &'a Model<Element>,
    chunks: Vec<ModelChunk>,
    mode: StreamingMode,
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
    /// Seed chain of the run (plan A). Owned by the coordinator in `Sync`
    /// mode, moved to the resident proving thread in `Pipeline` mode.
    seed_chain: Option<SeedChain<F, T>>,
    /// Completed proofs, in completion order. Shared with the resident
    /// proving thread in `Pipeline` mode.
    proofs: ProofsSlot<F, PCS>,
    /// `Pipeline` mode state.
    pipeline: Option<PipelineState<'scope, 'a>>,
    /// The transcript type is only used to derive the seed chain.
    _phantom: PhantomData<T>,
}

/// Where the completed proofs live, per mode.
enum ProofsSlot<F: PrimeField, PCS: CommitmentScheme> {
    /// `Sync` mode: accumulated by the coordinator itself.
    Local(Vec<(ChunkID, ChunkProof<F, PCS>)>),
    /// `Pipeline` mode: pushed by the resident proving thread.
    Shared(Arc<Mutex<Vec<ChunkProof<F, PCS>>>>),
}

struct PipelineState<'scope, 'a> {
    /// Bounded task channel: the run loop blocks when `window` tasks are
    /// queued (backpressure bounds the memory).
    task_tx: SyncSender<ProveTask<'a>>,
    /// Chunks whose proofs completed and whose pins can be released.
    release_rx: Receiver<ChunkID>,
    join: ScopedJoinHandle<'scope, anyhow::Result<()>>,
}

impl<'a, 'ctx, 'scope, F, T, PCS> StreamingProver<'a, 'ctx, 'scope, F, T, PCS>
where
    F: PrimeField,
    T: Transcript + InitTranscript + Send + 'static,
    PCS: CommitmentScheme<Field = F> + 'static,
    // the pipeline proving thread borrows the model and the prover context
    // for the whole scope
    'a: 'scope,
    'ctx: 'scope,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: &'ctx ProverContext<'a, F, PCS>,
        model: &'a Model<Element>,
        chunks: Vec<ModelChunk>,
        input_handles: &[TensorHandle<Element>],
        step_rx: Receiver<(NodeId, Step<Element>)>,
        scope: &'scope std::thread::Scope<'scope, '_>,
        mode: StreamingMode,
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
        let seed_chain = build_seed_chain::<F, T, PCS>(ctx, &bound_inputs, chunks.len())?;
        let mut seed_chain_opt: Option<SeedChain<F, T>> = Some(seed_chain);

        let (proofs, pipeline) = match mode {
            StreamingMode::Sync => (ProofsSlot::Local(Vec::new()), None),
            StreamingMode::Pipeline { window } => {
                let (task_tx, task_rx) = std::sync::mpsc::sync_channel(window);
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let proofs = Arc::new(Mutex::new(Vec::new()));
                let thread_proofs = Arc::clone(&proofs);
                // the seed chain moves to the resident proving thread, which
                // absorbs every proven chunk before deriving the next seed
                let thread_seed_chain = seed_chain_opt
                    .take()
                    .expect("seed chain created just above");
                let join = std::thread::Builder::new()
                    .stack_size(PROVE_THREAD_STACK_SIZE)
                    .spawn_scoped(scope, move || {
                        pipeline_prover_thread::<F, T, PCS>(
                            ctx,
                            task_rx,
                            release_tx,
                            thread_seed_chain,
                            thread_proofs,
                        )
                    })?;
                (
                    ProofsSlot::Shared(proofs),
                    Some(PipelineState {
                        task_tx,
                        release_rx,
                        join,
                    }),
                )
            }
        };

        Ok((
            Self {
                ctx,
                model,
                chunks,
                mode,
                remaining,
                node_to_chunk,
                chunk_keys,
                steps: HashMap::new(),
                step_rx,
                seed_chain: seed_chain_opt,
                proofs,
                pipeline,
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

    /// To be called after every executed node: when the node completes a
    /// chunk, the chunk is handed to the proving backend. Returns the ids of
    /// the chunks whose memory pins can be released.
    pub fn on_node_complete(&mut self, node_id: &NodeId) -> anyhow::Result<Vec<ChunkID>> {
        let Some(chunk_id) = self.chunk_of(node_id) else {
            return Ok(Vec::new());
        };
        let remaining = self
            .remaining
            .get_mut(&chunk_id)
            .ok_or(anyhow::anyhow!("Chunk {chunk_id} not tracked"))?;
        *remaining = remaining
            .checked_sub(1)
            .ok_or(anyhow::anyhow!("Node {node_id} counted twice for chunk {chunk_id}"))?;
        if *remaining > 0 {
            return Ok(Vec::new());
        }

        // assemble the completed chunk's sub-trace
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
            "== [streaming] chunk {chunk_id} complete: {} nodes, mode {:?} ==",
            steps.len(),
            self.mode
        );
        let chunk_trace = Trace {
            steps,
            input: vec![],  // unused by a chunk prover
            output: vec![], // unused by a chunk prover
            ..Default::default()
        };

        match self.mode {
            StreamingMode::Sync => {
                let chunk_layers = chunk.chunk_layers(self.model);
                let seed = self
                    .seed_chain
                    .as_ref()
                    .expect("seed chain present in sync mode")
                    .seed();
                let ctx = self.ctx;
                // prove on a dedicated thread with an explicit stack: the
                // prove recursion needs more stack than the caller's thread
                // may provide (e.g. small test threads)
                let proof = measure::r("streaming_chunk_prove", || {
                    std::thread::scope(|scope| {
                        std::thread::Builder::new()
                            .stack_size(PROVE_THREAD_STACK_SIZE)
                            .spawn_scoped(scope, move || {
                                prove_task::<F, T, PCS>(ctx, seed, chunk, &chunk_trace, &chunk_layers)
                            })
                            .expect("Failed to spawn the proving thread")
                            .join()
                            .expect("Proving thread panicked")
                    })
                })?;
                // absorb the proven chunk and derive the next seed (W2)
                let model_chunk = proof.chunk_data.model_chunk.clone();
                let commitments = proof.chunk_data.commitments.clone();
                self.seed_chain
                    .as_mut()
                    .expect("seed chain present in sync mode")
                    .absorb(&model_chunk, &commitments)?;
                if let ProofsSlot::Local(proofs) = &mut self.proofs {
                    proofs.push((chunk_id, proof));
                }
                Ok(vec![chunk_id])
            }
            StreamingMode::Pipeline { .. } => {
                let chunk_layers = chunk.chunk_layers(self.model);
                let pipeline = self.pipeline.as_ref().expect("pipeline state present");
                // backpressure: blocks while `window` chunks are pending
                pipeline
                    .task_tx
                    .send(ProveTask {
                        chunk_id,
                        chunk,
                        chunk_trace,
                        chunk_layers,
                    })
                    .map_err(|_| anyhow::anyhow!("Streaming proving thread terminated"))?;
                Ok(Vec::new())
            }
        }
    }

    /// Returns the ids of the chunks whose proofs completed since the last
    /// call (always empty in `Sync` mode, where the release happens right
    /// after proving).
    pub fn take_released(&mut self) -> Vec<ChunkID> {
        match &self.pipeline {
            Some(pipeline) => {
                let mut released = Vec::new();
                while let Ok(chunk_id) = pipeline.release_rx.try_recv() {
                    released.push(chunk_id);
                }
                released
            }
            None => Vec::new(),
        }
    }

    /// Waits for the proving backend, merges all the collected chunk proofs
    /// (in completion order) and builds the verifier IO from the model
    /// input/output of the completed trace.
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

        let proofs = match self.proofs {
            ProofsSlot::Local(proofs) => proofs.into_iter().map(|(_, p)| p).collect(),
            ProofsSlot::Shared(shared) => {
                // drop the task sender so the proving thread can terminate,
                // then wait for it to drain the remaining tasks
                let pipeline = self.pipeline.take().expect("pipeline state present");
                drop(pipeline.task_tx);
                pipeline.join.join().map_err(|_| {
                    anyhow::anyhow!("Streaming proving thread panicked")
                })??;
                let mut proofs = shared.lock().expect("proofs lock poisoned");
                std::mem::take(&mut *proofs)
            }
        };

        // NOTE: the proofs are kept in **completion order**: it is the order
        // in which the chunks were absorbed into the global transcript, and
        // the streaming verifier replays the absorption in exactly this
        // (Vec) order to derive the same seed chain. Chunks are
        // self-contained, so — just like for the classic chunked prover — no
        // chunk-id ordering is required.
        debug!("== [streaming] merging {} chunk proofs ==", proofs.len());
        let proof = proofs.into();
        let io = trace.to_verifier_io::<F>()?;
        Ok((proof, io))
    }
}

/// Body of the resident proving thread (`Pipeline` mode): consumes the
/// completed segments in FIFO order, proves each one and absorbs it into the
/// seed chain before proving the next (plan A ordering).
fn pipeline_prover_thread<F, T, PCS>(
    ctx: &ProverContext<F, PCS>,
    task_rx: Receiver<ProveTask<'_>>,
    release_tx: std::sync::mpsc::Sender<ChunkID>,
    mut seed_chain: SeedChain<F, T>,
    proofs: Arc<Mutex<Vec<ChunkProof<F, PCS>>>>,
) -> anyhow::Result<()>
where
    F: PrimeField,
    T: Transcript + InitTranscript + Send + 'static,
    PCS: CommitmentScheme<Field = F> + 'static,
{
    while let Ok(task) = task_rx.recv() {
        let ProveTask {
            chunk_id,
            chunk,
            chunk_trace,
            chunk_layers,
        } = task;
        let seed = seed_chain.seed();
        let proof = measure::r("streaming_chunk_prove", || {
            prove_task::<F, T, PCS>(ctx, seed, chunk, &chunk_trace, &chunk_layers)
        })?;
        // absorb the proven chunk and derive the next seed (W2)
        let model_chunk = proof.chunk_data.model_chunk.clone();
        let commitments = proof.chunk_data.commitments.clone();
        seed_chain.absorb(&model_chunk, &commitments)?;
        proofs.lock().expect("proofs lock poisoned").push(proof);
        // notify the run loop that this chunk's pins can be released; the run
        // loop may already be gone (e.g. an error aborted the run): a failed
        // notification must not fail the proof
        let _ = release_tx.send(chunk_id);
        // drop the segment's tensors (the handles are shared Arcs; the memory
        // goes away when the run loop releases the pins and dries them)
        drop(chunk_trace);
    }
    Ok(())
}
