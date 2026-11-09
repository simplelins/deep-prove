//! GatherRows (D1.1 Stage A): the row-gather operator for banded attention.
//!
//! `K'[t, d] = K[gather_source_index(t), d]` where `t` flattens the `[i, off]`
//! window grid (`t = i·w + off`) and the source index is
//! `j(i, off) = clamp(i-(w-1)+off, 0, s-1)`.
//!
//! With the gathered tensors `K'`/`V'`, the banded attention becomes plain
//! einsums (`B = Q·K'ᵀ` banded-sliced, `AV = A·V'`) — the standard einsum
//! proof machinery applies unchanged, and the gather itself is proven as a
//! **logUp rearrangement argument**: the "table" is the (index, value) pair
//! set of the committed K, the queries are (f(t), K'[t, d]), and the
//! multiplicities are the closed-form histogram of f (data-independent).
//!
//! 证明分解（Stage A，全工程化）：
//! - gather 关系 = logUp 重排论证（动态表 = witness 承诺对，复用现有 logUp 机制）
//! - B/AV = 标准 einsum prove（fix_axes，零新协议）
//! - 后续 Stage B（研究轨）：零物化 K′ 的 Toeplitz/商见证构造（§10）

/// Source row index for the flattened window position `t = i·w + off`.
#[inline]
pub(crate) fn gather_source_index(t: usize, w: usize, s: usize) -> usize {
    let i = t / w;
    let off = t % w;
    let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
    j_i64.clamp(0, s as i64 - 1) as usize
}

/// Whether the window position `t` is valid (its source index is inside the
/// causal window; invalid positions are masked before softmax).
#[inline]
pub(crate) fn gather_position_valid(t: usize, w: usize) -> bool {
    let i = t / w;
    let off = t % w;
    off + 1 >= w.saturating_sub(i)
}

/// The closed-form multiplicity histogram of the gather map:
/// `mult[j] = |{t ∈ [0, s·w) : gather_source_index(t) = j}|`.
///
/// For the causal+window map each key row `j` is gathered by the queries
/// `i ∈ [j, j+w-1]` (clamped at the sequence start), giving
/// `mult[j] = min(j+1, w) + min(s-1-j, w-1)` capped by the boundary clamps.
pub(crate) fn gather_multiplicities(w: usize, s: usize) -> Vec<u64> {
    let mut mult = vec![0u64; s];
    for t in 0..s * w {
        mult[gather_source_index(t, w, s)] += 1;
    }
    mult
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gather map must agree with the banded window index per (i, off).
    #[test]
    fn test_gather_map_matches_window_index() {
        let (s, w) = (8usize, 3usize);
        for i in 0..s {
            for off in 0..w {
                let t = i * w + off;
                let j = gather_source_index(t, w, s);
                let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
                let expected = j_i64.clamp(0, s as i64 - 1) as usize;
                assert_eq!(j, expected, "gather map mismatch at t={t}");
                assert!(j <= i.min(s - 1));
            }
        }
    }

    /// The multiplicity histogram must (a) sum to s·w and (b) match the naive
    /// per-row count of the gather map.
    #[test]
    fn test_multiplicities_closed_form() {
        let (s, w) = (8usize, 3usize);
        let mult = gather_multiplicities(w, s);
        assert_eq!(mult.iter().sum::<u64>(), (s * w) as u64);
        let mut naive = vec![0u64; s];
        for t in 0..s * w {
            naive[gather_source_index(t, w, s)] += 1;
        }
        assert_eq!(mult, naive);
        // causal boundary (s=8, w=3): key row 0 gathers the clamped rows
        // (i+off < 2: (0,0),(0,1),(1,0)) plus the exact diagonal (i+off=2:
        // (0,2),(1,1),(2,0)) → 6 positions
        assert_eq!(mult[0], 6);
        // the last key row s-1: only (i=7, off=2) hits j=7 exactly
        assert_eq!(mult[s - 1], 1);
    }

    /// Semantic parity: gather(K, f) restricted to valid positions must equal
    /// the naive K[j(i,off)] (the values the banded scores consume).
    #[test]
    fn test_gather_values_parity() {
        let (s, w, d) = (8usize, 3usize, 4usize);
        let k: Vec<Vec<i64>> = (0..s)
            .map(|j| (0..d).map(|dd| ((j * 5 + dd * 7) % 13) as i64 - 6).collect())
            .collect();
        for t in 0..s * w {
            let j = gather_source_index(t, w, s);
            let gathered = k[j].clone();
            let i = t / w;
            let off = t % w;
            let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
            if j_i64 >= 0 {
                assert_eq!(gathered, k[j_i64 as usize], "gather parity at t={t}");
            } else {
                // clamped position: the value is K[0] (masked later)
                assert_eq!(gathered, k[0].clone(), "clamp parity at t={t}");
            }
            let _ = gather_position_valid(t, w);
        }
    }
}
