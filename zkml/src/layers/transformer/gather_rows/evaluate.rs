//! Code for evaluating the row-gather layer (D1.1 Stage A).
//!
//! `K'[h, i, off, d] = K[h, j(i,off), d]` — computed as `w` pad-top shifts of
//! K interleaved along the window dim (each shift `O(s·d)`; the full `[s, s]`
//! score matrix is never materialized).

use anyhow::{bail, ensure, Result};
use burn::tensor::{Tensor as BTensor, TensorData};

use super::*;
use crate::backend::Backend;
use crate::tensor::{TensorTypeParam, WrappedTensor};
use crate::Element;

impl GatherRows {
    /// Gathers the rows of `k` according to the window map.
    ///
    /// Inputs: `k` `[Hkv, s, dk]`. Output: `k'` `[Hkv, s, w, dk]` with
    /// `k'[.., i, off, :] = k[.., j(i,off), :]` (clamped positions included;
    /// invalid window positions are masked later by the mask layer).
    pub(crate) fn evaluate_internal<T: TensorTypeParam>(
        &self,
        inputs: &[&WrappedTensor<T>],
    ) -> Result<WrappedTensor<T>> {
        ensure!(
            inputs.len() == 1,
            "GatherRows expects exactly one input (K), got {}",
            inputs.len()
        );
        let w = self.window();
        let (k, unpadded_shape) = match inputs[0] {
            WrappedTensor::Rank3 {
                tensor,
                unpadded_shape,
            } => (tensor, unpadded_shape),
            _ => bail!("GatherRows expects a rank-3 K input"),
        };
        let [hkv, s, dk] = k.dims();

        let device = k.device();
        let mut bands = Vec::with_capacity(w);
        for off in 0..w {
            let delta = w - 1 - off;
            // clamp 语义：j = max(0, i-δ) → 行 i<δ 复制 K[0]（与 f 的闭式定义一致；
            // 这些位置多数为无效窗口位，mask 层后续置 -inf）
            let k_shift: BTensor<Backend, 3, T::Kind> = if delta == 0 {
                k.clone()
            } else if delta >= s {
                // 整段 clamp 到第 0 行
                let first = k.clone().slice([0..hkv, 0..1, 0..dk]);
                first.repeat(&[1, s, 1])
            } else {
                let rows = s - delta;
                let first = k.clone().slice([0..hkv, 0..1, 0..dk]).repeat(&[1, delta, 1]);
                let kept = k.clone().slice([0..hkv, 0..rows, 0..dk]);
                BTensor::cat(vec![first, kept], 1)
            };
            // [hkv, s, dk] → [hkv, s, 1, dk]（off 维插入）
            let band: BTensor<Backend, 4, T::Kind> =
                k_shift.reshape([hkv, s, 1, dk]);
            bands.push(band);
        }
        // 交错沿 off 维 → [hkv, s, w, dk]
        let gathered = BTensor::cat(bands, 2);
        Ok(WrappedTensor::Rank4 {
            tensor: gathered,
            unpadded_shape: unpadded_shape.clone(),
        })
    }
}

impl crate::layers::provable::Evaluate<f32> for GatherRows {
    fn evaluate(&self, inputs: &[&WrappedTensor<f32>]) -> Result<crate::layers::LayerOut<f32>> {
        let out = self.evaluate_internal(inputs)?;
        Ok(crate::layers::LayerOut::from_vec(vec![out]))
    }
}

impl crate::layers::provable::Evaluate<Element> for GatherRows {
    fn evaluate(&self, inputs: &[&WrappedTensor<Element>]) -> Result<crate::layers::LayerOut<Element>> {
        let out = self.evaluate_internal(inputs)?;
        Ok(crate::layers::LayerOut::from_vec(vec![out]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::transformer::attention_mask::AttentionSpan;

    type FKind = <f32 as TensorTypeParam>::Kind;

    /// Gather parity: the burn-op output must match the naive row gather.
    #[test]
    fn test_gather_evaluate_parity() -> Result<()> {
        let (hkv, s, dk, w) = (1usize, 8usize, 4usize, 3usize);
        let k_vec: Vec<f32> = (0..s * dk).map(|x| ((x * 7) % 11) as f32 - 5.0).collect();
        let k = BTensor::<Backend, 3, FKind>::from_data(
            TensorData::new(k_vec.clone(), [hkv, s, dk]),
            &Default::default(),
        );

        let layer: GatherRows = GatherRows::new(AttentionSpan::Local(w), 1);
        let k_w = WrappedTensor::<f32>::Rank3 {
            tensor: k.clone(),
            unpadded_shape: burn::prelude::Shape::new([hkv, s, dk]),
        };
        let gathered = layer.evaluate_internal(&[&k_w])?;
        let WrappedTensor::Rank4 {
            tensor: g_t,
            unpadded_shape: _,
        } = &gathered
        else {
            bail!("expected rank-4 gathered tensor");
        };
        assert_eq!(g_t.dims(), [hkv, s, w, dk]);

        let g_data = g_t.clone().into_data().into_vec::<f32>().unwrap();
        let k_naive: Vec<Vec<f32>> = (0..s)
            .map(|j| (0..dk).map(|dd| k_vec[j * dk + dd]).collect())
            .collect();
        for i in 0..s {
            for off in 0..w {
                let j = gather_source_index(i * w + off, w, s);
                for dd in 0..dk {
                    let got = g_data[i * w * dk + off * dk + dd];
                    let want = k_naive[j][dd];
                    assert!(
                        (got - want).abs() < 1e-6,
                        "K'[{i},{off},{dd}]={got} != K[{j},{dd}]={want}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Full Stage-A inference parity: banded scores via (gather + elementwise
    /// dot) must equal the naive masked full-attention scores at valid window
    /// positions.
    #[test]
    fn test_banded_via_gather_matches_full() -> Result<()> {
        let (hq, s, dk, w) = (1usize, 8usize, 4usize, 3usize);
        let q_vec: Vec<f32> = (0..s * dk).map(|x| ((x * 3) % 7) as f32 - 3.0).collect();
        let k_vec: Vec<f32> = (0..s * dk).map(|x| ((x * 5) % 9) as f32 - 4.0).collect();
        let q = BTensor::<Backend, 3, FKind>::from_data(
            TensorData::new(q_vec.clone(), [hq, s, dk]),
            &Default::default(),
        );
        let k = BTensor::<Backend, 3, FKind>::from_data(
            TensorData::new(k_vec.clone(), [hq, s, dk]),
            &Default::default(),
        );

        let layer: GatherRows = GatherRows::new(AttentionSpan::Local(w), 1);
        let k_w = WrappedTensor::<f32>::Rank3 {
            tensor: k.clone(),
            unpadded_shape: burn::prelude::Shape::new([hq, s, dk]),
        };
        let gathered = layer.evaluate_internal(&[&k_w])?;
        let WrappedTensor::Rank4 {
            tensor: k_p,
            unpadded_shape: _,
        } = &gathered
        else {
            bail!("expected rank-4");
        };

        // Q rep: each query row repeated w times (t = i·w + off order)
        let q_rep = q
            .clone()
            .reshape([hq, s, 1, dk])
            .repeat(&[1, 1, w, 1])
            .reshape([hq, s * w, dk]);
        let k_p_flat = k_p.clone().reshape([hq, s * w, dk]);
        // b[t] = Σ_d q_rep[t,d]·k_p[t,d]
        let b = q_rep.mul(k_p_flat).sum_dim(2).reshape([hq, s, w]);

        // naive baseline: full QKᵀ sliced at the window indices
        let full = q
            .clone()
            .matmul(k.clone().transpose())
            .into_data()
            .into_vec::<f32>()
            .unwrap();
        let b_data = b.into_data().into_vec::<f32>().unwrap();
        for i in 0..s {
            for off in 0..w {
                if !gather_position_valid(i * w + off, w) {
                    continue;
                }
                let j = gather_source_index(i * w + off, w, s);
                let got = b_data[i * w + off];
                let want = full[i * s + j];
                assert!(
                    (got - want).abs() < 1e-4,
                    "B[{i},{off}]={got} != full[{i},{j}]={want}"
                );
            }
        }
        Ok(())
    }
}
