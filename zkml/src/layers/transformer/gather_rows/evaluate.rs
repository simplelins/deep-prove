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
    /// Dispatches on the gather mode.
    pub(crate) fn evaluate_internal<T: TensorTypeParam>(
        &self,
        inputs: &[&WrappedTensor<T>],
    ) -> Result<WrappedTensor<T>> {
        ensure!(
            inputs.len() == 1,
            "GatherRows expects exactly one input, got {}",
            inputs.len()
        );
        match self.mode {
            GatherMode::Rows => self.evaluate_rows_internal(inputs),
            GatherMode::ScoreWindow => self.evaluate_score_window_internal(inputs),
        }
    }

    /// ScoreWindow variant: slices the last (key) axis of the score tensor
    /// down to the window. Input `[L, q, s]`（Rank3；GQA 的 Rank4 先折叠前导
    /// 轴）→ 输出 `[L, q, w]`，`out[.., i, off] = in[.., i, j(i,off)]`。
    /// 索引张量 `[q, w]` 由闭式映射生成，一次 `gather(dim=2)` 完成。
    fn evaluate_score_window_internal<T: TensorTypeParam>(
        &self,
        inputs: &[&WrappedTensor<T>],
    ) -> Result<WrappedTensor<T>> {
        let w = self.window();
        // 折叠前导轴 → [L, q, s]（保持 Rank3 处理，输出再还原秩）
        let rank4 = matches!(inputs[0], WrappedTensor::Rank4 { .. });
        let (in3, lead, unpadded_dims) = match inputs[0] {
            WrappedTensor::Rank3 {
                tensor,
                unpadded_shape,
            } => (tensor.clone(), None, unpadded_shape.dims.clone()),
            WrappedTensor::Rank4 {
                tensor,
                unpadded_shape,
            } => {
                let [d0, d1, q, s] = tensor.dims();
                (
                    tensor.clone().reshape([d0 * d1, q, s]),
                    Some((d0, d1)),
                    unpadded_shape.dims.clone(),
                )
            }
            _ => bail!("ScoreWindow expects a rank-3/4 score tensor"),
        };
        let [l, q, s] = in3.dims();

        // 闭式索引 [q, w]（i64），扩展到 [L, q, w]
        let idx_vec: Vec<i64> = (0..q * w)
            .map(|t| {
                let i = t / w;
                let off = t % w;
                (i as i64 - (w as i64 - 1) + off as i64).clamp(0, s as i64 - 1)
            })
            .collect();
        let device = in3.device();
        let idx = BTensor::<Backend, 2, burn::tensor::Int>::from_data(
            TensorData::new(idx_vec, [q, w]),
            &device,
        );
        let idx = idx.reshape([1, q, w]).repeat(&[l, 1, 1]);
        let gathered = in3.gather(2, idx); // [L, q, w]

        // unpadded：末维（key 轴）替换为 w，其余保留
        let mut out_unpadded = unpadded_dims;
        let n = out_unpadded.len();
        if n >= 1 {
            let last = out_unpadded[n - 1];
            out_unpadded[n - 1] = w;
            let _ = last;
        }

        let out = if let Some((d0, d1)) = lead {
            WrappedTensor::Rank4 {
                tensor: gathered.reshape([d0, d1, q, w]),
                unpadded_shape: burn::prelude::Shape::from(out_unpadded),
            }
        } else {
            WrappedTensor::Rank3 {
                tensor: gathered,
                unpadded_shape: burn::prelude::Shape::from(out_unpadded),
            }
        };
        Ok(out)
    }

    /// Rows variant: gathers the rows of `k` according to the window map.
    ///
    /// Inputs: `k` `[Hkv, s, dk]`. Output: `k'` `[Hkv, s, w, dk]` with
    /// `k'[.., i, off, :] = k[.., j(i,off), :]` (clamped positions included;
    /// invalid window positions are masked later by the mask layer).
    fn evaluate_rows_internal<T: TensorTypeParam>(
        &self,
        inputs: &[&WrappedTensor<T>],
    ) -> Result<WrappedTensor<T>> {
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
        // 输出的 unpadded 形状：在末维前插入 w（输入 unpadded = [Hkv, s, dk]）
        let mut out_unpadded = unpadded_shape.dims.clone();
        let last = out_unpadded.pop().unwrap_or(dk);
        out_unpadded.push(w);
        out_unpadded.push(last);
        Ok(WrappedTensor::Rank4 {
            tensor: gathered,
            unpadded_shape: burn::prelude::Shape::from(out_unpadded),
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

    /// ScoreWindow 对拍：burn gather 输出与朴素窗口切片逐元素一致。
    #[test]
    fn test_score_window_evaluate_parity() -> Result<()> {
        let (l, q, s, w) = (2usize, 8usize, 8usize, 4usize);
        let a_vec: Vec<f32> = (0..l * q * s).map(|x| ((x * 7) % 13) as f32 - 6.0).collect();
        let a = BTensor::<Backend, 3, FKind>::from_data(
            TensorData::new(a_vec.clone(), [l, q, s]),
            &Default::default(),
        );

        let layer: GatherRows =
            GatherRows::new_score_window(AttentionSpan::Local(w), 1);
        let a_w = WrappedTensor::<f32>::Rank3 {
            tensor: a,
            unpadded_shape: burn::prelude::Shape::new([l, q, s]),
        };
        let out = layer.evaluate_internal(&[&a_w])?;
        let WrappedTensor::Rank3 {
            tensor: out_t,
            unpadded_shape: _,
        } = &out
        else {
            bail!("expected rank-3 windowed scores");
        };
        assert_eq!(out_t.dims(), [l, q, w]);
        let out_data = out_t.clone().into_data().into_vec::<f32>().unwrap();
        for h in 0..l {
            for i in 0..q {
                for off in 0..w {
                    let j = gather_source_index(i * w + off, w, s);
                    let got = out_data[h * q * w + i * w + off];
                    let want = a_vec[h * q * s + i * s + j];
                    assert!(
                        (got - want).abs() < 1e-6,
                        "A'[{h},{i},{off}]={got} != A[{h},{i},{j}]={want}"
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
