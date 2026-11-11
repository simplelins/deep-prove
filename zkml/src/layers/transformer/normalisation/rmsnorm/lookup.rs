//! Internal code for generating the [`RMSNorm`] looup witness.

use dp_crypto::IntoMLE;

use crate::{lookup::context::LookupWitnessGen, model::Step, to_field};

use super::*;

impl RMSNorm<Element> {
    pub(crate) fn lookup_witness<F: PrimeField>(
        &self,
        id: NodeId,
        step: &Step<Element>,
    ) -> Result<LookupWitnessGen<'_, F>> {
        let rmsnorm_data = step
            .node_outputs
            .try_rmsnorm_data()
            .ok_or(anyhow!(
                "Could not get RMSNorm proving data for lookup witness generation"
            ))?
            .to_proving_data()?;

        let input = step.input_tensor_at(0)?;
        let input_shape = input.shape().clone();

        // P1 探针 B（真突变检测）：witness 输出列 vs step 存储输出逐元素对照。
        // 注意：比较对象必须是 generate_witness 产出的输出列（协议真实使用的
        // 数据），而不是 rmsnorm_data.apply 的直接产物——gemma3 的 (1+w) 处理
        // 使 apply 路径与 forward evaluate 不同（实测对所有节点都会 DIFF，
        // 是探针误报源）。下游 consumer 的 claim 链锚定在存储张量上，
        // witness 输出列与存储张量不一致 = P1 签名（self-check 触发的根源）。
        #[cfg(feature = "p1-debug")]
        let stored_output = step.output_tensor_at(0).ok().map(|t| {
            let mut d = t.data().clone();
            // pad 到 2 的幂（与 witness 列的 padding 一致，0 填充段跳过比较）
            let pad = d.len().next_power_of_two() - d.len();
            d.extend(std::iter::repeat_n(0i64, pad));
            d
        });

        let (_, normalisation_scaling_factor) = self.get_quantisation_scaling_factors().ok_or(
            anyhow!("Quantisation scaling factors not found for RMSNorm lookup witness generation"),
        )?;
        let table = Table::new_normalisation(normalisation_scaling_factor.bit_size() + 1);

        let wrapped_input = WrappedTensor::try_from(input.as_ref())?;
        let lookup_output = rmsnorm_data.apply(wrapped_input.clone(), &table)?;
        let scaled = wrapped_input.mul(rmsnorm_data.normalisation.clone())?;
        let scaled_input = Tensor::try_from(&scaled)?;
        let output = Tensor::try_from(&lookup_output)?;

        // P1 探针 C（记录侧）：witness-gen 时刻的输入张量哈希入表，
        // prove 侧（generic_prove）对照 —— 抓 witness-gen → prove 窗口。
        #[cfg(feature = "p1-debug")]
        crate::lookup::p1_probe::record_witness_input(id, input.data(), &input_shape);

        let rank = input_shape.rank();
        let number_of_chunks = input_shape[..rank.saturating_sub(2)]
            .iter()
            .product::<usize>();

        let lookup_witness = rmsnorm_data.generate_witness(scaled_input, &table)?;

        let mut element_counts = lookup_witness.get_counts(&table);
        let variant = rmsnorm_data.variant();

        let mut norm_counts =
            variant.compute_normalisation_witness_counts(number_of_chunks, output.as_ref())?;

        // Merge in the normalisation counts
        let norm_map = norm_counts.remove(0);
        for (key, val) in norm_map {
            let entry = element_counts[0].entry(key).or_insert(0);
            *entry += val;
        }

        let input_evals = lookup_witness.input_mle_evals::<F>(table.num_columns());
        let output_evals = lookup_witness.output_mle_evals::<F>();

        // P1 探针 B（续）：witness 输出列 vs 存储输出（此时 output_evals 已生成）
        #[cfg(feature = "p1-debug")]
        if let Some(stored) = &stored_output {
            if let Some(wit_col) = output_evals.first() {
                if stored.len() == wit_col.len() {
                    let mismatches = stored
                        .iter()
                        .zip(wit_col.iter())
                        .enumerate()
                        .filter(|(_, (v, e))| F::from(**v) != **e)
                        .count();
                    if mismatches > 0 {
                        let first = stored
                            .iter()
                            .zip(wit_col.iter())
                            .position(|(v, e)| F::from(*v) != *e);
                        eprintln!(
                            "[p1-rms] node {id}: witness 输出列 ≠ 存储输出（{mismatches} 处不一致，首个 idx {first:?}）"
                        );
                    }
                } else {
                    eprintln!(
                        "[p1-rms] node {id}: witness 输出列长度 {} ≠ 存储长度 {}（列布局未知，跳过）",
                        wit_col.len(),
                        stored.len()
                    );
                }
            }
        }


        // Make the commitments to the multipliers
        let dim_size = if rank >= 2 { input_shape[rank - 2] } else { 1 };

        let normalisation_evals = rmsnorm_data
            .normalisation
            .get_data()
            .chunks(dim_size)
            .map(|chunk| {
                let evals = chunk.iter().copied().chain(std::iter::repeat_n(
                    0,
                    dim_size.next_power_of_two() - chunk.len(),
                ));
                to_field::<_, F, _>(evals)
            })
            .collect::<Vec<Vec<F>>>();

        let mles = input_evals
            .into_iter()
            .chain(output_evals)
            .chain(normalisation_evals)
            .map(|evals| evals.into_mle())
            .collect();

        let mut gen_w = LookupWitnessGen::<F>::default();
        let tables = vec![
            Table::new_shift_check(),
            table,
            Table::new_zero_check(),
            Table::new_signed_zero_check(),
        ];
        gen_w.insert_layer_witness_data(id, mles, tables, element_counts);

        Ok(gen_w)
    }
}
