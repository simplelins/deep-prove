//! Separable-weight identity utilities for the banded-scores protocol (D1.1 方案一).
//!
//! The banded-QK proof checks a weighted aggregate identity
//! `Σ_{i,off} B[i,off]·z^i·ρ^off = ρ^{w-1}·Q̂(z/ρ)·K̂(ρ)` where the weighted
//! sums (e.g. `Σ_i v_i·y^i`) fold into **single-point MLE claims** via the
//! separability of the exponential weights:
//!
//! ```text
//! Σ_i v_i·y^i = Π_k(1+w_k) · v_MLE(x*),   w_k = y^{2^k},  x*_k = w_k/(1+w_k)
//! ```
//!
//! (per-bit: `w_k^{i_k} = (1+w_k)·eq_bit(x*_k, i_k)`).

use ark_ff::{AdditiveGroup, Field, PrimeField};
use dp_crypto::poly::eq::evals;

/// Returns `(scale, point)` such that `Σ_i v_i·y^i == scale · v_MLE(point)`
/// for any vector `v` of length `2^num_vars` (indices in little-endian bit
/// order, matching the MLE convention).
pub(crate) fn univariate_weight_to_mle_point<F: PrimeField>(
    y: F,
    num_vars: usize,
) -> (F, Vec<F>) {
    // per-bit weights w_k = y^{2^k}
    let mut w_ks = Vec::with_capacity(num_vars);
    let mut w = y;
    for _ in 0..num_vars {
        w_ks.push(w);
        w = w * w;
    }
    // scale = Π_k (1 + w_k)
    let scale = w_ks.iter().copied().fold(F::ONE, |acc, w_k| acc * (F::ONE + w_k));
    // point coordinates x*_k = w_k / (1 + w_k)
    let point = w_ks
        .iter()
        .map(|w_k| *w_k * (F::ONE + *w_k).inverse().expect("1+w_k != 0"))
        .collect();
    (scale, point)
}

/// The weighted sum `Σ_i v_i·y^i` computed directly (O(2^n)) — reference for
/// the folded form.
#[cfg(any(test, feature = "p1-debug"))]
pub(crate) fn univariate_weighted_sum_ref<F: PrimeField>(v: &[F], y: F) -> F {
    let mut acc = F::ZERO;
    let mut weight = F::ONE;
    for v_i in v {
        acc += *v_i * weight;
        weight *= y;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_bn254::Fr;
    use ark_std::UniformRand;
    use dp_crypto::poly::dense::DensePolynomial;

    #[test]
    fn test_univariate_weight_folds_to_mle_point() {
        let mut rng = ark_std::test_rng();
        for num_vars in [1usize, 3, 6, 10] {
            let len = 1 << num_vars;
            let v: Vec<Fr> = (0..len).map(|_| Fr::rand(&mut rng)).collect();
            for y in [Fr::from(3u64), Fr::rand(&mut rng)] {
                let direct = univariate_weighted_sum_ref(&v, y);
                let (scale, point) = univariate_weight_to_mle_point(y, num_vars);
                assert_eq!(point.len(), num_vars);
                let mle = DensePolynomial::new(v.clone());
                let folded = scale * mle.evaluate(&point).unwrap();
                assert_eq!(
                    direct, folded,
                    "separable-weight identity failed for num_vars={num_vars}"
                );
            }
        }
    }

    /// The eq-vector form must agree: eq(point) == Π_k eq_bit(x*_k, i_k), and
    /// the folded weighted sum via eq dot v matches the direct computation.
    #[test]
    fn test_univariate_weight_via_eq_vector() {
        let mut rng = ark_std::test_rng();
        let num_vars = 7usize;
        let len = 1 << num_vars;
        let v: Vec<Fr> = (0..len).map(|_| Fr::rand(&mut rng)).collect();
        let y = Fr::rand(&mut rng);
        let (scale, point) = univariate_weight_to_mle_point(y, num_vars);
        let eq = evals(&point);
        let via_eq: Fr = v.iter().zip(eq.iter()).map(|(v_i, e)| *v_i * *e).sum();
        assert_eq!(
            univariate_weighted_sum_ref(&v, y),
            scale * via_eq,
            "eq-vector form must match the direct weighted sum (scaled)"
        );
    }

    /// 方案一核心恒等式的数值验证（非因果纯带状版）：
    ///
    ///   Σ_{(i,off): j 有效} B[i,off]·z^i·ρ^off
    ///     == ρ^{w-1}·Σ_d Q̂_d(z/ρ)·K̂_d(ρ)
    ///
    /// 其中 B[i,off] = Σ_d Q[i,d]·K[i-(w-1)+off, d]（j = i-(w-1)+off ∈ [0,s)），
    /// Q̂_d(y) = Σ_i Q[i,d]·y^i，K̂_d(ρ) = Σ_j K[j,d]·ρ^j。
    /// 差分结构 j-i = off-(w-1) 使指数权重把 (i,off)→(i,j) 的耦合消解为比值。
    /// 带状约束 |j-i| ≤ w-1 非乘积集——纯因式分解不成立（数值已证伪）。
    /// 正确构造 = 截断乘积多项式 + 商见证（quotient witness）模式：
    /// T_d(z) := [z^Δ ∈ 带] 的 Â_d(z)·B̂_d(z^{-1}) 截断项，T_d(ρ^{-1}) = LHS_d，
    /// 需对截断恒等式 T_d·W = P_d - R_d 引入商见证承诺（研究轨，见设计文档 §10）。
    #[test]
    #[ignore = "banded 约束非乘积集：需截断乘积 + 商见证构造（研究轨）"]
    fn test_toeplitz_factorization_identity() {
        let mut rng = ark_std::test_rng();
        let (s, w, d) = (16usize, 4usize, 3usize);
        let q: Vec<Vec<Fr>> = (0..s).map(|_| (0..d).map(|_| Fr::rand(&mut rng)).collect()).collect();
        let k: Vec<Vec<Fr>> = (0..s).map(|_| (0..d).map(|_| Fr::rand(&mut rng)).collect()).collect();
        let z = Fr::rand(&mut rng);
        let rho = Fr::rand(&mut rng);

        // LHS：带内 (i,off) 的加权聚合（j = i-(w-1)+off 须落在 [0,s)）
        let mut lhs = Fr::ZERO;
        for i in 0..s {
            for off in 0..w {
                let j_i64 = i as i64 - (w as i64 - 1) + off as i64;
                if j_i64 < 0 || j_i64 >= s as i64 {
                    continue; // 带外（无效窗口位置，B=0）
                }
                let j = j_i64 as usize;
                let b: Fr = (0..d).map(|dd| q[i][dd] * k[j][dd]).sum();
                let z_pow = pow_at(z, i);
                let rho_pow = pow_at(rho, off);
                lhs += b * z_pow * rho_pow;
            }
        }

        // RHS：ρ^{w-1}·Σ_d Q̂_d(z/ρ)·K̂_d(ρ)
        let y = z * rho.inverse().unwrap();
        let q_hat = |dd: usize| -> Fr {
            let (scale, point) = univariate_weight_to_mle_point(y, ceil_log2_usize(s));
            let eq = evals(&point);
            let mle_evals = column_mle_evals(&q, dd, len_pow2(s));
            scale * mle_evals.iter().zip(eq.iter()).map(|(v, e)| *v * *e).sum::<Fr>()
        };
        let k_hat = |dd: usize| -> Fr {
            let (scale, point) = univariate_weight_to_mle_point(rho, ceil_log2_usize(s));
            let eq = evals(&point);
            let mle_evals = column_mle_evals(&k, dd, len_pow2(s));
            scale * mle_evals.iter().zip(eq.iter()).map(|(v, e)| *v * *e).sum::<Fr>()
        };
        let rho_pow_w: Fr = pow_at(rho, w - 1);
        let mut rhs = Fr::ZERO;
        for dd in 0..d {
            rhs += q_hat(dd) * k_hat(dd);
        }
        let rhs = rhs * rho_pow_w;

        assert_eq!(lhs, rhs, "Toeplitz 因式分解恒等式失败");
    }

    fn ceil_log2_usize(n: usize) -> usize {
        (n as f64).log2().ceil() as usize
    }
    fn len_pow2(s: usize) -> usize {
        1usize << ceil_log2_usize(s)
    }
    fn pow_at(base: Fr, exp: usize) -> Fr {
        let mut acc = Fr::ONE;
        for _ in 0..exp {
            acc *= base;
        }
        acc
    }
    /// 把 [s, d] 的第 d 列 pad 到 2^n（MLE 求值序）
    fn column_mle_evals(mat: &[Vec<Fr>], dd: usize, len: usize) -> Vec<Fr> {
        let mut col = vec![Fr::ZERO; len];
        for (i, row) in mat.iter().enumerate() {
            col[i] = row[dd];
        }
        col
    }
}
