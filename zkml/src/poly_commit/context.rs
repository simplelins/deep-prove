use anyhow::{Context, Result};
use itertools::Itertools;
use lazy_static::lazy_static;
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};

use ark_ff::PrimeField;
use ark_std::rand::Rng;
use dp_crypto::{
    ArcMultilinearExtension,
    arkyper::{
        CommitmentScheme,
        transcript::{AppendToTranscript, Transcript},
    },
    poly::dense::DensePolynomial,
};
use serde::{Deserialize, Serialize};
use tracing::debug;
use utils::Metrics;

use crate::{
    Element,
    graph::NodeId,
    lookup::table::Table,
    poly_commit::{
        ChunkedCommitment, num_vars_for_chunk, table_poly_id, verifier::VerifierCommitment,
    },
    tensor::CommitmentId,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CommittedPolynomial<'a, F: PrimeField, PCS: CommitmentScheme> {
    pub(super) chunk_commitments: Vec<PCS::Commitment>,
    /// Number of variables of the committed polynomial. Kept even when the Fr
    /// data is dropped (lazy mode) so metadata consumers keep working.
    pub(crate) poly_num_vars: usize,
    /// Fr data of the committed polynomial. `None` = lazily derivable: the
    /// polynomial was committed and its Fr data dropped to save memory (large
    /// model weights); it can be re-derived bit-exactly from the registered
    /// integer source when a claim needs it (chunk-end RLC/opening).
    pub(crate) polynomial: Option<ArcMultilinearExtension<'a, F>>,
}

impl<'a, F: PrimeField, PCS: CommitmentScheme> ChunkedCommitment
    for CommittedPolynomial<'a, F, PCS>
{
    fn num_chunks(&self) -> usize {
        self.chunk_commitments.len()
    }

    fn num_vars(&self) -> usize {
        self.poly_num_vars
    }
}

impl<'a, F: PrimeField, PCS: CommitmentScheme> CommittedPolynomial<'a, F, PCS> {
    pub fn polynomial(&self) -> &DensePolynomial<'a, F> {
        self.polynomial
            .as_ref()
            .expect("polynomial Fr data was dropped (lazy); materialize it before use")
            .as_ref()
    }

    pub(crate) fn chunked_polys(&self) -> Vec<DensePolynomial<'_, F>> {
        self.polynomial
            .as_ref()
            .expect("polynomial Fr data was dropped (lazy); materialize it before use")
            .as_view_chunks(self.num_chunks())
    }

    pub(crate) fn batch_commit(
        prover_params: &PCS::ProverSetup,
        polys: Vec<DensePolynomial<'a, F>>,
    ) -> anyhow::Result<Vec<Self>>
    where
        PCS: CommitmentScheme<Field = F>,
    {
        let num_chunks_per_poly = polys
            .iter()
            .map(|poly| num_chunks_for_polynomial(poly.num_vars()))
            .collect_vec();
        let chunked_mles = polys
            .iter()
            .zip(&num_chunks_per_poly)
            .flat_map(|(poly, &num_chunks)| poly.as_view_chunks(num_chunks))
            .collect_vec();
        let mut commitments_iter = PCS::batch_commit(prover_params, &chunked_mles)?.into_iter();
        let num_vars_per_poly = polys.iter().map(|poly| poly.num_vars()).collect_vec();
        Ok(polys
            .into_iter()
            .zip(num_chunks_per_poly)
            .zip(num_vars_per_poly)
            .map(|((poly, num_chunks), poly_num_vars)| {
                let comms = (0..num_chunks)
                    .map_while(|_| commitments_iter.next().map(|(comm, _)| comm))
                    .collect();
                CommittedPolynomial {
                    chunk_commitments: comms,
                    poly_num_vars,
                    polynomial: Some(poly.into()),
                }
            })
            .collect())
    }
}

impl<'a, F: PrimeField, PCS: CommitmentScheme> AppendToTranscript
    for CommittedPolynomial<'a, F, PCS>
{
    fn append_to_transcript<ProofTranscript: Transcript>(&self, transcript: &mut ProofTranscript) {
        self.chunk_commitments
            .iter()
            .for_each(|comm| comm.append_to_transcript(transcript))
    }
}

// This is the threshold we employ to split committed polynomials into multiple chunks;
// any polynomial with more variables than this is split into chunks.
//
// The chunk size (2^threshold field elements) must always be covered by the PCS
// setup: g1_powers are built with at least this many powers (see
// build_global_context_data). Larger thresholds mean fewer chunks per polynomial
// (less per-chunk overhead: transcript appends, opening batching) at the price of
// a larger prover setup. Configurable via ZKML_CHUNK_THRESHOLD_LOG (log2 of the
// chunk size), default 22 (16 MiB of field elements per chunk).
const DEFAULT_CHUNK_SIZE_THRESHOLD: usize = 22;

lazy_static! {
    pub(crate) static ref CHUNK_SIZE_THRESHOLD: RwLock<Option<usize>> = RwLock::new(
        std::env::var("ZKML_CHUNK_THRESHOLD_LOG")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    );
}

/// The effective log2 of the commitment chunk size (env-overridable, default 23).
pub(crate) fn chunk_size_threshold_log() -> usize {
    CHUNK_SIZE_THRESHOLD
        .read_recursive()
        .unwrap_or(DEFAULT_CHUNK_SIZE_THRESHOLD)
}
// Sanity bound against runaway chunk counts only. It must NEVER limit the chunk
// count below what keeps every chunk at the chunk-size threshold: capping the
// count makes each chunk grow past the threshold and the msm then indexes
// g1_powers out of range (seen as `range end index 2^23 out of range for slice
// of length 2^21+1` on gemma-270m s=4096 with the old value 128).
const MAX_CHUNKS: usize = 1 << 16;

/// Compute the number of chunks a polynomial with `poly_num_vars` variables is split into
/// in order to be committed
fn num_chunks_for_polynomial(poly_num_vars: usize) -> usize {
    let chunk_size_threshold = CHUNK_SIZE_THRESHOLD
        .read_recursive()
        .unwrap_or(DEFAULT_CHUNK_SIZE_THRESHOLD);
    let num_chunks = (1usize << poly_num_vars).div_ceil(1usize << chunk_size_threshold);
    // sanity cap only (see note above); never caps below the threshold-sized chunks
    num_chunks.min(MAX_CHUNKS)
}

#[derive(Debug, Serialize, Deserialize)]
/// Struct that contains all the data needed for proving/verifying commitments relating to a model.
pub struct GlobalCommitmentContext<'a, F, PCS>
where
    F: PrimeField,
    PCS: CommitmentScheme,
{
    /// Prover parameters for the [`PolynomialCommitmentScheme`]
    prover_params: PCS::ProverSetup,
    /// Verifier parameters for the [`PolynomialCommitmentScheme`]
    verifier_params: PCS::VerifierSetup,
    /// Commitment to the model static polynomials.
    model_commitments: BTreeMap<CommitmentId, CommittedPolynomial<'a, F, PCS>>,
    /// This is the [`NodeId`] used for tables in this model
    table_node_id: NodeId,
    /// This is the largest number of variables of any of the polynomials in `model_commitment`
    max_model_num_vars: usize,
    /// Integer (i64) sources for lazily-committed polynomials: large model
    /// weights are Fr-ized, committed, and their Fr data dropped to keep the
    /// context residency bounded; the Fr data is re-derived bit-exactly from
    /// these sources when a claim needs it (chunk-end RLC/opening).
    /// Serialized as the plain i64 data (1/4 of the old Fr dump) so that
    /// `--save-params`/`--load-params` keep working with lazy commitments.
    #[serde(with = "arc_vec_map_serde", default)]
    lazy_sources: HashMap<CommitmentId, std::sync::Arc<Vec<Element>>>,
}

/// Polynomials with at least this many variables are committed lazily (their Fr
/// data is dropped after the MSM and re-derived on demand). Below the threshold
/// the Fr data is small enough to stay resident (existing behavior).
const LAZY_COMMIT_MIN_VARS: usize = 20;

/// Cap on the total number of field elements Fr-ized at once during the
/// streaming commit — bounds the context-generation memory peak to a few GB
/// regardless of model size.
const COMMIT_GROUP_ELEMS: usize = 1 << 24;

impl<'a, F, PCS> GlobalCommitmentContext<'a, F, PCS>
where
    PCS: CommitmentScheme<Field = F>,
    F: PrimeField,
{
    /// Make a new [`GlobalCommitmentContext`].
    ///
    /// `polys` carries the integer (i64) evaluation sources of the model's
    /// static polynomials; each is Fr-ized, committed via MSM and — when large
    /// enough — dropped in favor of the registered source (lazy commitment).
    pub fn new<R: Rng>(
        witness_poly_num_vars: usize,
        polys: HashMap<CommitmentId, Vec<Element>>,
        lookup_ctx: &[&Table],
        max_node_id: NodeId,
        rng: &mut R,
    ) -> Result<GlobalCommitmentContext<'a, F, PCS>> {
        let num_vars_of = |evals: &Vec<Element>| -> usize {
            assert!(
                evals.len().is_power_of_two(),
                "committed poly length must be a power of two"
            );
            evals.len().trailing_zeros() as usize
        };
        // Find the maximum size so we can generate params
        let max_num_vars = polys.iter().fold(witness_poly_num_vars, |acc, (_, evals)| {
            acc.max(num_vars_of(evals))
        });

        let pcs_max_num_vars =
            num_vars_for_chunk(max_num_vars, num_chunks_for_polynomial(max_num_vars));

        debug!("Building PCS params for {pcs_max_num_vars} max variables...");
        let m = Metrics::new();
        let (prover_params, verifier_params) = PCS::test_setup(rng, pcs_max_num_vars);
        debug!("{} PPs & VPs built", m.to_span());

        // Find the maximum node id used in this model so we can pick a unique node id for table related commitments.
        let table_node_id = NodeId::from(max_node_id.0 + 1);

        // First we take all the model polys and sort them by the number of variables they have.
        // Then we do the same for any table commitments but here we set all of them to have `table_node_id`.
        let table_commitments_check = lookup_ctx.iter().any(|table| table.commit_output_column());
        let mut lazy_sources: HashMap<CommitmentId, std::sync::Arc<Vec<Element>>> = HashMap::new();
        let mut model_commitments: BTreeMap<CommitmentId, CommittedPolynomial<'a, F, PCS>> =
            BTreeMap::new();
        if !polys.is_empty() || table_commitments_check {
            // Streaming commit: Fr-ize and MSM one bounded group at a time so the
            // context-generation peak stays flat regardless of model size; large
            // polys drop their Fr data afterwards (lazy re-derivation from the
            // registered i64 source keeps the commitment bit-exact).
            let m = Metrics::new();
            let mut group: Vec<(CommitmentId, DensePolynomial<'a, F>)> = Vec::new();
            let mut group_elems = 0usize;
            let flush = |group: Vec<(CommitmentId, DensePolynomial<'a, F>)>,
                             model_commitments: &mut BTreeMap<
                CommitmentId,
                CommittedPolynomial<'a, F, PCS>,
            >,
                             lazy_sources: &HashMap<
                CommitmentId,
                std::sync::Arc<Vec<Element>>,
            >|
             -> Result<()> {
                if group.is_empty() {
                    return Ok(());
                }
                let (ids, polys): (Vec<_>, Vec<_>) = group.into_iter().unzip();
                let committed = CommittedPolynomial::batch_commit(&prover_params, polys)
                    .context("Model commitment for poly {poly_id}")?;
                for (poly_id, commitment) in ids.into_iter().zip(committed) {
                    let lazy = commitment.poly_num_vars >= LAZY_COMMIT_MIN_VARS
                        && lazy_sources.contains_key(&poly_id);
                    let mut commitment = commitment;
                    if lazy {
                        commitment.polynomial = None;
                    }
                    model_commitments.insert(poly_id, commitment);
                }
                Ok(())
            };

            for (poly_id, evals) in polys {
                let evals = std::sync::Arc::new(evals);
                let num_vars = num_vars_of(&evals);
                let fr = DensePolynomial::new(crate::to_field::<Element, F, _>(evals.as_slice()));
                lazy_sources.insert(poly_id.clone(), evals);
                group.push((poly_id, fr));
                group_elems += 1usize << num_vars;
                if group_elems >= COMMIT_GROUP_ELEMS {
                    flush(
                        std::mem::take(&mut group),
                        &mut model_commitments,
                        &lazy_sources,
                    )?;
                    group_elems = 0;
                }
            }
            // Table committed columns (Fr, no i64 source) — always resident.
            for table in lookup_ctx {
                if let Some(mle) = table.committed_columns::<F>() {
                    group.push((table_poly_id(table.name()), mle));
                }
            }
            flush(group, &mut model_commitments, &lazy_sources)?;
            debug!("{} model commitments built", m.to_span());
        }

        let max_model_num_vars = model_commitments
            .values()
            .map(|poly| poly.poly_num_vars)
            .max()
            .unwrap_or(0usize);
        Ok(GlobalCommitmentContext {
            verifier_params,
            prover_params,
            model_commitments,
            table_node_id,
            max_model_num_vars,
            lazy_sources,
        })
    }

    /// Generate a prover/verifier context for the `witness_poly_size` specified as input;
    /// `witness_poly_size` represents the size of the biggest witness polynomial to be
    /// committed to. If no `witness_poly_size` is provided as input, this method generates
    /// the prover/verifier context for `self.max_poly_size()`
    #[allow(dead_code)]
    pub(crate) fn generate_contexts(
        self,
    ) -> Result<(CommitmentProverCtx<'a, F, PCS>, CommitmentVerifierCtx<PCS>)> {
        let GlobalCommitmentContext {
            prover_params,
            verifier_params,
            model_commitments,
            table_node_id,
            max_model_num_vars,
            lazy_sources,
        } = self;

        let verifier_ctx = CommitmentVerifierCtx {
            verifier_params,
            model_commitments: model_commitments
                .iter()
                .map(|(poly_id, committed_poly)| {
                    (poly_id.clone(), VerifierCommitment::from(committed_poly))
                })
                .collect(),
            table_node_id,
            max_model_num_vars,
        };

        let prover_ctx = CommitmentProverCtx {
            prover_params,
            model_commitments,
            table_node_id,
            max_model_num_vars,
            lazy_sources,
        };

        Ok((prover_ctx, verifier_ctx))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(
    serialize = "F: ark_serialize::CanonicalSerialize",
    deserialize = "F: ark_serialize::CanonicalDeserialize"
))]
/// Context data for the commitment prover
pub struct CommitmentProverCtx<'a, F, PCS>
where
    F: PrimeField,
    PCS: CommitmentScheme,
{
    /// Prover parameters for the [`PolynomialCommitmentScheme`]
    pub(super) prover_params: PCS::ProverSetup,
    /// Commitment to the model static polynomials.
    pub(super) model_commitments: BTreeMap<CommitmentId, CommittedPolynomial<'a, F, PCS>>,
    /// This is the [`NodeId`] used for tables in this model
    table_node_id: NodeId,
    /// This is the largest number of variables of any of the polynomials in `model_commitment`
    pub(super) max_model_num_vars: usize,
    /// Integer sources for lazily-committed polynomials (see
    /// [`GlobalCommitmentContext`]); serialized as plain i64 data.
    #[serde(with = "arc_vec_map_serde", default)]
    pub(super) lazy_sources: HashMap<CommitmentId, std::sync::Arc<Vec<Element>>>,
}

impl<'a, F, PCS> CommitmentProverCtx<'a, F, PCS>
where
    F: PrimeField,
    PCS: CommitmentScheme<Field = F>,
{
    /// Re-derive the Fr data of the lazily-committed polynomials among `ids`
    /// from their registered integer sources. The re-derivation runs the exact
    /// conversion used at commitment time, so the result is bit-identical to
    /// what the MSM consumed. Drop the returned map after use (the whole point
    /// is to not keep large weight Fr data resident).
    pub(crate) fn materialize_lazy<'id>(
        &self,
        ids: impl Iterator<Item = &'id CommitmentId>,
    ) -> Result<HashMap<CommitmentId, ArcMultilinearExtension<'a, F>>> {
        let mut out = HashMap::new();
        for id in ids {
            let Some(committed) = self.model_commitments.get(id) else {
                continue;
            };
            if committed.polynomial.is_some() {
                continue;
            }
            let source = self.lazy_sources.get(id).ok_or_else(|| {
                anyhow::anyhow!("lazy poly {id} has no registered i64 source")
            })?;
            let mle = DensePolynomial::new(crate::to_field::<Element, F, _>(source.as_slice()));
            debug_assert_eq!(mle.num_vars(), committed.poly_num_vars);
            out.insert(id.clone(), mle.into());
        }
        Ok(out)
    }
}

impl<'a, F: PrimeField, PCS: CommitmentScheme<Field = F>> CommitmentProverCtx<'a, F, PCS> {
    /// Helper method to commit to polynomial.
    pub fn commit(
        &self,
        poly: DensePolynomial<'a, F>,
    ) -> anyhow::Result<CommittedPolynomial<'a, F, PCS>> {
        let poly_num_vars = poly.num_vars();
        let num_chunks = num_chunks_for_polynomial(poly_num_vars);
        if num_chunks == 1 {
            PCS::commit(&self.prover_params, &poly).map(|(commitment, _)| CommittedPolynomial {
                chunk_commitments: vec![commitment],
                poly_num_vars,
                polynomial: Some(poly.into()),
            })
        } else {
            let chunked_poly = poly.as_view_chunks(num_chunks);
            PCS::batch_commit(&self.prover_params, &chunked_poly).map(|chunk_commitments| {
                CommittedPolynomial {
                    chunk_commitments: chunk_commitments
                        .into_iter()
                        .map(|(comm, _)| comm)
                        .collect(),
                    poly_num_vars,
                    polynomial: Some(poly.into()),
                }
            })
        }
    }

    /// Helper method to commit to a set of polynomials.
    /// It returns one commitment for each polynomial provided as input
    pub fn batch_commit<'b>(
        &self,
        polys: Vec<DensePolynomial<'b, F>>,
    ) -> anyhow::Result<Vec<CommittedPolynomial<'b, F, PCS>>> {
        CommittedPolynomial::batch_commit(&self.prover_params, polys)
    }

    /// Write the commitment context to the transcript
    pub fn write_to_transcript<T: Transcript>(&self, transcript: &mut T) {
        self.model_commitments
            .iter()
            .for_each(|(_, committed_poly)| {
                committed_poly
                    .chunk_commitments
                    .iter()
                    .for_each(|comm| comm.append_to_transcript(transcript))
            })
    }

    pub fn table_node_id(&self) -> NodeId {
        self.table_node_id
    }
}

/// Context data for the commitment verifier
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CommitmentVerifierCtx<PCS>
where
    PCS: CommitmentScheme,
{
    /// Verifier parameters for the [`PolynomialCommitmentScheme`]
    pub(super) verifier_params: PCS::VerifierSetup,
    /// Commitment to the model static polynomials.
    pub(super) model_commitments: BTreeMap<CommitmentId, VerifierCommitment<PCS>>,
    /// This is the [`NodeId`] used for tables in this model
    table_node_id: NodeId,
    /// This is the largest number of variables of any of the polynomials in `model_commitment`
    max_model_num_vars: usize,
}

impl<PCS> CommitmentVerifierCtx<PCS>
where
    PCS: CommitmentScheme,
{
    pub fn write_to_transcript<T: Transcript>(&self, transcript: &mut T) {
        self.model_commitments
            .iter()
            .for_each(|(_, commitment)| commitment.append_to_transcript(transcript))
    }

    pub fn table_node_id(&self) -> NodeId {
        self.table_node_id
    }
}

/// serde bridge for `HashMap<CommitmentId, Arc<Vec<i64>>>` (serde's `rc`
/// feature is not enabled; the map is serialized as owned i64 vectors —
/// the natural on-disk format for lazy sources and 1/4 of the old Fr dump).
mod arc_vec_map_serde {
    use super::Element;
    use crate::tensor::CommitmentId;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{collections::HashMap, sync::Arc};

    pub fn serialize<S: Serializer>(
        map: &HashMap<CommitmentId, Arc<Vec<Element>>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let owned: HashMap<&CommitmentId, &Vec<Element>> =
            map.iter().map(|(k, v)| (k, &**v)).collect();
        owned.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<CommitmentId, Arc<Vec<Element>>>, D::Error> {
        let owned = HashMap::<CommitmentId, Vec<Element>>::deserialize(deserializer)?;
        Ok(owned.into_iter().map(|(k, v)| (k, Arc::new(v))).collect())
    }
}
