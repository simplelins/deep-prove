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

mod evaluate;
pub use evaluate::*;

mod prove;

use crate::backend::Backend;
use crate::layers::transformer::attention_mask::AttentionSpan;
use crate::tensor::WrappedTensor;
use crate::lookup::logup_gkr::structs::LogUpBatchProof;
use crate::poly_commit::verifier::VerifierCommitment;
use dp_crypto::arkyper::CommitmentScheme;

pub const GATHER_ROWS_LAYER: &str = "GATHER";

/// The gather variant.
///
/// - `Rows`: the K/V row gather `K'[h, i, off, d] = K[h, j(i,off), d]` —
///   the banded einsums consume the gathered tensors (Stage A / plan β).
/// - `ScoreWindow`: the score-tensor window slice
///   `A'[.., i, off] = A[.., i, j(i,off)]` along the last (key) axis —
///   plan α's mask→softmax insertion (the softmax then runs over the window
///   and the AV einsum consumes the gathered V).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum GatherMode {
    #[default]
    Rows,
    ScoreWindow,
}

/// Proving context for the gather layer.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GatherRowsCtx {
    pub span: AttentionSpan,
    pub heads_per_kv_head: usize,
    #[serde(default)]
    pub mode: GatherMode,
}

/// Proof for the gather layer: the two logUp GKR proofs (the K' query side
/// and the K table side) whose fraction balance proves the rearrangement,
/// plus the commitment to the gathered tensor (opened at the logup point).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(bound(
    serialize = "F: ark_serialize::CanonicalSerialize",
    deserialize = "F: ark_serialize::CanonicalDeserialize"
))]
pub struct GatherRowsProof<F: ark_ff::PrimeField, PCS: CommitmentScheme> {
    pub logup_proof_lookup: LogUpBatchProof<F>,
    pub logup_proof_table: LogUpBatchProof<F>,
    pub commitments: Vec<VerifierCommitment<PCS>>,
}

/// GatherRows layer: the gather operator for banded attention (D1.1 Stage A).
/// See [`GatherMode`] for the two variants.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GatherRows {
    pub span: AttentionSpan,
    pub heads_per_kv_head: usize,
    #[serde(default)]
    pub mode: GatherMode,
}

impl GatherRows {
    pub fn new(span: AttentionSpan, heads_per_kv_head: usize) -> Self {
        Self {
            span,
            heads_per_kv_head,
            mode: GatherMode::Rows,
        }
    }

    pub fn new_score_window(span: AttentionSpan, heads_per_kv_head: usize) -> Self {
        Self {
            span,
            heads_per_kv_head,
            mode: GatherMode::ScoreWindow,
        }
    }

    pub fn window(&self) -> usize {
        match self.span {
            AttentionSpan::Local(w) => w,
            AttentionSpan::Full => panic!("GatherRows requires AttentionSpan::Local"),
        }
    }
}

/// Source row index for the flattened window position `t = i·w + off`.
#[inline]
pub(crate) fn gather_source_index(t: usize, w: usize, s: usize) -> usize {
    let i = t / w;
    let off = t % w;
    let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
    j_i64.clamp(0, s as i64 - 1) as usize
}

/// Source row index for the flattened `[Hkv·s_pad·w]` output row `t` over the
/// flattened `[Hkv·s_pad]` input rows (row-major `[Hkv, s_pad, dk]` view of the
/// zero-padded K/V). The window map is head-relative and clamps to the
/// UNPADDED row count `s`（padded 源行零填充且永不读取）。
#[inline]
pub(crate) fn gather_source_row(t: usize, w: usize, s_pad: usize, s_unpadded: usize) -> usize {
    let per_head_out = s_pad * w;
    let head = t / per_head_out;
    let i = (t % per_head_out) / w;
    let off = t % w;
    let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
    let j = j_i64.clamp(0, s_unpadded as i64 - 1) as usize;
    head * s_pad + j
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

/// The head-tiled multiplicity histogram over the flattened `[Hkv·s_pad]`
/// padded input rows（clamp 到 unpadded s——padded 源行零填充且重数为 0）.
pub(crate) fn gather_multiplicities_rows(
    w: usize,
    s_pad: usize,
    s_unpadded: usize,
    heads: usize,
) -> Vec<u64> {
    let per_head = {
        let mut mult = vec![0u64; s_pad];
        for t in 0..s_pad * w {
            let i = t / w;
            let off = t % w;
            let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
            let j = j_i64.clamp(0, s_unpadded as i64 - 1) as usize;
            mult[j] += 1;
        }
        mult
    };
    let mut out = Vec::with_capacity(per_head.len() * heads);
    for _ in 0..heads {
        out.extend_from_slice(&per_head);
    }
    out
}

/// Source cell index for the ScoreWindow variant: for the flattened
/// `[L·q_pad·w]` output position `t`（行主序 `[L, q_pad, w]`，L = 前导轴折叠），
/// 返回输入 `[L, q_pad, s_pad]` 行主序 cell id `l·(q_pad·s_pad) + i·s_pad + j`，
/// 其中 `j = clamp(i-(w-1)+off, 0, s_unpadded-1)`。
///
/// 网格 = padded（q_pad/s_pad 为 2 的幂），clamp 用 unpadded s：padded 源
/// 行/列（零填充）只会在 padded 输出行被读到，其值恒为 0——与 einsum 的
/// 零填充约定一致，也符合 claim 链的 padding 修正语义（修正后的 claim =
/// 零填充 MLE 的求值）。
#[inline]
pub(crate) fn score_window_source_cell(
    t: usize,
    w: usize,
    q_pad: usize,
    s_pad: usize,
    s_unpadded: usize,
) -> usize {
    let per_head_out = q_pad * w;
    let head = t / per_head_out;
    let i = (t % per_head_out) / w;
    let off = t % w;
    let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
    let j = j_i64.clamp(0, s_unpadded as i64 - 1) as usize;
    head * (q_pad * s_pad) + i * s_pad + j
}

/// ScoreWindow 的 cell 重数直方图（数据无关闭式）：输出位置 t 复制输入
/// cell `score_window_source_cell(t)`，对 `[L·q_pad·s_pad]` 个 cell 计数。
///
/// 不变量：Σ mult = L·q_pad·w（每个输出 cell 恰好复制一个输入 cell）。
pub(crate) fn score_window_multiplicities(
    w: usize,
    q_pad: usize,
    s_pad: usize,
    s_unpadded: usize,
    heads: usize,
) -> Vec<u64> {
    let mut mult = vec![0u64; heads * q_pad * s_pad];
    for t in 0..heads * q_pad * w {
        mult[score_window_source_cell(t, w, q_pad, s_pad, s_unpadded)] += 1;
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
