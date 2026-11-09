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

use ark_ff::PrimeField;
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
}
