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

use super::{
    gather_multiplicities_rows, gather_source_row, score_window_multiplicities,
    score_window_source_cell, GatherMode, GatherRows, GatherRowsCtx, GatherRowsProof,
};

/// The number of variables of a padded length (length must be a power of two).
fn num_vars_of(len: usize) -> usize {
    assert!(len.is_power_of_two(), "padded length must be a power of two");
    len.trailing_zeros() as usize
}

/// Builds the logUp inputs for the gather relation.
///
/// `k` is the flattened `[heads·s, dk]` source matrix over the field (already
/// converted; row-major `[Hkv, s, dk]` view). The window map is head-relative
/// (`gather_source_row`), so `heads = 1` reproduces the plain single-head
/// gather. Returns `[Lookup(rows·w rows), Table(rows rows)]` in
/// decreasing-size order (the GKR requires the circuits sorted by decreasing
/// size).
///
/// Both sides carry `dk + 1` columns: the `dk` value columns followed by the
/// index column — the separation challenge binds (index, value) tuples
/// atomically, making the rearrangement position-exact.
pub fn build_gather_logup_inputs<F: PrimeField>(
    k: &[Vec<F>],
    w: usize,
    heads: usize,
    constant_challenge: F,
    column_separation_challenge: F,
) -> anyhow::Result<(LogUpInput<F>, LogUpInput<F>)> {
    let rows = k.len();
    ensure!(rows % heads == 0, "row count must be a multiple of heads");
    let s = rows / heads;
    let dk = k.first().map(|r| r.len()).unwrap_or(0);
    ensure!(rows.is_power_of_two(), "source rows must be a power of two (padded)");
    ensure!(w.is_power_of_two(), "window must be a power of two (padded)");
    let t_len = rows * w;
    let _ = num_vars_of(t_len);

    // Lookup side: K'[t, d] = K[gather_source_row(t), d] columns + the source
    // row index column, padded to 2^{n_t} rows.
    let mut lookup_cols = vec![Vec::with_capacity(t_len); dk + 1];
    for t in 0..t_len {
        let j = gather_source_row(t, w, s);
        for d in 0..dk {
            lookup_cols[d].push(k[j][d]);
        }
        lookup_cols[dk].push(F::from(j as u64));
    }

    // Table side: K[j, d] columns + the index column + the head-tiled
    // multiplicities, padded to 2^{n_table} rows.
    let multiplicities_raw = gather_multiplicities_rows(w, s, heads);
    let mut table_cols = vec![Vec::with_capacity(rows); dk + 1];
    for j in 0..rows {
        for d in 0..dk {
            table_cols[d].push(k[j][d]);
        }
        table_cols[dk].push(F::from(j as u64));
    }
    let mut multiplicities = vec![F::ZERO; rows];
    for (j, m) in multiplicities_raw.iter().enumerate() {
        multiplicities[j] = F::from(*m);
    }

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

    // 两个独立 GKR（框架要求同型实例）：Lookup 侧（rows·w 行）与 Table 侧
    // （rows 行）。两侧 fraction 平衡（claim 相等）= 重排正确性，由 verifier 检查。
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

/// ScoreWindow 的 logUp 输入：末轴窗口切片的重排论证（cell-id tuple 绑定）。
///
/// `scores_flat`：`[heads·q·s]` 行主序 cell 值（1 个 value 列）；`q`/`s` 为
/// 每个 head 的 query/key 轴长（padded）。表侧 = 全部 `[heads·q·s]` 个 cell
/// （value + cell-id 列）+ 闭式重数；查询侧 = `[heads·q·w]` 个输出位置
/// （复制的 value + 源 cell-id 列）。tuple = (cell-id, value) 与分离挑战
/// 原子绑定 → 位置精确。
pub fn build_score_window_logup_inputs<F: PrimeField>(
    scores_flat: &[F],
    w: usize,
    q: usize,
    s: usize,
    heads: usize,
    constant_challenge: F,
    column_separation_challenge: F,
) -> anyhow::Result<(LogUpInput<F>, LogUpInput<F>)> {
    ensure!(q.is_power_of_two() && s.is_power_of_two() && w.is_power_of_two());
    ensure!(q * s == scores_flat.len() / heads, "cell count mismatch");
    let cells = scores_flat.len();
    let t_len = heads * q * w;
    ensure!(cells.is_power_of_two() && t_len.is_power_of_two());

    // 查询侧：out[t] = in[cell(t)] 的 value 列 + 闭式 cell-id 列
    let mut lookup_cols = vec![Vec::with_capacity(t_len); 2];
    for t in 0..t_len {
        let c = score_window_source_cell(t, w, q, s);
        lookup_cols[0].push(scores_flat[c]);
        lookup_cols[1].push(F::from(c as u64));
    }

    // 表侧：全部 cell 的 value 列 + cell-id 列 + 闭式重数
    let multiplicities_raw = score_window_multiplicities(w, q, s, heads);
    let mut table_cols = vec![Vec::with_capacity(cells); 2];
    for c in 0..cells {
        table_cols[0].push(scores_flat[c]);
        table_cols[1].push(F::from(c as u64));
    }
    let multiplicities = multiplicities_raw
        .iter()
        .map(|m| F::from(*m))
        .collect::<Vec<F>>();

    let lookup_input = LogUpInput::new_lookup(
        lookup_cols,
        constant_challenge,
        column_separation_challenge,
        2,
    )
    .map_err(|e| anyhow::anyhow!("score window lookup input: {e:?}"))?;
    let table_input = LogUpInput::new_table(
        table_cols,
        multiplicities,
        constant_challenge,
        column_separation_challenge,
    )
    .map_err(|e| anyhow::anyhow!("score window table input: {e:?}"))?;
    Ok((lookup_input, table_input))
}

/// ScoreWindow 的 verifier 侧实例（镜像上面的输入构造）。
pub fn build_score_window_verifier_instances<F: PrimeField>(
    w: usize,
    q: usize,
    s: usize,
    heads: usize,
    constant_challenge: F,
    column_separation_challenge: F,
) -> anyhow::Result<Vec<crate::lookup::logup_gkr::structs::LogUpVerifierInstance<F>>> {
    use crate::lookup::logup_gkr::structs::LogUpVerifierInstance;
    ensure!(q.is_power_of_two() && s.is_power_of_two() && w.is_power_of_two());
    let n_lookup = num_vars_of(heads * q * w) - 1;
    let n_table = num_vars_of(heads * q * s) - 1;
    Ok(vec![
        LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            2,
            ProofType::Lookup,
            n_lookup,
        ),
        LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            2,
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

    /// ScoreWindow round trip：cell-id tuple 的 logUp 重排 prove → verify，
    /// fraction 平衡 + index 列闭式对照成立。
    #[test]
    fn test_score_window_logup_round_trip() -> Result<()> {
        let (q, s, w, heads) = (8usize, 8usize, 4usize, 2usize);
        let cells = heads * q * s;
        let mut rng = ark_std::test_rng();
        let scores: Vec<Fr> = (0..cells).map(|_| Fr::rand(&mut rng)).collect();

        let mut t_prove = Blake3Transcript::new(b"sw_roundtrip");
        let mut t_verify = Blake3Transcript::new(b"sw_roundtrip");
        let cc: Fr = t_prove.challenge_scalar();
        let csc: Fr = t_prove.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();

        let (lookup_input, table_input) =
            build_score_window_logup_inputs(&scores, w, q, s, heads, cc, csc)?;
        let proof_l = new_batch_multiple_sizes_prove(&[lookup_input], &mut t_prove)?;
        let proof_t = new_batch_multiple_sizes_prove(&[table_input], &mut t_prove)?;

        let instances = build_score_window_verifier_instances(w, q, s, heads, cc, csc)?;
        let claim_l =
            new_verify_logup_proof_multiple_sizes(&proof_l, &instances[..1], &mut t_verify)?;
        let claim_t =
            new_verify_logup_proof_multiple_sizes(&proof_t, &instances[1..], &mut t_verify)?;

        ensure!(
            fraction_balanced(&claim_l, &claim_t),
            "score window fraction balance broken"
        );

        // index 列闭式对照（双侧；table = [mult, value, index]）
        let t_len = heads * q * w;
        let idx_claim = claim_l.output_claims().get(1).unwrap();
        let index_col: Vec<Fr> = (0..t_len)
            .map(|t| Fr::from(score_window_source_cell(t, w, q, s) as u64))
            .collect();
        let expected = index_col.into_mle().evaluate(idx_claim.point())?;
        ensure!(expected == idx_claim.evaluation(), "lookup index mismatch");
        let mult_claim = claim_t.output_claims().first().unwrap();
        let mult_col: Vec<Fr> = score_window_multiplicities(w, q, s, heads)
            .into_iter()
            .map(Fr::from)
            .collect();
        let expected_m = mult_col.into_mle().evaluate(mult_claim.point())?;
        ensure!(expected_m == mult_claim.evaluation(), "table multiplicity mismatch");
        let idx_claim_t = claim_t.output_claims().get(2).unwrap();
        let table_index: Vec<Fr> = (0..cells).map(|c| Fr::from(c as u64)).collect();
        let expected_t = table_index.into_mle().evaluate(idx_claim_t.point())?;
        ensure!(expected_t == idx_claim_t.evaluation(), "table index mismatch");
        Ok(())
    }

    /// ScoreWindow 负对照：腐化一个复制的 value，fraction 平衡破坏。
    #[test]
    fn test_score_window_rejects_corruption() {
        let (q, s, w, heads) = (8usize, 8usize, 4usize, 1usize);
        let cells = heads * q * s;
        let mut rng = ark_std::test_rng();
        let mut scores: Vec<Fr> = (0..cells).map(|_| Fr::rand(&mut rng)).collect();

        let mut t_prove = Blake3Transcript::new(b"sw_corrupt");
        let mut t_verify = Blake3Transcript::new(b"sw_corrupt");
        let cc: Fr = t_prove.challenge_scalar();
        let csc: Fr = t_prove.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();

        // 腐化查询侧：构建后翻转一个复制的 value（表侧保持原值）
        let (mut lookup_input, table_input) =
            build_score_window_logup_inputs(&scores, w, q, s, heads, cc, csc).unwrap();
        let corrupted = match &mut lookup_input {
            LogUpInput::Lookup { column_evals, .. } => {
                column_evals[0][5] += Fr::from(1u64);
                true
            }
            _ => false,
        };
        assert!(corrupted);
        let proof_l = new_batch_multiple_sizes_prove(&[lookup_input], &mut t_prove).unwrap();
        let proof_t = new_batch_multiple_sizes_prove(&[table_input], &mut t_prove).unwrap();
        let instances = build_score_window_verifier_instances(w, q, s, heads, cc, csc).unwrap();
        let claim_l =
            new_verify_logup_proof_multiple_sizes(&proof_l, &instances[..1], &mut t_verify)
                .unwrap();
        let claim_t =
            new_verify_logup_proof_multiple_sizes(&proof_t, &instances[1..], &mut t_verify)
                .unwrap();
        assert!(
            !fraction_balanced(&claim_l, &claim_t),
            "corrupted score window must break the fraction balance"
        );
    }

    /// 重数不变量：Σ mult = heads·q·w（每个输出 cell 恰好复制一个输入 cell）。
    #[test]
    fn test_score_window_multiplicity_invariant() {
        let (q, s, w, heads) = (16usize, 16usize, 4usize, 2usize);
        let mult = score_window_multiplicities(w, q, s, heads);
        assert_eq!(mult.len(), heads * q * s);
        let total: u64 = mult.iter().sum();
        assert_eq!(total as usize, heads * q * w);
    }

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

        let (lookup_input, table_input) = build_gather_logup_inputs(&k, w, 1, cc, csc)?;

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

    /// Multi-head round trip: the flattened `[Hkv·s, dk]` input with the
    /// head-relative window map. The lookup rows of head 1 must gather head
    /// 1's rows (not head 0's) — catches head-blind index maps.
    #[test]
    fn test_gather_logup_round_trip_multi_head() -> Result<()> {
        let (heads, s, w, dk) = (2usize, 8usize, 4usize, 3usize);
        let rows = heads * s;
        let mut rng = ark_std::test_rng();
        let k: Vec<Vec<Fr>> = (0..rows)
            .map(|_| (0..dk).map(|_| Fr::rand(&mut rng)).collect())
            .collect();

        let mut t_prove = Blake3Transcript::new(b"gather_mh");
        let mut t_verify = Blake3Transcript::new(b"gather_mh");
        let cc: Fr = t_prove.challenge_scalar();
        let csc: Fr = t_prove.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();
        let _: Fr = t_verify.challenge_scalar();

        let (lookup_input, table_input) = build_gather_logup_inputs(&k, w, heads, cc, csc)?;

        let proof_l = new_batch_multiple_sizes_prove(&[lookup_input], &mut t_prove)?;
        let proof_t = new_batch_multiple_sizes_prove(&[table_input], &mut t_prove)?;

        let instances = build_gather_verifier_instances(w, rows, dk, cc, csc)?;
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

        ensure!(
            fraction_balanced(&claim_l, &claim_t),
            "multi-head gather fraction balance broken"
        );

        // 直接校验 head 1 的首个输出行取自 head 1 的行（head 相对映射）
        let t_head1_first = s * w; // head 1 的 t=0（i=0, off=0）
        let j = gather_source_row(t_head1_first, w, s);
        assert!(
            (s..rows).contains(&j),
            "head 1 first output row must source within head 1, got row {j}"
        );
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
        let (mut lookup_input, table_input) = build_gather_logup_inputs(&k, w, 1, cc, csc).unwrap();
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

    /// The gathered tensor's flattened field MLE.
    /// - Rows 模式：行主序 `[Hkv, s, w, dk]`，dk 为低位变量；
    /// - ScoreWindow 模式：行主序 `[L, q, w]`（L = 前导轴折叠）。
    fn gathered_mle<F: PrimeField>(&self, input: &crate::tensor::Tensor<Element>) -> Result<Vec<F>> {
        let shape = input.shape();
        let data = input.data();
        let w = self.window();
        match self.mode {
            GatherMode::Rows => {
                ensure!(
                    shape.rank() == 3,
                    "gather input must be rank-3 [Hkv, s, dk], got rank {}",
                    shape.rank()
                );
                let heads = shape.dim(0);
                let s = shape.dim(1);
                let dk = shape.dim(2);
                let mut flat = Vec::with_capacity(heads * s * w * dk);
                for h in 0..heads {
                    for i in 0..s {
                        for off in 0..w {
                            let t = h * s * w + i * w + off;
                            let j = gather_source_row(t, w, s);
                            for d in 0..dk {
                                flat.push(F::from(data[j * dk + d]));
                            }
                        }
                    }
                }
                Ok(flat)
            }
            GatherMode::ScoreWindow => {
                ensure!(
                    shape.rank() == 3 || shape.rank() == 4,
                    "score window input must be rank-3/4, got rank {}",
                    shape.rank()
                );
                let (heads, q, s) = match shape.rank() {
                    3 => (shape.dim(0), shape.dim(1), shape.dim(2)),
                    _ => (shape.dim(0) * shape.dim(1), shape.dim(2), shape.dim(3)),
                };
                let mut flat = Vec::with_capacity(heads * q * w);
                for h in 0..heads {
                    for i in 0..q {
                        for off in 0..w {
                            let t = h * q * w + i * w + off;
                            let c = score_window_source_cell(t, w, q, s);
                            flat.push(F::from(data[c]));
                        }
                    }
                }
                Ok(flat)
            }
        }
    }
}

impl GatherRowsCtx {
    pub fn window(&self) -> usize {
        match self.span {
            AttentionSpan::Local(w) => w,
            AttentionSpan::Full => panic!("GatherRows requires AttentionSpan::Local"),
        }
    }

    pub fn mode(&self) -> GatherMode {
        self.mode
    }

    fn output_shape(&self, input: &Shape) -> Shape {
        let w = self.window();
        match self.mode {
            GatherMode::Rows => Shape::new(vec![input.dim(0), input.dim(1), w, input.dim(2)]),
            GatherMode::ScoreWindow => {
                // 末轴（key 轴）s → w，秩不变
                let mut dims: Vec<usize> = input.iter().copied().collect();
                let n = dims.len();
                dims[n - 1] = w;
                Shape::new(dims)
            }
        }
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
        last_claims: &[&Claim<F>],
        prover: &mut Prover<F, T, PCS>,
    ) -> Result<(GatherRowsProof<F, PCS>, Vec<Claim<F>>)>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F> + 'static,
        T: Transcript,
    {
        if self.mode == GatherMode::ScoreWindow {
            return self.prove_score_window(id, step_data, last_claims, prover);
        }
        let w = self.window();
        let input = step_data.input_tensor_at(0)?;
        // 输入约定：Rank3 [Hkv, s, dk]（K/V 布局），行主序展平 [Hkv·s, dk]
        let shape = input.shape();
        ensure!(
            shape.rank() == 3,
            "gather input must be rank-3 [Hkv, s, dk], got rank {}",
            shape.rank()
        );
        let heads = shape.dim(0);
        let s = shape.dim(1);
        let dk = shape.dim(2);
        ensure!(s.is_power_of_two(), "gather sequence length must be padded");
        ensure!(w.is_power_of_two(), "gather window must be a power of two");
        let t_len = heads * s * w;

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

        let k_field: Vec<Vec<F>> = (0..heads * s)
            .map(|r| (0..dk).map(|d| F::from(input.data()[r * dk + d])).collect())
            .collect();
        let (lookup_input, table_input) = build_gather_logup_inputs(
            &k_field,
            w,
            heads,
            constant_challenge,
            column_separation_challenge,
        )?;
        let logup_proof_lookup =
            new_batch_multiple_sizes_prove(&[lookup_input], prover.transcript)?;
        let logup_proof_table =
            new_batch_multiple_sizes_prove(&[table_input], prover.transcript)?;

        // 3) The claims at the logup point + the fraction balance
        let n_lookup = num_vars_of(t_len) - 1;
        let n_table = num_vars_of(heads * s) - 1;
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

        // 4) The claims wiring (§12): the logup point covers the (head, i, off)
        //    variables; the flattened poly's d-variables bind to the basis
        //    vectors e_d.
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

            // claim 布局（combine_final_evals）：lookup = [v0..v_{dk-1}, index]，
            // table = [multiplicity, v0..v_{dk-1}, index] —— table 的第 0 个
            // claim 是重数列，value 从 1 开始！
            let l_claim = claim_l
                .output_claims()
                .get(d)
                .ok_or_else(|| anyhow::anyhow!("missing lookup output claim {d}"))?;
            k_prime_claims.push(Claim::new(point.clone(), l_claim.evaluation()));

            let t_claim = claim_t
                .output_claims()
                .get(d + 1)
                .ok_or_else(|| anyhow::anyhow!("missing table output claim {d}"))?;
            input_claims.push(Claim::new(point.clone(), t_claim.evaluation()));
        }
        // 单 committed poly（K'）：logup 点 claims + consumer 的 claims 一起注册。
        // consumer（softmax/AV einsum 的 linking）对其输入（= 本节点输出）产生的
        // claim 位于 consumer 的 sumcheck 点——若不注册，两个承诺
        // （本节点的 K' poly vs consumer witness 里的输入列）之间没有交叉校验，
        // 是协议级 soundness 缺口。claims batching 是 eq 修正的多点批处理，
        // 一次 PCS opening 同时覆盖 consumer 点与 logup 点。
        let mut all_k_prime_claims = k_prime_claims;
        all_k_prime_claims.extend(last_claims.iter().map(|c| (*c).clone()));
        prover.add_witness_claim(id, vec![all_k_prime_claims]);

        // 4b) index/重数列 claim 对照闭式重算（position-exact 的关键：没有这一步，
        //     恶意 prover 可用任意索引映射证明错误的重排——如恒等映射）。
        {
            let idx_claim = claim_l
                .output_claims()
                .get(dk)
                .ok_or_else(|| anyhow::anyhow!("missing lookup index claim"))?;
            let index_col: Vec<F> = (0..t_len)
                .map(|t| F::from(gather_source_row(t, w, s) as u64))
                .collect();
            let expected = index_col.into_mle().evaluate(idx_claim.point())?;
            ensure!(
                expected == idx_claim.evaluation(),
                "gather lookup index column does not match the closed-form map"
            );
            // table：[mult, v0..v_{dk-1}, index] —— index 在 dk+1
            let idx_claim_t = claim_t
                .output_claims()
                .get(dk + 1)
                .ok_or_else(|| anyhow::anyhow!("missing table index claim"))?;
            let table_index: Vec<F> = (0..heads * s).map(|r| F::from(r as u64)).collect();
            let expected_t = table_index.into_mle().evaluate(idx_claim_t.point())?;
            ensure!(
                expected_t == idx_claim_t.evaluation(),
                "gather table index column does not match the row ids"
            );
            // 重数列 claim 对照闭式重数（table claims[0]）
            let mult_claim = claim_t
                .output_claims()
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing table multiplicity claim"))?;
            let mult_col: Vec<F> = gather_multiplicities_rows(w, s, heads)
                .into_iter()
                .map(F::from)
                .collect();
            let expected_m = mult_col.into_mle().evaluate(mult_claim.point())?;
            ensure!(
                expected_m == mult_claim.evaluation(),
                "gather table multiplicity column does not match the closed form"
            );
        }

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

    /// ScoreWindow 模式的 prove：末轴窗口切片的 logUp 重排（cell-id tuple）。
    /// 结构与 prove_gather 相同，但 value 列为 1 列、表侧为全部 `[L·q·s]`
    /// cell、claims 无 e_d 扩展（输出 poly 的变量 = 查询侧行变量）。
    fn prove_score_window<F, PCS, T>(
        &self,
        id: NodeId,
        step_data: &Step<Element>,
        last_claims: &[&Claim<F>],
        prover: &mut Prover<F, T, PCS>,
    ) -> Result<(GatherRowsProof<F, PCS>, Vec<Claim<F>>)>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F> + 'static,
        T: Transcript,
    {
        let w = self.window();
        let input = step_data.input_tensor_at(0)?;
        let shape = input.shape();
        ensure!(
            shape.rank() == 3 || shape.rank() == 4,
            "score window input must be rank-3/4, got rank {}",
            shape.rank()
        );
        let (heads, q, s) = match shape.rank() {
            3 => (shape.dim(0), shape.dim(1), shape.dim(2)),
            _ => (shape.dim(0) * shape.dim(1), shape.dim(2), shape.dim(3)),
        };
        ensure!(q.is_power_of_two() && s.is_power_of_two(), "score axes must be padded");
        let t_len = heads * q * w;
        let cells = heads * q * s;

        // 1) challenges
        let constant_challenge: F = prover.transcript.append_and_sample(b"gather_cc");
        let column_separation_challenge: F =
            prover.transcript.append_and_sample(b"gather_csc");

        // 2) committed gathered poly（查询侧 value 列）与输入 cell 值（表侧 value 列）
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
            k_prime_evals.len() == t_len,
            "gathered scores poly length mismatch: {} vs {}",
            k_prime_evals.len(),
            t_len
        );
        let data = input.data();
        let scores_flat: Vec<F> = data.iter().map(|x| F::from(*x)).collect();

        let (lookup_input, table_input) = build_score_window_logup_inputs(
            &scores_flat,
            w,
            q,
            s,
            heads,
            constant_challenge,
            column_separation_challenge,
        )?;
        let logup_proof_lookup =
            new_batch_multiple_sizes_prove(&[lookup_input], prover.transcript)?;
        let logup_proof_table =
            new_batch_multiple_sizes_prove(&[table_input], prover.transcript)?;

        // 3) claims at the logup points + fraction balance
        let inst_l = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            2,
            ProofType::Lookup,
            num_vars_of(t_len) - 1,
        );
        let inst_t = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            2,
            ProofType::Table,
            num_vars_of(cells) - 1,
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
            "score window fraction balance broken at node {id}"
        );

        // 4) claims wiring：value claim 直接落在 gathered poly 的全部变量上；
        //    claim 布局：lookup = [value, index]，table = [multiplicity, value, index]。
        //    index/重数列双侧闭式对照；表侧 value claim 上链到 mask 节点。
        let logup_point = claim_l.point().to_vec();
        let value_claim = claim_l
            .output_claims()
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing lookup value claim"))?
            .clone();
        ensure!(
            value_claim.point() == logup_point.as_slice(),
            "lookup value claim point mismatch"
        );

        {
            let idx_claim = claim_l
                .output_claims()
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("missing lookup index claim"))?;
            let index_col: Vec<F> = (0..t_len)
                .map(|t| F::from(score_window_source_cell(t, w, q, s) as u64))
                .collect();
            let expected = index_col.into_mle().evaluate(idx_claim.point())?;
            ensure!(
                expected == idx_claim.evaluation(),
                "score window lookup index column does not match the closed-form map"
            );
            // table：[mult, value, index]
            let mult_claim = claim_t
                .output_claims()
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing table multiplicity claim"))?;
            let mult_col: Vec<F> = score_window_multiplicities(w, q, s, heads)
                .into_iter()
                .map(F::from)
                .collect();
            let expected_m = mult_col.into_mle().evaluate(mult_claim.point())?;
            ensure!(
                expected_m == mult_claim.evaluation(),
                "score window table multiplicity column does not match the closed form"
            );
            let idx_claim_t = claim_t
                .output_claims()
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("missing table index claim"))?;
            let table_index: Vec<F> = (0..cells).map(|c| F::from(c as u64)).collect();
            let expected_t = table_index.into_mle().evaluate(idx_claim_t.point())?;
            ensure!(
                expected_t == idx_claim_t.evaluation(),
                "score window table index column does not match the cell ids"
            );
        }

        let input_claim = claim_t
            .output_claims()
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("missing table value claim"))?
            .clone();

        // consumer claims 一并注册（同 Rows 模式的 soundness 说明）
        let mut all_claims = vec![value_claim];
        all_claims.extend(last_claims.iter().map(|c| (*c).clone()));
        prover.add_witness_claim(id, vec![all_claims]);

        let commitments = vec![k_prime_commitment];
        Ok((
            GatherRowsProof {
                logup_proof_lookup,
                logup_proof_table,
                commitments,
            },
            vec![input_claim],
        ))
    }

    /// The verifier mirror of [`prove_gather`] / [`prove_score_window`].
    pub(crate) fn verify_gather<F, PCS, T>(
        &self,
        id: NodeId,
        proof: &GatherRowsProof<F, PCS>,
        unpadded_input_shape: &Shape,
        last_claims: &[&Claim<F>],
        verifier: &mut crate::iop::verifier::Verifier<F, T, PCS>,
    ) -> Result<Vec<Claim<F>>>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F> + 'static,
        T: Transcript,
    {
        let w = self.window();
        if self.mode == GatherMode::ScoreWindow {
            return self.verify_score_window(id, proof, unpadded_input_shape, last_claims, verifier);
        }
        // 输入约定：Rank3 [Hkv, s, dk]，与 prove_gather 一致
        ensure!(
            unpadded_input_shape.rank() == 3,
            "gather input must be rank-3 [Hkv, s, dk], got rank {}",
            unpadded_input_shape.rank()
        );
        let heads = unpadded_input_shape.dim(0);
        let s = unpadded_input_shape.dim(1);
        let dk = unpadded_input_shape.dim(2);

        // 1) The challenges (mirroring the prover's sampling)
        let constant_challenge: F = verifier.transcript.append_and_sample(b"gather_cc");
        let column_separation_challenge: F =
            verifier.transcript.append_and_sample(b"gather_csc");

        // 2) The claims at the logup point + the fraction balance
        let n_lookup = num_vars_of(heads * s * w) - 1;
        let n_table = num_vars_of(heads * s) - 1;
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

            // claim 布局：lookup = [v0..v_{dk-1}, index]，table = [mult, v0..v_{dk-1}, index]
            let l_claim = claim_l
                .output_claims()
                .get(d)
                .ok_or_else(|| anyhow::anyhow!("missing lookup output claim {d}"))?;
            k_prime_claims.push(
                Claim::new(point.clone(), l_claim.evaluation()),
            );

            let t_claim = claim_t
                .output_claims()
                .get(d + 1)
                .ok_or_else(|| anyhow::anyhow!("missing table output claim {d}"))?;
            input_claims.push(Claim::new(point.clone(), t_claim.evaluation()));
        }

        // 3b) index/重数列 claim 对照闭式重算（与 prove 侧一致）
        {
            let t_len = heads * s * w;
            let idx_claim = claim_l
                .output_claims()
                .get(dk)
                .ok_or_else(|| anyhow::anyhow!("missing lookup index claim"))?;
            let index_col: Vec<F> = (0..t_len)
                .map(|t| F::from(gather_source_row(t, w, s) as u64))
                .collect();
            let expected = index_col.into_mle().evaluate(idx_claim.point())?;
            ensure!(
                expected == idx_claim.evaluation(),
                "gather lookup index column does not match the closed-form map"
            );
            // table：[mult, v0..v_{dk-1}, index]
            let idx_claim_t = claim_t
                .output_claims()
                .get(dk + 1)
                .ok_or_else(|| anyhow::anyhow!("missing table index claim"))?;
            let table_index: Vec<F> = (0..heads * s).map(|r| F::from(r as u64)).collect();
            let expected_t = table_index.into_mle().evaluate(idx_claim_t.point())?;
            ensure!(
                expected_t == idx_claim_t.evaluation(),
                "gather table index column does not match the row ids"
            );
            let mult_claim = claim_t
                .output_claims()
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing table multiplicity claim"))?;
            let mult_col: Vec<F> = gather_multiplicities_rows(w, s, heads)
                .into_iter()
                .map(F::from)
                .collect();
            let expected_m = mult_col.into_mle().evaluate(mult_claim.point())?;
            ensure!(
                expected_m == mult_claim.evaluation(),
                "gather table multiplicity column does not match the closed form"
            );
        }

        // 4) Register the K' commitment claims (from the proof structure):
        //    logup 点 claims + consumer 的 claims（与 prove 侧一致，见
        //    prove_gather 第 4 步的 soundness 说明）
        if let Some(commitment) = proof.commitments.first() {
            let mut all_claims = k_prime_claims;
            all_claims.extend(last_claims.iter().map(|c| (*c).clone()));
            verifier
                .commit_verifier
                .add_witness_claim(id, vec![VerifierClaim {
                    commitment: commitment.clone(),
                    claims: all_claims,
                }]);
        }

        Ok(input_claims)
    }

    /// ScoreWindow 模式的 verifier 镜像（[`prove_score_window`]）。
    fn verify_score_window<F, PCS, T>(
        &self,
        id: NodeId,
        proof: &GatherRowsProof<F, PCS>,
        unpadded_input_shape: &Shape,
        last_claims: &[&Claim<F>],
        verifier: &mut crate::iop::verifier::Verifier<F, T, PCS>,
    ) -> Result<Vec<Claim<F>>>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F> + 'static,
        T: Transcript,
    {
        let w = self.window();
        ensure!(
            unpadded_input_shape.rank() == 3 || unpadded_input_shape.rank() == 4,
            "score window input must be rank-3/4, got rank {}",
            unpadded_input_shape.rank()
        );
        let (heads, q, s) = match unpadded_input_shape.rank() {
            3 => (
                unpadded_input_shape.dim(0),
                unpadded_input_shape.dim(1),
                unpadded_input_shape.dim(2),
            ),
            _ => (
                unpadded_input_shape.dim(0) * unpadded_input_shape.dim(1),
                unpadded_input_shape.dim(2),
                unpadded_input_shape.dim(3),
            ),
        };
        let t_len = heads * q * w;
        let cells = heads * q * s;

        // 1) challenges
        let constant_challenge: F = verifier.transcript.append_and_sample(b"gather_cc");
        let column_separation_challenge: F =
            verifier.transcript.append_and_sample(b"gather_csc");

        // 2) GKR verify + fraction balance
        let inst_l = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            2,
            ProofType::Lookup,
            num_vars_of(t_len) - 1,
        );
        let inst_t = LogUpVerifierInstance::new(
            constant_challenge,
            column_separation_challenge,
            2,
            ProofType::Table,
            num_vars_of(cells) - 1,
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
            "score window fraction balance broken at node {id}"
        );

        // 3) claims wiring + index/重数列闭式对照
        // claim 布局：lookup = [value, index]，table = [mult, value, index]
        let value_claim = claim_l
            .output_claims()
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing lookup value claim"))?
            .clone();
        {
            let idx_claim = claim_l
                .output_claims()
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("missing lookup index claim"))?;
            let index_col: Vec<F> = (0..t_len)
                .map(|t| F::from(score_window_source_cell(t, w, q, s) as u64))
                .collect();
            let expected = index_col.into_mle().evaluate(idx_claim.point())?;
            ensure!(
                expected == idx_claim.evaluation(),
                "score window lookup index column does not match the closed-form map"
            );
            let mult_claim = claim_t
                .output_claims()
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing table multiplicity claim"))?;
            let mult_col: Vec<F> = score_window_multiplicities(w, q, s, heads)
                .into_iter()
                .map(F::from)
                .collect();
            let expected_m = mult_col.into_mle().evaluate(mult_claim.point())?;
            ensure!(
                expected_m == mult_claim.evaluation(),
                "score window table multiplicity column does not match the closed form"
            );
            let idx_claim_t = claim_t
                .output_claims()
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("missing table index claim"))?;
            let table_index: Vec<F> = (0..cells).map(|c| F::from(c as u64)).collect();
            let expected_t = table_index.into_mle().evaluate(idx_claim_t.point())?;
            ensure!(
                expected_t == idx_claim_t.evaluation(),
                "score window table index column does not match the cell ids"
            );
        }
        let input_claim = claim_t
            .output_claims()
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("missing table value claim"))?
            .clone();

        // 4) consumer claims 一并注册（同 prove 侧）
        if let Some(commitment) = proof.commitments.first() {
            let mut all_claims = vec![value_claim];
            all_claims.extend(last_claims.iter().map(|c| (*c).clone()));
            verifier
                .commit_verifier
                .add_witness_claim(id, vec![VerifierClaim {
                    commitment: commitment.clone(),
                    claims: all_claims,
                }]);
        }

        Ok(vec![input_claim])
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
        last_claims: &[&Claim<F>],
        verifier: &mut crate::iop::verifier::Verifier<F, T, PCS>,
        shape_step: &ShapeStep,
        node_id: NodeId,
    ) -> Result<Vec<Claim<F>>> {
        let unpadded_input_shape = &shape_step.unpadded_input_shape[0];
        self.verify_gather(node_id, proof, unpadded_input_shape, last_claims, verifier)
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
        last_claims: Vec<&Claim<F>>,
        step_data: &Step<Element>,
        prover: &mut Prover<'c, 'd, F, T, PCS>,
    ) -> Result<Vec<Claim<F>>> {
        let (proof, input_claims) =
            _ctx.prove_gather::<F, PCS, T>(node_id, step_data, &last_claims, prover)?;
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
        let flat = self.gathered_mle::<F>(&input)?;
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
            mode: self.mode,
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
