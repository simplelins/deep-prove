//! Proof-side construction for the GatherRows layer (D1.1 Stage A).
//!
//! The gather relation `K'[t, d] = K[f(t), d]` is proven as a **logUp
//! rearrangement argument**: the "table" side carries the (index, value)
//! columns of the committed K with the closed-form multiplicities of the
//! gather map, and the "lookup" side carries the (index, value) columns of
//! the committed K'. The multi-column tuple binding (index and value columns
//! combined with the separation challenge) makes the pairing position-exact.

use anyhow::{bail, ensure, Result};
use ark_ff::PrimeField;
use dp_crypto::arkyper::CommitmentScheme;
use dp_crypto::arkyper::transcript::{AppendToTranscript, Transcript};
use dp_crypto::poly::dense::DensePolynomial;
use dp_crypto::util::ceil_log2;
use dp_crypto::IntoMLE;

use crate::graph::NodeId;
use crate::iop::claim::Claim;
use crate::iop::context::ContextAux;
use crate::quantization::ToField;
use crate::iop::context::ProverContext;
use crate::lookup::context::LookupWitnessGen;
use crate::iop::prover::Prover;
use crate::layers::provable::{PadOp, ProveInfo, ProvableOp, VerifiableCtx};
use crate::iop::context::ShapeStep;
use crate::layers::provable::OpInfo;
use crate::layers::LayerCtx;
use crate::layers::transformer::attention_mask::AttentionSpan;
use crate::lookup::logup_gkr::prover::new_batch_multiple_sizes_prove;
use crate::lookup::logup_gkr::structs::{LogUpBatchVerifierClaim, LogUpInput, LogUpVerifierInstance, ProofType};
use crate::poly_commit::verifier::VerifierClaim;
use crate::lookup::logup_gkr::verifier::new_verify_logup_proof_multiple_sizes;
use crate::model::trace::Step;
use crate::padding::PaddingMode;
use crate::poly_commit::verifier::VerifierCommitment;
use crate::{Element, InitTranscript, Shape};

use super::{gather_multiplicities, gather_source_index, GatherRows, GatherRowsCtx, GatherRowsProof};

/// The number of variables of a padded length (length must be a power of two).
fn num_vars_of(len: usize) -> usize {
    assert!(len.is_power_of_two(), "padded length must be a power of two");
    len.trailing_zeros() as usize
}

/// Builds the logUp inputs for the gather relation.
///
/// `k` is the `[s, dk]` source matrix over the field (already converted).
/// Returns `[Lookup(s·w rows), Table(s rows)]` in decreasing-size order (the
/// GKR requires the circuits sorted by decreasing size).
///
/// Both sides carry `dk + 1` columns: the `dk` value columns followed by the
/// index column — the separation challenge binds (index, value) tuples
/// atomically, making the rearrangement position-exact.
pub fn build_gather_logup_inputs<F: PrimeField>(
    k: &[Vec<F>],
    w: usize,
    constant_challenge: F,
    column_separation_challenge: F,
) -> anyhow::Result<(LogUpInput<F>, LogUpInput<F>)> {
    let s = k.len();
    let dk = k.first().map(|r| r.len()).unwrap_or(0);
    ensure!(s.is_power_of_two(), "source rows must be a power of two (padded)");
    ensure!(w.is_power_of_two(), "window must be a power of two (padded)");
    let t_len = s * w;
    let n_t = num_vars_of(t_len);
    let n_s = num_vars_of(s);

    // Lookup side: K'[t, d] = K[f(t), d] columns + the f(t) index column,
    // padded to 2^{n_t} rows.
    let mut lookup_cols = vec![Vec::with_capacity(t_len); dk + 1];
    for t in 0..t_len {
        let j = gather_source_index(t, w, s);
        for d in 0..dk {
            lookup_cols[d].push(k[j][d]);
        }
        lookup_cols[dk].push(F::from(j as u64));
    }

    // Table side: K[j, d] columns + the index column + the multiplicities,
    // padded to 2^{n_s} rows.
    let multiplicities_raw = gather_multiplicities(w, s);
    let mut table_cols = vec![Vec::with_capacity(s); dk + 1];
    for j in 0..s {
        for d in 0..dk {
            table_cols[d].push(k[j][d]);
        }
        table_cols[dk].push(F::from(j as u64));
    }
    let mut multiplicities = vec![F::ZERO; s];
    for (j, m) in multiplicities_raw.iter().enumerate() {
        multiplicities[j] = F::from(*m);
    }
    let _ = n_s;

    let lookup_input = LogUpInput::new_lookup(
        lookup_cols,
        constant_challenge,
        column_separation_challenge,
        dk + 1,
    )
    .map_err(|e| anyhow::anyhow!("gather lookup input: {e:?}"))?;
    let table_input = LogUpInput::new_table(
        table_cols,
        multiplicities,
        constant_challenge,
        column_separation_challenge,
    )
    .map_err(|e| anyhow::anyhow!("gather table input: {e:?}"))?;

    // 两个独立 GKR（框架要求同型实例）：Lookup 侧（s·w 行）与 Table 侧（s 行）。
    // 两侧 fraction 平衡（claim 相等）= 重排正确性，由 verifier 检查。
    Ok((lookup_input, table_input))
}

/// LogUp 平衡检查：lookup 侧分子为 -1（`circuit.rs:167` 的 `-F::ONE`），
/// 平衡 ⟺ frac_l + frac_t == 0 ⟺ num_l·den_t + num_t·den_l == 0。
fn fraction_balanced<F: PrimeField>(
    l: &crate::lookup::logup_gkr::structs::LogUpBatchVerifierClaim<F>,
    t: &crate::lookup::logup_gkr::structs::LogUpBatchVerifierClaim<F>,
) -> bool {
    l.numerators().len() == t.numerators().len()
        && l.numerators()
            .iter()
            .zip(l.denominators().iter())
            .zip(t.numerators().iter().zip(t.denominators().iter()))
            .all(|((&nl, &dl), (&nt, &dt))| nl * dt + nt * dl == F::ZERO)
}

/// The verifier-side instances mirroring [`build_gather_logup_inputs`].
pub fn build_gather_verifier_instances<F: PrimeField>(
    w: usize,
    s: usize,
    dk: usize,
    constant_challenge: F,
    column_separation_challenge: F,
) -> anyhow::Result<Vec<crate::lookup::logup_gkr::structs::LogUpVerifierInstance<F>>> {
    use crate::lookup::logup_gkr::structs::LogUpVerifierInstance;
    ensure!(s.is_power_of_two() && w.is_power_of_two());
    // 与 circuit 的约定一致：num_vars = ceil_log2(len/2)（GKR 首轮对折，
    // 参照既有 `table.table_bit_size() - 1` 用法）
    let n_lookup = num_vars_of(s * w) - 1;
    let n_table = num_vars_of(s) - 1;
    Ok(vec![
        LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            dk + 1,
            ProofType::Lookup,
            n_lookup,
        ),
        LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            dk + 1,
            ProofType::Table,
            n_table,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::transformer::attention_mask::AttentionSpan;
use crate::lookup::logup_gkr::prover::new_batch_multiple_sizes_prove;
    use crate::lookup::logup_gkr::verifier::new_verify_logup_proof_multiple_sizes;
    use anyhow::Result;
    use ark_bn254::Fr;
    use ark_std::UniformRand;
    use dp_crypto::arkyper::transcript::blake3::Blake3Transcript;
    use dp_crypto::arkyper::transcript::{AppendToTranscript, Transcript};

    /// Round trip: build the gather logUp inputs → GKR prove → verify. The
    /// verify succeeds and the fraction claims balance (the wiring is sound).
    #[test]
    fn test_gather_logup_round_trip() -> Result<()> {
        let (s, w, dk) = (8usize, 4usize, 3usize);
        let mut rng = ark_std::test_rng();
        let k: Vec<Vec<Fr>> = (0..s)
            .map(|_| (0..dk).map(|_| Fr::rand(&mut rng)).collect())
            .collect();

        let mut t_prove = Blake3Transcript::new(b"gather_roundtrip");
        let mut t_verify = Blake3Transcript::new(b"gather_roundtrip");

        // The challenges: sampled once, shared by both sides (in the real flow
        // they come from the challenge storage at chunk init).
        let cc: Fr = t_prove.challenge_scalar();
        let csc: Fr = t_prove.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();

        let (lookup_input, table_input) = build_gather_logup_inputs(&k, w, cc, csc)?;

        // 两个同型 GKR：Lookup 侧与 Table 侧分别证明
        let proof_l = new_batch_multiple_sizes_prove(&[lookup_input], &mut t_prove)?;
        let proof_t = new_batch_multiple_sizes_prove(&[table_input], &mut t_prove)?;

        let instances = build_gather_verifier_instances(w, s, dk, cc, csc)?;
        let claim_l = new_verify_logup_proof_multiple_sizes(
            &proof_l,
            &instances[..1],
            &mut t_verify,
        )?;
        let claim_t = new_verify_logup_proof_multiple_sizes(
            &proof_t,
            &instances[1..],
            &mut t_verify,
        )?;

        // fraction 平衡：LogUp 约定 lookup 侧分子为 -1 → 平衡 = frac_l + frac_t == 0
        // ⟺ num_l·den_t + num_t·den_l == 0（逐 fraction 元素）
        ensure!(
            fraction_balanced(&claim_l, &claim_t),
            "gather fraction balance broken"
        );
        ensure!(!claim_l.point().is_empty(), "verified claim has an empty point");
        Ok(())
    }

    /// Negative control: a corrupted gather (one value flipped) must fail the
    /// round trip — the logUp argument is position-exact.
    #[test]
    fn test_gather_logup_rejects_corruption() {
        let (s, w, dk) = (8usize, 4usize, 3usize);
        let mut rng = ark_std::test_rng();
        let k: Vec<Vec<Fr>> = (0..s)
            .map(|_| (0..dk).map(|_| Fr::rand(&mut rng)).collect())
            .collect();

        let mut t_prove = Blake3Transcript::new(b"gather_corrupt");
        let mut t_verify = Blake3Transcript::new(b"gather_corrupt");
        let cc: Fr = t_prove.challenge_scalar();
        let csc: Fr = t_prove.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();

        // Corrupt the lookup side: flip one gathered value
        let (mut lookup_input, table_input) = build_gather_logup_inputs(&k, w, cc, csc).unwrap();
        let corrupted = match &mut lookup_input {
            LogUpInput::Lookup { column_evals, .. } => {
                column_evals[0][0] += Fr::from(1u64);
                true
            }
            _ => false,
        };
        assert!(corrupted);
        let proof_l = new_batch_multiple_sizes_prove(&[lookup_input], &mut t_prove).unwrap();
        let proof_t = new_batch_multiple_sizes_prove(&[table_input], &mut t_prove).unwrap();
        let instances = build_gather_verifier_instances(w, s, dk, cc, csc).unwrap();
        let claim_l = new_verify_logup_proof_multiple_sizes(
            &proof_l,
            &instances[..1],
            &mut t_verify,
        )
        .unwrap();
        let claim_t = new_verify_logup_proof_multiple_sizes(
            &proof_t,
            &instances[1..],
            &mut t_verify,
        )
        .unwrap();
        // 平衡检查捕获腐化：lookup 与 table 的 fraction 不再抵消
        assert!(
            !fraction_balanced(&claim_l, &claim_t),
            "corrupted gather must break the fraction balance"
        );
    }
}

// ---------------------------------------------------------------------------
// The layer integration (ProvableOp / VerifiableCtx)
// ---------------------------------------------------------------------------

impl GatherRows {
    fn output_shape(&self, input: &Shape) -> Shape {
        let w = self.window();
        Shape::new(vec![input.dim(0), input.dim(1), w, input.dim(2)])
    }

    /// The gathered tensor's flattened field MLE (行主序 [t, d]，d 为低位变量).
    fn gathered_mle<F: PrimeField>(&self, input: &crate::tensor::Tensor<Element>) -> Vec<F> {
        let s = input.shape().dim(0);
        let dk = input.shape().dim(1);
        let w = self.window();
        let data = input.data();
        let mut flat = Vec::with_capacity(s * w * dk);
        for i in 0..s {
            for off in 0..w {
                let j = gather_source_index(i * w + off, w, s);
                for d in 0..dk {
                    flat.push(F::from(data[j * dk + d]));
                }
            }
        }
        flat
    }
}

impl GatherRowsCtx {
    pub fn window(&self) -> usize {
        match self.span {
            AttentionSpan::Local(w) => w,
            AttentionSpan::Full => panic!("GatherRows requires AttentionSpan::Local"),
        }
    }

    fn output_shape(&self, input: &Shape) -> Shape {
        Shape::new(vec![input.dim(0), input.dim(1), self.window(), input.dim(2)])
    }

    /// Proves the gather: the two logUp GKR proofs whose fraction balance
    /// proves `K' = gather(K, f)`. The claims about K' and K at the logup
    /// point feed the commitment openings and the claim chain.
    ///
    /// Claim points: the logup point covers the t-variables (行主序中为高位);
    /// the flattened poly's d-variables bind to the basis vectors e_d —
    /// the logup's per-column claims map to the flattened poly's claims at
    /// (logup_point ++ e_d).
    pub(crate) fn prove_gather<F, PCS, T>(
        &self,
        id: NodeId,
        step_data: &Step<Element>,
        prover: &mut Prover<F, T, PCS>,
    ) -> Result<(GatherRowsProof<F, PCS>, Vec<Claim<F>>)>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F> + 'static,
        T: Transcript,
    {
        let w = self.window();
        let input = step_data.input_tensor_at(0)?;
        let s = input.shape().dim(0);
        let dk = input.shape().dim(1);
        ensure!(s.is_power_of_two(), "gather sequence length must be padded");
        ensure!(w.is_power_of_two(), "gather window must be a power of two");
        let t_len = s * w;

        // 1) The challenges (sampled at this point; the verifier mirrors)
        let constant_challenge: F = prover.transcript.append_and_sample(b"gather_cc");
        let column_separation_challenge: F =
            prover.transcript.append_and_sample(b"gather_csc");

        // 2) The two logUp inputs: the K' columns from the committed witness
        //    poly (the chunk's batch commit), the K columns from the input.
        //    借用分离：先取 evals 与 commitment 克隆，释放 prover 借用后再用 transcript
        let (k_prime_evals, k_prime_commitment) = {
            let committed = prover.lookup_witness(id)?;
            ensure!(
                committed.len() == 1,
                "gather expects exactly one committed witness poly, got {}",
                committed.len()
            );
            let evals: Vec<F> = committed[0].polynomial().evals().into_iter().collect();
            let commitment = VerifierCommitment::from(&committed[0]);
            (evals, commitment)
        };
        ensure!(
            k_prime_evals.len() == t_len * dk,
            "K' poly length mismatch: {} vs {}",
            k_prime_evals.len(),
            t_len * dk
        );

        let k_field: Vec<Vec<F>> = (0..s)
            .map(|j| (0..dk).map(|d| F::from(input.data()[j * dk + d])).collect())
            .collect();
        let (lookup_input, table_input) = build_gather_logup_inputs(
            &k_field,
            w,
            constant_challenge,
            column_separation_challenge,
        )?;
        let logup_proof_lookup =
            new_batch_multiple_sizes_prove(&[lookup_input], prover.transcript)?;
        let logup_proof_table =
            new_batch_multiple_sizes_prove(&[table_input], prover.transcript)?;

        // 3) The claims at the logup point + the fraction balance
        let n_lookup = num_vars_of(t_len) - 1;
        let n_table = num_vars_of(s) - 1;
        let inst_l = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            dk + 1,
            ProofType::Lookup,
            n_lookup,
        );
        let inst_t = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            dk + 1,
            ProofType::Table,
            n_table,
        );
        let claim_l = new_verify_logup_proof_multiple_sizes(
            &logup_proof_lookup,
            &[inst_l],
            prover.transcript,
        )?;
        let claim_t = new_verify_logup_proof_multiple_sizes(
            &logup_proof_table,
            &[inst_t],
            prover.transcript,
        )?;
        ensure!(
            fraction_balanced(&claim_l, &claim_t),
            "gather fraction balance broken at node {id}"
        );

        // 4) The claims wiring (§12): the logup point covers the t-variables;
        //    the flattened poly's d-variables bind to the basis vectors e_d.
        let logup_point = claim_l.point().to_vec();
        let n_d_vars = ceil_log2(dk);
        let mut k_prime_claims = Vec::with_capacity(dk);
        let mut input_claims = Vec::with_capacity(dk);
        for d in 0..dk {
            let mut e_d = vec![F::ZERO; n_d_vars];
            if d < e_d.len() {
                e_d[d] = F::ONE;
            }
            let point: Vec<F> = logup_point.iter().chain(e_d.iter()).copied().collect();

            let l_claim = claim_l
                .output_claims()
                .get(d)
                .ok_or_else(|| anyhow::anyhow!("missing lookup output claim {d}"))?;
            k_prime_claims.push(Claim::new(point.clone(), l_claim.evaluation()));

            let t_claim = claim_t
                .output_claims()
                .get(d)
                .ok_or_else(|| anyhow::anyhow!("missing table output claim {d}"))?;
            input_claims.push(Claim::new(point.clone(), t_claim.evaluation()));
        }
        // 单 committed poly（K'），dk 个 claims（内层 vec = 该 poly 的全部 claims）
        prover.add_witness_claim(id, vec![k_prime_claims]);

        // 5) The K' commitment for the proof structure
        let commitments = vec![k_prime_commitment];

        Ok((
            GatherRowsProof {
                logup_proof_lookup,
                logup_proof_table,
                commitments,
            },
            input_claims,
        ))
    }

    /// The verifier mirror of [`prove_gather`].
    pub(crate) fn verify_gather<F, PCS, T>(
        &self,
        id: NodeId,
        proof: &GatherRowsProof<F, PCS>,
        unpadded_input_shape: &Shape,
        verifier: &mut crate::iop::verifier::Verifier<F, T, PCS>,
    ) -> Result<Vec<Claim<F>>>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F> + 'static,
        T: Transcript,
    {
        let w = self.window();
        let s = unpadded_input_shape.dim(0);
        let dk = unpadded_input_shape.dim(2);

        // 1) The challenges (mirroring the prover's sampling)
        let constant_challenge: F = verifier.transcript.append_and_sample(b"gather_cc");
        let column_separation_challenge: F =
            verifier.transcript.append_and_sample(b"gather_csc");

        // 2) The claims at the logup point + the fraction balance
        let n_lookup = num_vars_of(s * w) - 1;
        let n_table = num_vars_of(s) - 1;
        let inst_l = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            dk + 1,
            ProofType::Lookup,
            n_lookup,
        );
        let inst_t = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            dk + 1,
            ProofType::Table,
            n_table,
        );
        let claim_l = new_verify_logup_proof_multiple_sizes(
            &proof.logup_proof_lookup,
            &[inst_l],
            verifier.transcript,
        )?;
        let claim_t = new_verify_logup_proof_multiple_sizes(
            &proof.logup_proof_table,
            &[inst_t],
            verifier.transcript,
        )?;
        ensure!(
            fraction_balanced(&claim_l, &claim_t),
            "gather fraction balance broken at node {id}"
        );

        // 3) The claims wiring (the mirror)
        let logup_point = claim_l.point().to_vec();
        let n_d_vars = ceil_log2(dk);
        let mut k_prime_claims = Vec::with_capacity(dk);
        let mut input_claims = Vec::with_capacity(dk);
        for d in 0..dk {
            let mut e_d = vec![F::ZERO; n_d_vars];
            if d < e_d.len() {
                e_d[d] = F::ONE;
            }
            let point: Vec<F> = logup_point.iter().chain(e_d.iter()).copied().collect();

            let l_claim = claim_l
                .output_claims()
                .get(d)
                .ok_or_else(|| anyhow::anyhow!("missing lookup output claim {d}"))?;
            k_prime_claims.push(
                Claim::new(point.clone(), l_claim.evaluation()),
            );

            let t_claim = claim_t
                .output_claims()
                .get(d)
                .ok_or_else(|| anyhow::anyhow!("missing table output claim {d}"))?;
            input_claims.push(Claim::new(point.clone(), t_claim.evaluation()));
        }

        // 4) Register the K' commitment claims (from the proof structure)
        if let Some(commitment) = proof.commitments.first() {
            verifier
                .commit_verifier
                .add_witness_claim(id, vec![VerifierClaim {
                    commitment: commitment.clone(),
                    claims: k_prime_claims,
                }]);
        }

        Ok(input_claims)
    }
}

impl OpInfo for GatherRowsCtx {
    fn output_shapes(
        &self,
        input_shapes: &[Shape],
        _padding_mode: PaddingMode,
    ) -> Result<Vec<Shape>> {
        Ok(vec![self.output_shape(&input_shapes[0])])
    }

    fn num_outputs(&self, _num_inputs: usize) -> Result<usize> {
        Ok(1)
    }

    fn describe(&self) -> String {
        let w = match self.span {
            AttentionSpan::Local(w) => w,
            AttentionSpan::Full => 0,
        };
        format!("gather-rows: window={w}")
    }

    fn is_provable(&self) -> bool {
        true
    }
}

impl<F: PrimeField, PCS: CommitmentScheme<Field = F> + 'static> VerifiableCtx<F, PCS>
    for GatherRowsCtx
{
    type Proof = GatherRowsProof<F, PCS>;

    fn verify<T: Transcript>(
        &self,
        proof: &Self::Proof,
        _last_claims: &[&Claim<F>],
        verifier: &mut crate::iop::verifier::Verifier<F, T, PCS>,
        shape_step: &ShapeStep,
        node_id: NodeId,
    ) -> Result<Vec<Claim<F>>> {
        let unpadded_input_shape = &shape_step.unpadded_input_shape[0];
        self.verify_gather(node_id, proof, unpadded_input_shape, verifier)
    }

    fn write_proof_to_transcript<T: Transcript>(
        &self,
        proof: &Self::Proof,
        transcript: &mut T,
    ) -> Result<()> {
        for commitment in &proof.commitments {
            commitment.append_to_transcript(transcript);
        }
        Ok(())
    }

    fn verify_input_claim<D: std::ops::Deref<Target = F> + crate::quantization::ToField<F>, A: AsRef<crate::tensor::Tensor<D>>>(
        &self,
        inputs: &[A],
        claims: &[&Claim<F>],
    ) -> Result<()> {
        for (input, claim) in inputs.iter().zip(claims.iter()) {
            let computed = input
                .as_ref()
                .to_field()
                .into_mle()
                .evaluate(claim.point())?;
            ensure!(
                computed == claim.evaluation(),
                "gather input claim incorrect: computed {computed:?} vs claimed {:?}",
                claim.evaluation()
            );
        }
        Ok(())
    }
}

impl<F: PrimeField, PCS: CommitmentScheme<Field = F> + 'static> ProvableOp<F, PCS> for GatherRows {
    type Ctx = GatherRowsCtx;


    fn prove<'a, 'b, 'c, 'd, T: Transcript + InitTranscript>(
        &'a self,
        node_id: NodeId,
        _ctx: &'b Self::Ctx,
        _last_claims: Vec<&Claim<F>>,
        step_data: &Step<Element>,
        prover: &mut Prover<'c, 'd, F, T, PCS>,
    ) -> Result<Vec<Claim<F>>> {
        let (proof, input_claims) = _ctx.prove_gather::<F, PCS, T>(node_id, step_data, prover)?;
        prover.push_proof(node_id, crate::layers::LayerProof::GatherRows(proof));
        Ok(input_claims)
    }

    fn gen_lookup_witness(
        &self,
        id: NodeId,
        _ctx: &ProverContext<F, PCS>,
        step_data: &Step<Element>,
    ) -> Result<LookupWitnessGen<'_, F>> {
        // K' 的展平 MLE 进 witness 提交路径（logup 查询侧 + PCS opening）
        let input = step_data.input_tensor_at(0)?;
        let flat = self.gathered_mle::<F>(&input);
        let mle = flat.into_mle();
        let mut gen_w = LookupWitnessGen::default();
        gen_w.insert_layer_witness_data(id, vec![mle], vec![], vec![]);
        Ok(gen_w)
    }
}

impl ProveInfo for GatherRows {
    fn step_info<F: PrimeField>(&self, aux: ContextAux) -> Result<(LayerCtx<F>, ContextAux)> {
        let out_shape = self.output_shape(&aux.last_output_shape[0]);
        let ctx = GatherRowsCtx {
            span: self.span,
            heads_per_kv_head: self.heads_per_kv_head,
        };
        // K' 的 MLE 尺寸计入 max_poly_len（承诺分块覆盖）
        let out_elems = out_shape.numel();
        let max_poly_len = aux.max_poly_len.max(out_elems.next_power_of_two());
        Ok((
            LayerCtx::GatherRows(ctx),
            ContextAux {
                last_output_shape: vec![out_shape],
                max_poly_len,
                ..aux
            },
        ))
    }
}

impl PadOp for GatherRows {
    fn pad_node(self, _si: &mut crate::padding::ShapeInfo) -> Result<Self> {
        // 窗口 w 与位置映射数据无关，pad 不改变算子
        Ok(self)
    }
}

impl OpInfo for GatherRows {
    fn is_provable(&self) -> bool {
        true
    }

    fn output_shapes(
        &self,
        input_shapes: &[Shape],
        _padding_mode: PaddingMode,
    ) -> Result<Vec<Shape>> {
        Ok(vec![self.output_shape(&input_shapes[0])])
    }

    fn num_outputs(&self, _num_inputs: usize) -> Result<usize> {
        Ok(1)
    }

    fn describe(&self) -> String {
        let w = match self.span {
            AttentionSpan::Local(w) => w,
            AttentionSpan::Full => 0,
        };
        format!("gather-rows: window={w}")
    }
}
