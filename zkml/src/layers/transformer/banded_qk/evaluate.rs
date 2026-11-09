//! Code for evaluating a banded attention scores layer (D1.1).
//!
//! The band is computed with `w` shifted elementwise dot-products — each
//! `O(s·d)` — so the full `[Hq, s, s]` score matrix is never materialized.

use anyhow::{bail, ensure, Result};
use burn::tensor::{Tensor as BTensor, TensorData};

use super::*;
use crate::backend::Backend;
use crate::tensor::WrappedTensor;

impl<T> BandedQK<T>
where
    T: TensorTypeParam,
{
    /// Computes the banded scores from Q and K.
    ///
    /// Inputs: `q` `[Hq, s, dk]` and `k` `[Hkv, s, dk]` (GQA: k is broadcast
    /// along the head dim). Output: `b` `[Hq, s, w]` with
    /// `b[.., i, off] = Σ_d q[.., i, d]·k[.., i-δ, d]` where `δ = w-1-off`
    /// and rows `i < δ` are zeroed (invalid window positions, masked later).
    pub(crate) fn evaluate_internal(
        &self,
        inputs: &[&WrappedTensor<T>],
    ) -> Result<WrappedTensor<T>> {
        ensure!(
            inputs.len() == 2,
            "BandedQK expects exactly two inputs (Q, K), got {}",
            inputs.len()
        );
        let w = self.window();
        let (q, q_unpadded) = match inputs[0] {
            WrappedTensor::Rank3 {
                tensor,
                unpadded_shape,
            } => (tensor, unpadded_shape),
            _ => bail!("BandedQK expects a rank-3 Q input"),
        };
        let (k, _) = match inputs[1] {
            WrappedTensor::Rank3 {
                tensor,
                unpadded_shape,
            } => (tensor, unpadded_shape),
            _ => bail!("BandedQK expects a rank-3 K input"),
        };
        let [hq, s, dk] = q.dims();
        let [hkv, s_k, dk_k] = k.dims();
        ensure!(s == s_k && dk == dk_k, "Q/K shape mismatch");
        ensure!(
            hq == hkv * self.heads_per_kv_head,
            "GQA head counts mismatch: hq={hq}, hkv={hkv}, hpk={}",
            self.heads_per_kv_head
        );

        // Broadcast K along the head dim for GQA (no-op for MHA, hpk=1)
        let k_full = if self.heads_per_kv_head == 1 {
            k.clone()
        } else {
            k.clone().repeat(&[self.heads_per_kv_head])
        };

        // The device for fresh tensors
        let device = q.device();

        // For each window offset: shift K up by δ=w-1-off (pad-top with zeros,
        // clamp semantics: rows i<δ are invalid and masked later) and take the
        // elementwise dot product with Q along d.
        let mut bands = Vec::with_capacity(w);
        for off in 0..w {
            let delta = w - 1 - off;
            let k_shift: BTensor<Backend, 3, T::Kind> = if delta == 0 {
                k_full.clone()
            } else if delta >= s {
                BTensor::<Backend, 3, T::Kind>::zeros([hq, s, dk], &device)
            } else {
                let rows = s - delta;
                let zeros =
                    BTensor::<Backend, 3, T::Kind>::zeros([hq, delta, dk], &device);
                let kept = k_full.clone().slice([0..hq, 0..rows, 0..dk]);
                BTensor::cat(vec![zeros, kept], 1)
            };
            let prod = q.clone().mul(k_shift);
            // Σ over d → [hq, s, 1]（burn 的 sum_dim 保秩，和的维度变 1）
            let b_off: BTensor<Backend, 3, T::Kind> = prod.sum_dim(2);
            bands.push(b_off);
        }
        // Concatenate along the window dim → [hq, s, w]
        let b = BTensor::cat(bands, 2);
        Ok(WrappedTensor::Rank3 {
            tensor: b,
            unpadded_shape: q_unpadded.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::transformer::attention_mask::AttentionSpan;

    type FKind = <f32 as TensorTypeParam>::Kind;

    /// BandedQK must reproduce the full masked attention scores at the window
    /// positions (numeric parity against the naive [s, s] baseline).
    #[test]
    fn test_banded_evaluate_matches_full() -> Result<()> {
        let (hq, s, dk, w) = (2usize, 8usize, 4usize, 3usize);
        let q_vec: Vec<f32> = [
            [0.5, -1.0, 2.0, 0.25],
            [1.5, 0.0, -0.5, 1.0],
            [0.0, 1.0, 1.0, -1.0],
            [2.0, 0.5, 0.0, 0.5],
            [-0.5, 1.5, 1.0, 0.0],
            [1.0, 1.0, -1.0, 2.0],
            [0.25, 0.75, 0.5, -0.25],
            [-1.0, 0.5, 2.0, 1.5],
        ]
        .concat();
        let k_vec: Vec<f32> = [
            [1.0, 0.0, -1.0, 0.5],
            [0.5, 1.0, 0.0, -0.5],
            [-1.0, 0.5, 1.0, 0.0],
            [0.0, -0.5, 0.5, 1.0],
            [1.5, 0.0, 0.5, -1.0],
            [0.0, 1.5, -0.5, 0.5],
            [0.5, -1.0, 1.5, 0.0],
            [1.0, 1.0, 0.0, 1.0],
        ]
        .concat();
        let q = BTensor::<Backend, 3, FKind>::from_data(
            TensorData::new(q_vec.clone(), burn::prelude::Shape::new([1, s, dk])).convert::<f32>(),
            &Default::default(),
        )
        .repeat(&[hq]);
        let k = BTensor::<Backend, 3, FKind>::from_data(
            TensorData::new(k_vec.clone(), burn::prelude::Shape::new([1, s, dk])).convert::<f32>(),
            &Default::default(),
        )
        .repeat(&[hq]);

        let layer: BandedQK<f32> = BandedQK::new(AttentionSpan::Local(w), 1);
        let q_w = WrappedTensor::<f32>::Rank3 {
            tensor: q.clone(),
            unpadded_shape: burn::prelude::Shape::new([hq, s, dk]),
        };
        let k_w = WrappedTensor::<f32>::Rank3 {
            tensor: k.clone(),
            unpadded_shape: burn::prelude::Shape::new([hq, s, dk]),
        };
        let b = layer.evaluate_internal(&[&q_w, &k_w])?;
        let WrappedTensor::Rank3 {
            tensor: b_t,
            unpadded_shape: _,
        } = &b
        else {
            bail!("expected rank-3 banded scores");
        };
        assert_eq!(b_t.dims(), [hq, s, w]);

        // Naive baseline: full QKᵀ, sliced at the window indices
        let full = q
            .clone()
            .matmul(k.clone().transpose())
            .into_data()
            .into_vec::<f32>()
            .unwrap();
        let full: Vec<Vec<Vec<f32>>> = {
            let data = full;
            let mut acc = vec![vec![vec![0.0; s]; s]; hq];
            for h in 0..hq {
                for i in 0..s {
                    for j in 0..s {
                        acc[h][i][j] = data[h * s * s + i * s + j];
                    }
                }
            }
            acc
        };

        let b_data = b_t.clone().into_data().into_vec::<f32>().unwrap();
        for h in 0..hq {
            for i in 0..s {
                for off in 0..w {
                    let got = b_data[h * s * w + i * w + off];
                    if window_position_valid(i, off, w) {
                        // 有效位置：必须与全量分数在窗口索引处一致
                        let j = window_key_index(i, off, w);
                        let want = full[h][i][j];
                        assert!(
                            (got - want).abs() < 1e-5,
                            "B[{h},{i},{off}]={got} != full[{h},{i},{j}]={want}"
                        );
                    } else {
                        // 无效位置：按约定填 0（mask 层置 -inf 后被 softmax 忽略）
                        assert_eq!(got, 0.0, "invalid position B[{h},{i},{off}]={got}, expect 0");
                    }
                }
            }
        }
        Ok(())
    }
}
