//! Deepprove library
#![recursion_limit = "1024"]
#![feature(iter_next_chunk)]
#![feature(min_specialization)]
#![feature(exact_size_is_empty)]
#![feature(mapped_lock_guards)]
#![feature(associated_type_defaults)]

use std::{borrow::Borrow, env, ops::Deref, str::FromStr};

use anyhow::anyhow;
use ark_ff::PrimeField;
use ark_std::rand::{self, SeedableRng, rngs::StdRng};
use dp_crypto::arkyper::transcript::{Transcript, blake3::Blake3Transcript};
use itertools::Itertools;
use quantization::ToField;
use rayon::iter::ParallelIterator;
use serde::{Deserialize, Serialize};

mod backend;
mod fft;
pub mod graph;
pub mod inputs;
pub mod iop;
pub mod layers;
pub mod lookup;
pub mod measure;
pub mod model;
pub mod number;
pub mod padding;
pub mod parser;
pub mod poly_commit;
pub mod quantization;
pub mod shape;
pub mod tensor;
pub use crate::number::Number;
use crate::quantization::ToElement;

// Re-exports
pub use iop::{
    ChunkProof, Proof, ProverContext,
    claim::Claim,
    prover::Prover,
    verifier::{IO, verify, verify_streaming},
};
pub use quantization::{ScalingFactor, ScalingStrategy};
pub use shape::Shape;
pub use tensor::Tensor;

#[cfg(feature = "capture-layers-quant")]
pub mod capture;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(bound(
    serialize = "F: ark_serialize::CanonicalSerialize",
    deserialize = "F: ark_serialize::CanonicalDeserialize"
))]
pub struct SerializableField<F: PrimeField>(#[serde(with = "dp_crypto::serialization")] F);

impl<F: PrimeField> Deref for SerializableField<F> {
    type Target = F;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<F: PrimeField> ToField<F> for SerializableField<F> {
    fn to_field(&self) -> F {
        self.0
    }
}

impl<F: PrimeField> ToField<SerializableField<F>> for Element {
    fn to_field(&self) -> SerializableField<F> {
        SerializableField(self.to_field())
    }
}

impl<F: PrimeField> ToElement for SerializableField<F> {
    fn to_element(&self) -> Element {
        self.0.to_element()
    }
}

impl<F: PrimeField> From<F> for SerializableField<F> {
    fn from(value: F) -> Self {
        Self(value)
    }
}
#[cfg(test)]
mod testing;
pub(crate) mod util;

pub const GIT_VERSION: &str = git_version::git_version!(args = ["--abbrev=6", "--always"]);
pub fn version() -> String {
    GIT_VERSION.to_string()
}

/// We allow higher range to account for overflow. Since we do a requant after each layer, we
/// can support with i128 with 8 bits quant:
/// 16 + log(c) = 64 => c = 2^48 columns in a dense layer
pub type Element = i64;

/// Returns the default transcript the prover and verifier must instantiate to validate a proof.
pub fn default_transcript() -> Blake3Transcript {
    Blake3Transcript::new(b"m2vec")
}

/// Returns the bit sequence of num of bit_length length.
pub(crate) fn to_bit_sequence_le(
    num: usize,
    bit_length: usize,
) -> impl DoubleEndedIterator<Item = usize> {
    assert!(
        bit_length as u32 <= usize::BITS,
        "bit_length cannot exceed usize::BITS"
    );
    (0..bit_length).map(move |i| (num >> i) & 1)
}

/// Returns a 2^n-th root of unity for PrimeField `F`
pub fn get_root_of_unity<F: PrimeField>(n: usize) -> anyhow::Result<F> {
    F::get_root_of_unity(1 << n as u64).ok_or(anyhow!("Cannot compute 2^{n}-th root of unity"))
}

/// Evaluate the MLE of a rectangular "row ≥ col" zeroifier over the split points.
/// Row indices live over `row_point` (2^len bits), column indices over `col_point`.
/// Generalises the square-only [`eval_zeroifier_mle`]:
/// eval = Σ_c eq_col(c) · Σ_{row ≥ c} eq_row(row).
pub fn eval_row_ge_col_mle<F: PrimeField>(row_point: &[F], col_point: &[F]) -> F {
    let row_eqs = dp_crypto::poly::eq::evals(row_point);
    let col_eqs = dp_crypto::poly::eq::evals(col_point);
    // 行 eq 的后缀和 S(t) = Σ_{row ≥ t} eq_row(row)
    let mut suffix = vec![F::ZERO; row_eqs.len() + 1];
    for (i, v) in row_eqs.iter().enumerate().rev() {
        suffix[i] = suffix[i + 1] + *v;
    }
    col_eqs
        .iter()
        .take(suffix.len())
        .zip(suffix.iter())
        .fold(F::ZERO, |acc, (&c, &s)| acc + c * s)
}

/// Evaluate the MLE of the index indicator "index < threshold" over `point`.
/// eval = Σ_{v < threshold} eq(v),语义无位序歧义。
pub fn eval_lt_index_mle<F: PrimeField>(point: &[F], threshold: usize) -> F {
    let eqs = dp_crypto::poly::eq::evals(point);
    eqs.iter()
        .take(threshold.min(eqs.len()))
        .fold(F::ZERO, |acc, &v| acc + v)
}

/// Evaluate the MLE of a rectangular band "row < col + n" over the split points.
/// eval = Σ_c eq_col(c) · Σ_{row < c+n} eq_row(row)。
pub fn eval_band_mle<F: PrimeField>(row_point: &[F], col_point: &[F], n: usize) -> F {
    let row_eqs = dp_crypto::poly::eq::evals(row_point);
    let col_eqs = dp_crypto::poly::eq::evals(col_point);
    // 行 eq 的前缀和 P(t) = Σ_{row ≤ t} eq_row(row)（含端点）
    let mut prefix = vec![F::ZERO; row_eqs.len()];
    let mut acc = F::ZERO;
    for (i, v) in row_eqs.iter().enumerate() {
        acc += *v;
        prefix[i] = acc;
    }
    col_eqs
        .iter()
        .enumerate()
        .fold(F::ZERO, |sum, (c, &c_eq)| {
            let last = (c + n).min(row_eqs.len());
            if last == 0 {
                sum
            } else {
                sum + c_eq * prefix[last - 1]
            }
        })
}

/// Method to efficiency evaluate the MLE of the zeroifier matrix over a random
/// point. The point is provided already split between coordinates referring to the
/// columns and coordinates referring to the rows of the matrix.
/// Currently, it works only for a square zeroifier matrix
pub fn eval_zeroifier_mle<F: PrimeField>(column_point: &[F], row_point: &[F]) -> F {
    column_point
        .iter()
        .zip(row_point)
        .fold(F::ONE, |acc, (&c, &r)| {
            acc * (F::ONE - c - r + F::from(2) * c * r) + (F::ONE - c) * r
        })
}

/// Method to efficiency evaluate the MLE of the infinitizer matrix over a random
/// point. The point is provided already split between coordinates referring to the
/// columns and coordinates referring to the rows of the matrix.
/// Currently, it works only for a square infinitizer matrix
pub fn eval_infinitizer_mle<F: PrimeField>(
    column_point: &[F],
    row_point: &[F],
    minus_infinity: Element,
) -> F {
    <Element as ToField<F>>::to_field(&minus_infinity)
        * (F::ONE - eval_zeroifier_mle(column_point, row_point))
}
#[allow(dead_code)]
pub(crate) fn try_unzip<I, C, T, E>(iter: I) -> Result<C, E>
where
    I: IntoIterator<Item = Result<T, E>>,
    C: Extend<T> + Default,
{
    iter.into_iter().try_fold(C::default(), |mut c, r| {
        c.extend([r?]);
        Ok(c)
    })
}
#[allow(dead_code)]
pub(crate) fn try_unzip_parallel<I, C, T, E>(iter: I) -> Result<C, E>
where
    I: ParallelIterator<Item = Result<T, E>>,
    C: Extend<T> + Default + Send,
    E: Send,
    T: Send,
{
    // ToDo: remove need to collect into vector first
    let v = iter.collect::<Vec<_>>();
    try_unzip(v)
}

pub trait VectorTranscript<F: PrimeField> {
    fn read_challenges(&mut self, n: usize) -> Vec<F>;
}

impl<T: Transcript, F: PrimeField> VectorTranscript<F> for T {
    fn read_challenges(&mut self, n: usize) -> Vec<F> {
        (0..n).map(|_| self.challenge_scalar()).collect_vec()
    }
}

pub trait InitTranscript: Clone {
    type InitData: Default + From<&'static [u8]>;

    fn new(init_data: Self::InitData) -> Self;
}

impl InitTranscript for Blake3Transcript {
    type InitData = &'static [u8];

    fn new(init_data: Self::InitData) -> Self {
        Self::new(init_data)
    }
}

pub fn argmax<T: PartialOrd>(v: &[T]) -> Option<usize> {
    if v.is_empty() {
        return None;
    }

    let mut max_index = 0;
    let mut max_value = &v[0];

    for (i, value) in v.iter().enumerate().skip(1) {
        // Only update if strictly greater, ensuring we take the first maximum in ties
        if value > max_value {
            max_index = i;
            max_value = value;
        }
    }

    Some(max_index)
}

// glibc：将空闲堆内存归还 OS。context 生成（权重 Fr 化）峰值数百 GB 后，
// glibc 默认滞留已释放内存，后续阶段（trace + 证明）叠加会提前触顶 OOM。
#[cfg(target_os = "linux")]
pub fn trim_heap() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe { malloc_trim(0) };
}

#[cfg(not(target_os = "linux"))]
pub fn trim_heap() {}

/// Converts an iterator of elements to the extension field.
pub(crate) fn to_field<T, E, I>(iter: I) -> Vec<E>
where
    I: IntoIterator,
    I::Item: Borrow<T>,
    T: ToField<E>,
{
    iter.into_iter().map(|v| v.borrow().to_field()).collect()
}

pub trait NextPowerOfTwo {
    /// Returns a new vector where each element is the next power of two.
    fn next_power_of_two(&self) -> Self;
}

// For unsigned integer vectors
impl NextPowerOfTwo for Vec<usize> {
    fn next_power_of_two(&self) -> Self {
        self.iter().map(|&i| i.next_power_of_two()).collect()
    }
}

impl NextPowerOfTwo for Shape {
    fn next_power_of_two(&self) -> Self {
        Shape::new(self.deref().next_power_of_two())
    }
}

impl NextPowerOfTwo for Vec<Shape> {
    fn next_power_of_two(&self) -> Self {
        self.iter().map(|el| el.next_power_of_two()).collect()
    }
}

#[cfg(test)]
static INIT: std::sync::Once = std::sync::Once::new();

#[cfg(test)]
pub fn init_test_logging_default() {
    use tracing_subscriber::EnvFilter;

    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::fmt().with_env_filter(filter).init();
    });
}

#[cfg(test)]
pub fn init_test_logging(default_level: &str) {
    use tracing_subscriber::EnvFilter;

    INIT.call_once(|| {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
        tracing_subscriber::fmt().with_env_filter(filter).init();
    });
}

/// Get a rng generator from a seed from env var or generate a random one
pub fn rng_from_env_or_random() -> StdRng {
    let seed = seed_from_env_or_rng();
    StdRng::seed_from_u64(seed)
}

/// Get a seed from env var or generate a random one
pub fn seed_from_env_or_rng() -> u64 {
    env::var("RNG_SEED")
        .map(|val| u64::from_str(&val).expect("RNG_SEED must be a u64"))
        .unwrap_or_else(|_| rand::random::<u64>())
}

#[cfg(test)]
mod test {
    #[test]
    fn test_dense_var_order_probe() {
        use ark_bn254::Fr;
        use dp_crypto::poly::dense::DensePolynomial;
        // poly = 指示函数 δ_{index=1}
        let mle = DensePolynomial::new(vec![
            Fr::from(0u64),
            Fr::from(1u64),
            Fr::from(0u64),
            Fr::from(0u64),
        ]);
        let r = vec![Fr::from(3u64), Fr::from(5u64)];
        let e = mle.evaluate(&r).unwrap();
        // 约定 A：变量0 ↔ 索引 MSB → 索引1 = (x0=0,x1=1) → (1−r0)·r1
        let a = (<Fr as ark_ff::AdditiveGroup>::ZERO - r[0]) * r[1];
        // 约定 B：变量0 ↔ 索引 LSB → 索引1 = (x0=1,x1=0) → r0·(1−r1)
        let b = r[0] * (<Fr as ark_ff::AdditiveGroup>::ZERO - r[1]);
        eprintln!("eval={:?} convA_msb={:?} convB_lsb={:?}", e, a, b);
        // 手工 Σ eq(r,i)·z[i]，eq 用 dp-crypto evals（r[0] ↔ 索引 MSB）
        let chis = dp_crypto::poly::eq::evals(&r);
        let zero = Fr::from(0u64);
        let manual: Fr = chis
            .iter()
            .zip(mle.evals())
            .map(|(c, z)| *c * z)
            .reduce(|a, b| a + b)
            .unwrap_or(zero);
        eprintln!("manual_dot={:?} matches_eval={}", manual, manual == e);
        // 约定 B 修正版：r0·(1−r1)
        let b2 = r[0] * (Fr::from(1u64) - r[1]);
        eprintln!("convB2={:?}", b2);
    }

    #[test]
    fn test_eq_convention_probe() {
        use ark_bn254::Fr;
        use ark_ff::{AdditiveGroup, PrimeField};
        use dp_crypto::poly::eq::evals;
        // 4x4 grid: poly[row*4+col] = [row >= col]
        let mut poly = vec![Fr::ZERO; 16];
        for row in 0..4usize {
            for col in 0..4usize {
                poly[row * 4 + col] = if row >= col { Fr::from(1u64) } else { Fr::from(0u64) };
            }
        }
        let mle = dp_crypto::poly::dense::DensePolynomial::new(poly);
        let r_row = vec![Fr::from(2u64), Fr::from(0u64)];
        let r_col = vec![Fr::from(1u64), Fr::from(1u64)];
        let point = [r_col.clone(), r_row.clone()].concat();
        let true_eval = mle.evaluate(&point).unwrap();
        let old = crate::eval_zeroifier_mle(&r_col, &r_row);
        let new = crate::eval_row_ge_col_mle(&r_row, &r_col);
        let new_swapped = crate::eval_row_ge_col_mle(&r_col, &r_row);
        eprintln!("true={:?} old={:?} new={:?} new_swapped={:?}", true_eval, old, new, new_swapped);
        assert_eq!(true_eval, old, "old fold (col,row)");
        assert_eq!(true_eval, new, "suffix(row,col)");
    }

    use ark_std::rand::Rng;
    use dp_crypto::{IntoMLE, arkyper::transcript::blake3::Blake3Transcript};
    use itertools::Itertools;
    use tenstore::GenStore;

    use crate::{
        iop::{prover::Prover, verifier::verify},
        parser::onnx::FloatOnnxLoader,
        rng_from_env_or_random,
        tensor::Tensor,
        testing::{Pcs, random_field_vector},
        to_bit_sequence_le,
    };

    type F = ark_bn254::Fr;
    type T = Blake3Transcript;

    type P<'a, 'b> = Prover<'a, 'b, F, T, Pcs>;

    #[test]
    fn test_model_run() -> anyhow::Result<()> {
        test_model_run_helper()?;
        Ok(())
    }

    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        PathBuf::from(manifest_dir).parent().unwrap().to_path_buf()
    }

    fn test_model_run_helper() -> anyhow::Result<()> {
        let filepath = workspace_root().join("zkml/assets/model.onnx");
        let (model, _md) = FloatOnnxLoader::new(&filepath.to_string_lossy()).build()?;

        println!("[+] Loaded onnx file");
        let (prover_ctx, verifier_ctx) = model
            .generate_contexts::<F, Pcs>()
            .expect("Unable to generate contexts");
        println!("[+] Setup parameters");

        let shapes = model.input_shapes();
        assert_eq!(shapes.len(), 1);
        let shape = &shapes[0];
        assert_eq!(shape.len(), 1);
        let input = Tensor::random(&vec![shape[0]].into());
        println!("input: {:?}", input.data());
        let inputs = model.prepare_inputs(vec![input])?;

        let trace = model.run(inputs, &mut GenStore::default()).unwrap();

        let output = trace.outputs().first().unwrap();
        println!("[+] Run inference. Result: {output:?}");

        println!("[+] Run prover");
        let (proof, io) = P::prove(&prover_ctx, trace, &model).expect("unable to generate proof");

        verify::<_, T, _>(&verifier_ctx, proof, io).expect("invalid proof");
        println!("[+] Verify proof: valid");
        Ok(())
    }

    // TODO: move below code to a vector module

    #[test]
    fn test_vector_mle() {
        let n = 10_usize.next_power_of_two();
        let v = random_field_vector::<F>(n);
        let mle = v.clone().into_mle();
        let random_index = rng_from_env_or_random().gen_range(0..v.len());
        let eval = to_bit_sequence_le(random_index, v.len().next_power_of_two().ilog2() as usize)
            .map(|b| F::from(b as u64))
            .collect_vec();
        let output = mle.evaluate(&eval).unwrap();

        assert_eq!(output, v[random_index]);
    }
}
