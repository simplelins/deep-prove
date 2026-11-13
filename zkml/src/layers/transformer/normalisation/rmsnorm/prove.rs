//! Internal methods for proving the [`RMSNorm`] layer.

use crate::{
    layers::LayerProof,
    lookup::operation::generic_prove::{GenericLookupProof, LookupProverResult, prove_lookup_op},
};

use std::collections::HashMap;

use super::*;

impl RMSNorm<Element> {
    pub(crate) fn prove_internal<F, T, PCS>(
        &self,
        id: NodeId,
        last_claim: &Claim<F>,
        step_data: &Step<Element>,
        prover: &mut Prover<F, T, PCS>,
    ) -> Result<Vec<Claim<F>>>
    where
        F: PrimeField,
        T: Transcript,
        PCS: CommitmentScheme<Field = F>,
    {
        let lookup_op = step_data
            .node_outputs
            .try_rmsnorm_data()
            .ok_or(anyhow!(
                "Could not get RMSNorm proving data for lookup witness generation"
            ))?
            .to_proving_data()?;

        let (_, normalisation_scaling_factor) = self.get_quantisation_scaling_factors().ok_or(
            anyhow!("Quantisation scaling factors not found for RMSNorm lookup witness generation"),
        )?;
        let table = Table::new_normalisation(normalisation_scaling_factor.bit_size() + 1);

        // P1 决定性探针：witness 重生成比对。冻结 witness（witness-gen 时刻
        // 提交的多项式）vs 现在从同一 step 数据重新生成的 witness：
        // - 不一致 → witness-gen 非确定性（数据/并行竞态）
        // - 一致而 linking 自检仍触发 → claim 链是移动部件
        #[cfg(feature = "p1-debug")]
        {
            let frozen = prover.lookup_witness(id)?;
            let fresh_gen = self.lookup_witness(id, step_data)?;
            let fresh_polys = fresh_gen.polys(&id).unwrap_or(&[]);
            let mut mismatches = 0usize;
            for (i, (fr, fz)) in fresh_polys.iter().zip(frozen.iter()).enumerate() {
                let fresh_evals: Vec<F> = fr.evals().into_iter().collect();
                let frozen_evals: Vec<F> = fz.polynomial().evals().into_iter().collect();
                if fresh_evals.len() != frozen_evals.len() {
                    eprintln!(
                        "[p1-wregen] node {id} poly {i}: 长度不一致 fresh={} frozen={}",
                        fresh_evals.len(),
                        frozen_evals.len()
                    );
                    mismatches += 1;
                } else {
                    let fh = witness_fnv(&fresh_evals);
                    let zh = witness_fnv(&frozen_evals);
                    if fh != zh {
                        mismatches += 1;
                        eprintln!(
                            "[p1-wregen] node {id} poly {i}: 指纹不一致 fresh={fh:#x} frozen={zh:#x}"
                        );
                    }
                }
            }
            if mismatches > 0 {
                eprintln!("[p1-wregen] node {id}: witness 重生成比对 {mismatches} 个 poly 不一致");
            }
        }

        let LookupProverResult {
            generic_proof,
            input_claims,
            weight_evaluation_point,
        } = if let Some(alpha_handle) = self.alpha.as_ref() {
            let wrapped_alpha = alpha_handle.wrapped_tensor()?.clone();
            let alpha = Tensor::try_from(&wrapped_alpha)?;

            prove_lookup_op(
                &lookup_op,
                last_claim,
                step_data,
                &table,
                Some(&alpha),
                prover,
                id,
            )?
        } else {
            prove_lookup_op(&lookup_op, last_claim, step_data, &table, None, prover, id)?
        };

        let GenericLookupProof {
            logup_proof,
            sumcheck_proof,
            evaluations,
            commitments,
            weight_evaluation,
            ..
        } = generic_proof;

        let proof = RMSNormProof {
            logup_proof,
            right_shift: lookup_op.right_shift() as Element,
            commitments,
            io_proof: sumcheck_proof,
            io_evaluations: evaluations,
            alpha_evaluation: weight_evaluation,
        };

        // Add the proof to the prover
        prover.push_proof(id, LayerProof::RMSNorm(proof));

        match (
            weight_evaluation,
            weight_evaluation_point,
            self.alpha.as_ref(),
        ) {
            (Some(alpha_eval), Some(weight_eval_point), Some(alpha_tensor)) => {
                // Add the weight commitment claim to the prover
                let alpha_key = CommitmentId::from(alpha_tensor.storage_key());
                let claims_map =
                    HashMap::from([(alpha_key, Claim::<F>::new(weight_eval_point, alpha_eval))]);
                prover.add_common_claims(id, claims_map);
            }
            (None, None, None) => {
                // No alpha key, nothing to check
            }
            _ => {
                bail!("Inconsistent alpha key/evaluation state in RMSNorm proving");
            }
        }
        // Return the input claim for the next layer
        Ok(input_claims)
    }
}

/// P1 调试：witness 多项式指纹（FNV-1a over the evals）。
#[cfg(feature = "p1-debug")]
pub(crate) fn witness_fnv<F: PrimeField>(evals: &[F]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in evals {
        for limb in x.into_bigint().as_ref() {
            h ^= limb & 0xffff;
            h = h.wrapping_mul(0x100000001b3);
            h ^= (limb >> 16) & 0xffff;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}
