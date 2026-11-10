//! Proof-side construction for the GatherRows layer (D1.1 Stage A).
//!
//! The gather relation `K'[t, d] = K[f(t), d]` is proven as a **logUp
//! rearrangement argument**: the "table" side carries the (index, value)
//! columns of the committed K with the closed-form multiplicities of the
//! gather map, and the "lookup" side carries the (index, value) columns of
//! the committed K'. The multi-column tuple binding (index and value columns
//! combined with the separation challenge) makes the pairing position-exact.

use anyhow::{bail, ensure};
use ark_ff::PrimeField;

use crate::lookup::logup_gkr::structs::{LogUpInput, ProofType};

use super::{gather_multiplicities, gather_source_index};

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
    use crate::lookup::logup_gkr::prover::new_batch_multiple_sizes_prove;
    use crate::lookup::logup_gkr::verifier::new_verify_logup_proof_multiple_sizes;
    use anyhow::Result;
    use ark_bn254::Fr;
    use ark_std::UniformRand;
    use dp_crypto::arkyper::transcript::blake3::Blake3Transcript;
    use dp_crypto::arkyper::transcript::Transcript;

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
