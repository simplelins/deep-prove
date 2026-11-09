//! Code for generating lookup witnesses for the Requant layer.

use dp_crypto::IntoMLE;

use super::*;

impl Requant {
    pub(crate) fn lookup_witness<F: PrimeField>(
        &self,
        id: NodeId,
        input: &Tensor<Element>,
    ) -> Result<LookupWitnessGen<'_, F>> {
        let lookup_data = self.activation_lookup_data;
        let lookup_witness = lookup_data.get_lookup_witness(input.clone())?;
        let element_counts = lookup_witness.get_counts(&lookup_data.table);

        let input_evals = lookup_witness.input_mle_evals::<F>(lookup_data.table.num_columns());
        let output_evals = lookup_witness.output_mle_evals::<F>();

        // P1 突变检测器（p1-debug）：witness 重算值 vs 张量现值逐元素对照。
        // witness = f(当前 input 张量)；若与 forward 时的张量内容不一致，
        // 说明 trace 张量在前向之后被改写（pipeline 并发窗口内的数据竞争）。
        #[cfg(feature = "p1-debug")]
        {
            let input_vals = input.data();
            let mut in_mismatch = 0usize;
            for (idx, v) in input_vals.iter().enumerate() {
                // input_evals 为按列组织（第 0 列 = input 值）
                if let Some(col) = input_evals.first() {
                    if let Some(e) = col.get(idx) {
                        let want = F::from(*v);
                        if *e != want {
                            in_mismatch += 1;
                            if in_mismatch <= 3 {
                                eprintln!(
                                    "[p1-mut] node {id} input idx {idx}: witness={e:?} tensor={v}"
                                );
                            }
                        }
                    }
                }
            }
            if in_mismatch > 0 {
                eprintln!("[p1-mut] node {id}: input 张量在前向与 witness-gen 之间被改写（{in_mismatch} 处不一致）");
            }
        }

        let mles = input_evals
            .into_iter()
            .chain(output_evals)
            .map(|evals| evals.into_mle())
            .collect();

        let mut gen_w = LookupWitnessGen::<F>::default();
        let tables = vec![
            Table::new_shift_check(),
            lookup_data.table,
            Table::new_zero_check(),
            Table::new_signed_zero_check(),
        ];
        gen_w.insert_layer_witness_data(id, mles, tables, element_counts);

        Ok(gen_w)
    }
}
