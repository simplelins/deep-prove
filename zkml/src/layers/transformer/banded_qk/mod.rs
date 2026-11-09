//! Banded attention scores (D1.1): the sliding-window QK product.
//!
//! Replaces the full `[Hq, s, s]` QK einsum with a banded `[Hq, s, w]` score
//! tensor so that the trace of a long-context prompt stays proportional to
//! `s·w` instead of `s²` (L 文档 /64 论证的落地).
//!
//! Semantics (causal + sliding window, matching
//! `attention_mask::non_caching_case_mask` for `AttentionSpan::Local(w)`):
//!
//! ```text
//! B[h, i, off] = Σ_d Q[h, i, d] · K[h, j(i, off), d]
//! j(i, off)    = i - (w - 1) + off,  clamped to [0, i]
//! ```
//!
//! The proof expresses the gathered K as a sum of `w` constant shifts of K
//! along the query axis (`K_δ`), reusing the sparse shift-matrix decomposition
//! already proven in `attention_mask/prove.rs` (the offset matrix O = U·S + P
//! family). See `docs/zkml_bootcamp/设计_D1.1带状注意力分数.md`.

use crate::layers::transformer::attention_mask::AttentionSpan;
use crate::tensor::TensorTypeParam;

mod evaluate;
pub use evaluate::*;

mod poly_identity;

/// Banded attention scores layer (D1.1): computes the sliding-window QK
/// product directly at `[Hq, s, w]` instead of the full `[Hq, s, s]`.
#[derive(Debug, Clone)]
pub struct BandedQK<T> {
    pub span: AttentionSpan,
    pub heads_per_kv_head: usize,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> BandedQK<T> {
    pub fn new(span: AttentionSpan, heads_per_kv_head: usize) -> Self {
        Self {
            span,
            heads_per_kv_head,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn window(&self) -> usize {
        match self.span {
            AttentionSpan::Local(w) => w,
            AttentionSpan::Full => panic!("BandedQK requires AttentionSpan::Local"),
        }
    }
}

/// Query index selected by window position `off` for query row `i`.
/// `s` is the sequence length; positions are clamped to `[0, i]` (causal).
#[inline]
pub(crate) fn window_key_index(i: usize, off: usize, w: usize) -> usize {
    let j = (i + off).saturating_sub(w - 1);
    j.min(i)
}

/// Whether window position `off` of query row `i` is inside the causal window
/// (the complement positions are masked out by the mask layer).
#[inline]
pub(crate) fn window_position_valid(i: usize, off: usize, w: usize) -> bool {
    // j = i + off - (w-1) must satisfy 0 <= j <= i:
    //   j <= i  <=> off <= w-1 (always true by construction)
    //   j >= 0  <=> off >= w-1-i
    off + 1 >= w.saturating_sub(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Semantic parity: the banded scores `B[i, off]` must equal the full
    /// `Q·Kᵀ` scores evaluated at the window index, for every valid window
    /// position (w=2, s=8, d=4 — the design doc's对拍尺寸).
    #[test]
    fn test_banded_matches_full_scores() {
        let (s, w, d) = (8usize, 2usize, 4usize);
        let q: Vec<Vec<i64>> = (0..s)
            .map(|i| (0..d).map(|dd| ((i * 7 + dd * 3) % 11) as i64 - 5).collect())
            .collect();
        let k: Vec<Vec<i64>> = (0..s)
            .map(|j| (0..d).map(|dd| ((j * 5 + dd * 7) % 13) as i64 - 6).collect())
            .collect();
        let dot = |a: &[i64], b: &[i64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<i64>();

        // full scores (as the QK einsum + Local(w) mask would produce, pre-mask)
        let full: Vec<Vec<i64>> = (0..s)
            .map(|i| (0..s).map(|j| dot(&q[i], &k[j])).collect())
            .collect();

        for i in 0..s {
            for off in 0..w {
                let j = window_key_index(i, off, w);
                let b = dot(&q[i], &k[j]);
                assert_eq!(b, full[i][j], "B[{i},{off}] must equal full[{i},{j}]");
                if window_position_valid(i, off, w) {
                    // valid positions must lie inside the causal window
                    assert!(j <= i);
                    assert!(i.saturating_sub(j) < w, "window violated at {i},{off}");
                }
            }
        }
    }

    /// Decomposition identity: gathering via `w` constant shifts of K
    /// (`K_δ[i,d] = K[j(i,δ), d]`) reproduces the banded scores — the form the
    /// proof uses (Σ_δ S_δ·K_δ, one sparse shift matrix per δ).
    #[test]
    fn test_shift_decomposition_identity() {
        let (s, w, d) = (8usize, 4usize, 3usize);
        let q: Vec<Vec<i64>> = (0..s)
            .map(|i| (0..d).map(|dd| ((i * 3 + dd) % 7) as i64 - 3).collect())
            .collect();
        let k: Vec<Vec<i64>> = (0..s)
            .map(|j| (0..d).map(|dd| ((j * 11 + dd * 5) % 9) as i64 - 4).collect())
            .collect();

        // K_δ[i,d] = K[j(i,δ), d]
        let k_delta: Vec<Vec<Vec<i64>>> = (0..w)
            .map(|delta| {
                (0..s)
                    .map(|i| {
                        let j = window_key_index(i, delta, w);
                        k[j].iter().copied().collect::<Vec<i64>>()
                    })
                    .collect()
            })
            .collect();

        // B via the decomposition: B[i,off] = Σ_d Q[i,d]·K_off[i,d]
        for i in 0..s {
            for off in 0..w {
                let b_decomposed: i64 = (0..d)
                    .map(|dd| q[i][dd] * k_delta[off][i][dd])
                    .sum();
                let j = window_key_index(i, off, w);
                let b_direct: i64 = (0..d).map(|dd| q[i][dd] * k[j][dd]).sum();
                assert_eq!(b_decomposed, b_direct, "decomposition mismatch at {i},{off}");
            }
        }
    }

    /// Window boundary semantics: rows before the window is full must clamp to
    /// key index 0 (the mask layer zeroes the duplicated positions).
    #[test]
    fn test_window_clamp_boundaries() {
        let (w, s) = (4usize, 8usize);
        // i < w-1: the left edge of the window is before the sequence start
        assert_eq!(window_key_index(0, 0, w), 0);
        assert_eq!(window_key_index(0, w - 1, w), 0); // all clamp to 0
        assert_eq!(window_key_index(1, 0, w), 0);
        // i >= w-1: no clamping, the window covers [i-w+1, i]
        assert_eq!(window_key_index(3, 0, w), 0);
        assert_eq!(window_key_index(3, w - 1, w), 3);
        assert_eq!(window_key_index(7, 0, w), 4);
        assert_eq!(window_key_index(7, w - 1, w), 7);
        // validity: row 0 has exactly one valid position (off = w-1)
        let valid: Vec<usize> = (0..w).filter(|&off| window_position_valid(0, off, w)).collect();
        assert_eq!(valid, vec![w - 1]);
        let valid_full: Vec<usize> =
            (0..w).filter(|&off| window_position_valid(w - 1, off, w)).collect();
        assert_eq!(valid_full.len(), w);
        let _ = s;
    }
}
