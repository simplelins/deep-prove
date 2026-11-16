//! Code for proving an attention mask layer
use anyhow::anyhow;

use dp_crypto::{
    IntoMLE,
    poly::{dense::DensePolynomial, eq::evals},
    structs::IOPProverState,
    util::{ceil_log2, optimal_sumcheck_threads},
    virtual_polys::VirtualPolynomialsBuilder,
};

use super::*;

fn fix_mle_rec<F: PrimeField, const FIX_ROW: bool>(
    evals: &mut Vec<F>,
    point: &[F],
    acc: F,
    eval_index: usize,
    num_bits: usize,
) {
    if point.len() == num_bits {
        // we got to the end, so we set acc to the proper evals
        evals[eval_index] = acc;
        return;
    }
    let c = point[num_bits];
    let update_acc = |new_bit| {
        if FIX_ROW {
            acc * (F::ONE - new_bit - c + F::from(2) * new_bit * c) + (F::ONE - new_bit) * c
        } else {
            acc * (F::ONE - c - new_bit + F::from(2) * c * new_bit) + (F::ONE - c) * new_bit
        }
    };
    fix_mle_rec::<_, FIX_ROW>(evals, point, update_acc(F::ZERO), eval_index, num_bits + 1);
    fix_mle_rec::<_, FIX_ROW>(
        evals,
        point,
        update_acc(F::ONE),
        eval_index | (1 << num_bits),
        num_bits + 1,
    );
}

/// Optimized algorithm to fix the row variables of the MLE for the lower triangular matrix L
#[cfg(test)]
pub(crate) fn fix_lower_mle_rows<F: PrimeField>(row_point: &[F]) -> Vec<F> {
    let mut evals = vec![F::ZERO; 1 << row_point.len()];
    fix_mle_rec::<F, true>(&mut evals, row_point, F::ONE, 0, 0);
    evals
}

/// Optimized algorithm to fix the column variables of the MLE for the lower triangular matrix L
pub(crate) fn fix_lower_mle_columns<F: PrimeField>(column_point: &[F]) -> Vec<F> {
    let mut evals = vec![F::ZERO; 1 << column_point.len()];
    fix_mle_rec::<F, false>(&mut evals, column_point, F::ONE, 0, 0);
    evals
}

impl AttentionMask<Element> {
    pub(crate) fn prove_internal<F, PCS, T>(
        &self,
        _ctx: &AttentionMaskCtx,
        mask_proving_data: MaskProvingData<F>,
        unpadded_rows_len: usize,
        unpadded_seq_len: usize,
        prover: &mut Prover<F, T, PCS>,
    ) -> Result<(AttentionMaskProof<F>, Vec<Claim<F>>)>
    where
        F: PrimeField,
        PCS: CommitmentScheme<Field = F>,
        T: Transcript,
    {
        let MaskProvingData {
            batching_challenges,
            batching_point,
            eq_evals,
            input_polys,
            input_shape,
        } = mask_proving_data;
        let num_vars = ceil_log2(eq_evals.len());
        let eq_poly = DensePolynomial::new(eq_evals);
        // 矩形网格 [rows_pad × cols_pad]（α 角掩码 [q, w] 等）：mask/lt 多项式
        // 按输入实际 padded 网格构建，与 eq/输入多项式网格一致。
        let rows_pad = input_shape.dim(-2);
        let cols_pad = input_shape.dim(-1);
        debug_assert_eq!(ceil_log2(rows_pad) + ceil_log2(cols_pad), num_vars);
        let mask_polys = &self.make_mask_polys(rows_pad, cols_pad);
        // These polys are used so that the sumcheck only takes into account the portions of the mask poly corresponding to non-padded areas
        let (row_lt_poly, column_lt_poly) =
            self.make_row_column_lt_polys::<F>(unpadded_rows_len, unpadded_seq_len, rows_pad, cols_pad);
        #[cfg(feature = "alpha-debug-shapes")]
        eprintln!(
            "[mask-debug] input_shape={input_shape:?} unpadded_seq_len={unpadded_seq_len} eq_evals={} num_vars={num_vars} mask_vars={:?} lt_vars=({}, {}) input_raw_lens={:?}",
            eq_poly.evals().len(),
            mask_polys.iter().map(|p| p.num_vars()).collect::<Vec<_>>(),
            row_lt_poly.num_vars(),
            column_lt_poly.num_vars(),
            input_polys.iter().map(|v| v.len()).collect::<Vec<_>>(),
        );

        let input_polys = input_polys
            .into_iter()
            .map(|evals| DensePolynomial::new(evals))
            .collect::<Vec<_>>();

        // this flag specifies whether the mask matrix is decomposed or not in 2 matrices
        let decomposed_mask = mask_polys.len() == 2;
        let either_mles = if decomposed_mask {
            vec![
                &eq_poly,
                &mask_polys[0],
                &row_lt_poly,
                &column_lt_poly,
                &mask_polys[1],
            ]
        } else {
            vec![&eq_poly, &mask_polys[0], &row_lt_poly, &column_lt_poly]
        }
        .into_iter()
        .chain(input_polys.iter())
        .map(Either::Left)
        .collect::<Vec<_>>();
        let num_threads = optimal_sumcheck_threads(num_vars);
        let expr_builder =
            VirtualPolynomialsBuilder::<F>::new_with_mles(num_threads, num_vars, either_mles);

        let sumcheck_expression =
            self.build_sumcheck_expression(&input_shape, self.negative_infinity, decomposed_mask);
        let virtual_poly = expr_builder.to_virtual_polys(
            &sumcheck_expression[..input_polys.len()],
            &batching_challenges,
        );
        let (sumcheck_proof, state) = IOPProverState::<F>::prove(virtual_poly, prover.transcript);
        let evaluations = state.get_mle_flatten_final_evaluations()[4..].to_vec();

        let sumcheck_point = state.collect_raw_challenges();

        // We match the number of provided claims, so if we received one claim we output one claim, if we received n claims we output n claims
        let combined_eval = batching_challenges
            .iter()
            .zip(evaluations[decomposed_mask as usize..].iter()) // we need to skip the first evaluation in case of local attention mask,
            // as it refers to the additional mask polynomial
            .fold(F::ZERO, |acc, (c, e)| acc + (*c) * (*e));
        let full_point = sumcheck_point
            .iter()
            .chain(batching_point.iter())
            .copied()
            .collect::<Vec<_>>();
        let input_claim = vec![Claim::<F>::new(full_point, combined_eval)];

        // band 多项式 [row < col + n] 的 verifier 侧求值用闭式
        // （eval_band_mle，矩形网格通用）核对，无需子 sumcheck 证明。
        let local_mask_proof = None;


        Ok((
            AttentionMaskProof {
                sumcheck_proof,
                evaluations,
                local_mask_proof,
            },
            input_claim,
        ))
    }

    /// Compute the polynomial(s) related to the mask. There can be more than one mask polynomial,
    /// depending on the type of `AttentionSpan` in `self`.
    /// Indeed:
    /// - In case of `AttentionSpan::Full`, the mask to be applied is the usual lower triangular matrix L
    /// - In case of `AttentionSpan::Local(window)`, the mask to be applied is the matrix M defined as
    ///   M[i,j] = 1 iff j <= i <= j + window. In order to allow the verifier to efficiently evaluate
    ///   claims for this matrix, we decompose `M` as the entry-wise product of the lower triangular
    ///   matrix L and the offset matrix O[i,j] = 1 iff i <= j + window. Therefore, in the sumcheck for the
    ///   attention mask we employ 2 matrices to apply the mask rather than a single matrix.  
    ///
    /// NOTE: the function does NOT handle the single inference with caching case, and that's ok
    /// since we never want to prove a single token inference with caching enabled.
    /// We always prove the full sequence length, without caching.
    /// However, the evaluation needs to support both cases.
    /// 矩形网格 [rows_pad × cols_pad] 的 mask 多项式（行主序：flat = row·cols_pad + col）。
    /// band 语义 Local(n) 在矩形网格上恒等判定：n ≥ rows_pad 时 band 覆盖全网格。
    pub(crate) fn make_mask_polys<F: PrimeField>(&self, rows_pad: usize, cols_pad: usize) -> Vec<DensePolynomial<'_, F>> {
        let bool_mle_from_fn = |evals_fn: Box<dyn Fn(usize, usize) -> F>| {
            let mut evals = vec![];
            for row in 0..rows_pad {
                for col in 0..cols_pad {
                    let eval = evals_fn.as_ref()(row, col);
                    evals.push(eval);
                }
            }

            DensePolynomial::new(evals)
        };

        let zeroifier_mle =
            bool_mle_from_fn(Box::new(
                |token, other| {
                    if token >= other { F::ONE } else { F::ZERO }
                },
            ));

        if let AttentionSpan::Local(n) = &self.span {
            if *n >= rows_pad {
                // band [row < col + n] 在整个网格上恒为 1（col ≥ 0, row ≤ rows_pad-1 < n）
                return vec![zeroifier_mle];
            }
            let local_mask_mle =
                bool_mle_from_fn(Box::new(
                    |token, other| {
                        if token < other + *n { F::ONE } else { F::ZERO }
                    },
                ));
            return vec![zeroifier_mle, local_mask_mle];
        }

        vec![zeroifier_mle]
    }

    /// Function to make the row and column less than polynomials for proving purposes
    /// 矩形网格 [rows_pad × cols_pad]（行主序）：
    /// row lt = [row < rows_unpadded]（行模式）；
    /// col lt = [col < cols_unpadded]（每行内的列模式）。
    pub(crate) fn make_row_column_lt_polys<F: PrimeField>(
        &self,
        rows_unpadded: usize,
        cols_unpadded: usize,
        rows_pad: usize,
        cols_pad: usize,
    ) -> (DensePolynomial<'_, F>, DensePolynomial<'_, F>) {
        // Row less-than: 每一整行的值由 row 决定（[row < rows_unpadded]）。
        // 注意不能用 cycle()——cycle 会使 flat i 取 pattern[i % rows_pad]
        // = [col < rows_unpadded]，实际变成列模式（对称阈值/全 1 时与
        // 列 lt 数组恰好相同，测试不可见；非对称网格暴露）。
        let row_evals = (0..rows_pad)
            .flat_map(move |row_index| {
                std::iter::repeat_n(
                    if row_index < rows_unpadded { F::ONE } else { F::ZERO },
                    cols_pad,
                )
            })
            .collect::<Vec<F>>();

        // The column less than evaluations: within each row the first
        // `cols_unpadded` entries are one, the rest zero; repeated per row.
        let column_evals = (0..rows_pad)
            .flat_map(move |_| {
                (0..cols_pad).map(move |col_index| {
                    if col_index < cols_unpadded {
                        F::ONE
                    } else {
                        F::ZERO
                    }
                })
            })
            .collect::<Vec<F>>();

        (
            DensePolynomial::new(row_evals),
            DensePolynomial::new(column_evals),
        )
    }
}

#[derive(Debug, Clone)]
/// Struct storing all information to prove the application of an attention mask correctly without having to do proving work on padded parts.
pub(crate) struct MaskProvingData<F: PrimeField> {
    /// These values are the evaluations of the eq-poly for the higher dims that aren't from padding
    batching_challenges: Vec<F>,
    /// This is the point used to make the batch challenges
    batching_point: Vec<F>,
    /// This is evaluations of the eq-poly for each of the rank-2 tensors that the mask is applied to
    eq_evals: Vec<F>,
    /// This list of evaluations are the rank-2 tensors forming the input that aren't padding parts
    input_polys: Vec<Vec<F>>,
    input_shape: Shape,
}

impl<F: PrimeField> MaskProvingData<F> {
    /// Create a new [`MaskProvingData`]
    pub fn new(
        batching_challenges: Vec<F>,
        batching_point: Vec<F>,
        eq_evals: Vec<F>,
        input_polys: Vec<Vec<F>>,
        input_shape: Shape,
    ) -> Self {
        MaskProvingData {
            batching_challenges,
            batching_point,
            eq_evals,
            input_polys,
            input_shape,
        }
    }

    pub fn from_claims_and_input(claim: &Claim<F>, input: &Tensor<F>) -> Result<Self> {
        let input_shape = input.shape().clone();
        let unpadded_shape = input.unpadded_shape();
        let rank = input_shape.rank();

        let unpadded_input = input.reduce_to_shape(unpadded_shape)?;
        let final_dim = unpadded_shape.dim(-1);
        let second_to_last_dim = unpadded_shape.dim(-2);
        let chunk_size = second_to_last_dim * final_dim;

        // 矩形网格支持：input_polys = [rows_pad, cols_pad]（各维独立 pad），
        // mask 多项式按同一矩形网格构建（见 make_mask_polys）。
        let input_polys = unpadded_input
            .data()
            .chunks(chunk_size)
            .map(|chunk| {
                let tensor =
                    Tensor::<F>::new(vec![second_to_last_dim, final_dim].into(), chunk.to_vec())?;
                Ok(tensor.pad_next_power_of_two().into_data())
            })
            .collect::<Result<Vec<Vec<F>>>>()?;

        // Split the last claim point into the points corresponding to each dimension
        let dim_points = input_shape.split_point(claim.point())?;

        // Make the eq_poly evaluations from the points for the last two dimensions
        // 点序 = [cols 点, rows 点]（与既有 rev 顺序一致），矩形网格下不扩展。
        let eq_point = dim_points[rank - 2..]
            .iter()
            .rev()
            .flat_map(|p| *p)
            .copied()
            .collect::<Vec<F>>();

        let batching_challenges = dim_points[..rank - 2]
            .iter()
            .zip(unpadded_shape[..rank - 2].iter())
            .fold(vec![F::ONE], |mut acc, (point_slice, dim)| {
                let evals = evals(point_slice);
                acc = acc
                    .into_iter()
                    .flat_map(|c| evals.iter().take(*dim).map(|e| c * *e).collect::<Vec<F>>())
                    .collect::<Vec<F>>();
                acc
            });

        let batching_point = dim_points[..rank - 2]
            .iter()
            .rev()
            .flat_map(|p| *p)
            .copied()
            .collect::<Vec<F>>();

        Ok(MaskProvingData::new(
            batching_challenges,
            batching_point,
            evals(&eq_point),
            input_polys,
            input_shape,
        ))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::ensure;
    use ark_ff::{AdditiveGroup, Field};
    use dp_crypto::poly::{dense::DensePolynomial, slice::SmartSlice};

    use crate::{
        Element, Shape, Tensor,
        layers::transformer::attention_mask::{
            AttentionMask, AttentionSpan,
            evaluate::non_caching_case_mask,
            prove::{fix_lower_mle_columns, fix_lower_mle_rows},
        },
        quantization::ToElement,
        tensor::WrappedTensor,
        testing::random_field_vector,
    };

    type F = ark_bn254::Fr;

    #[test]
    fn test_fix_lower_mle() -> anyhow::Result<()> {
        let num_row_vars = 6;
        let num_col_vars = num_row_vars;
        let mut lower_evals = vec![];
        let mut upper_evals = vec![];
        for row in 0..(1 << num_row_vars) {
            for col in 0..(1 << num_col_vars) {
                if row >= col {
                    lower_evals.push(F::ONE)
                } else {
                    lower_evals.push(F::ZERO)
                }
            }
            for col in 0..(1 << num_col_vars) {
                if row <= col {
                    upper_evals.push(F::ONE)
                } else {
                    upper_evals.push(F::ZERO)
                }
            }
        }
        let lower_mle = DensePolynomial::new_from_smart_slice(SmartSlice::Borrowed(&lower_evals));
        let upper_mle = DensePolynomial::new_from_smart_slice(SmartSlice::Borrowed(&upper_evals));

        let row_point = random_field_vector(num_row_vars);

        let fixed_lower_mle = lower_mle.fix_high_variables(&row_point);

        let fixed_lower_evals = fix_lower_mle_rows(&row_point);
        let custom_fixed_lower_mle =
            DensePolynomial::new_from_smart_slice(SmartSlice::Borrowed(&fixed_lower_evals));
        assert_eq!(fixed_lower_mle.evals(), custom_fixed_lower_mle.evals(),);

        // test fixing columns in lower triangular MLE
        let column_point = random_field_vector(num_col_vars);
        let fixed_lower_mle = lower_mle.fix_low_variables(&column_point);

        let fixed_lower_evals = fix_lower_mle_columns(&column_point);
        let custom_fixed_lower_mle =
            DensePolynomial::new_from_smart_slice(SmartSlice::Borrowed(&fixed_lower_evals));
        assert_eq!(fixed_lower_mle.evals(), custom_fixed_lower_mle.evals(),);

        // test fixing rows in upper triangular MLE
        let fixed_upper_mle = upper_mle.fix_high_variables(&row_point);
        let fixed_upper_evals = fix_lower_mle_columns(&row_point);
        let custom_fixed_upper_mle =
            DensePolynomial::new_from_smart_slice(SmartSlice::Borrowed(&fixed_upper_evals));
        assert_eq!(fixed_upper_mle.evals(), custom_fixed_upper_mle.evals(),);

        // test fixing columns in upper triangular MLE
        let fixed_upper_mle = upper_mle.fix_low_variables(&column_point);

        let fixed_upper_evals = fix_lower_mle_rows(&column_point);
        let custom_fixed_upper_mle =
            DensePolynomial::new_from_smart_slice(SmartSlice::Borrowed(&fixed_upper_evals));
        assert_eq!(fixed_upper_mle.evals(), custom_fixed_upper_mle.evals(),);

        Ok(())
    }

    const SEQ_LENS: [usize; 6] = [32, 64, 128, 256, 512, 1024];

    #[test]
    fn test_mask_polys_vs_mask() -> anyhow::Result<()> {
        for seq_len in SEQ_LENS {
            test_mask_polys_vs_mask_helper(seq_len)?;
        }
        Ok(())
    }

    fn test_mask_polys_vs_mask_helper(seq_len: usize) -> anyhow::Result<()> {
        let local_span = seq_len / 2;
        let span = AttentionSpan::Local(local_span);
        let mask_layer = AttentionMask::<Element>::new(Element::MIN).with_span(span)?;

        let test_tensor = Tensor::<Element>::new(
            Shape::new(vec![seq_len, seq_len]),
            vec![1; seq_len * seq_len],
        )?;
        let wrapped_tensor = WrappedTensor::try_from(test_tensor)?;
        // non_caching_case_mask returns true where a position is masked out (set to -inf)
        // false = attended position = the element-wise product of the two mask polys should be 1
        let bool_mask = non_caching_case_mask(span, seq_len);
        let masked_tensor = wrapped_tensor.mask_fill(bool_mask, 0)?.to_native();

        // make_mask_polys returns [lower_tri_mle, offset_mle] for Local(n < seq_len)
        let mask_polys = mask_layer.make_mask_polys::<F>(seq_len, seq_len);
        assert_eq!(
            mask_polys.len(),
            2,
            "Expected two mask polynomials for Local(512) with seq_len=1024"
        );

        let combined_poly_data = mask_polys[0]
            .evals()
            .iter()
            .zip(mask_polys[1].evals().iter())
            .map(|(&a, &b)| {
                let a = a.to_element();
                let b = b.to_element();
                a * b
            })
            .collect::<Vec<Element>>();

        let poly_tensor =
            Tensor::<Element>::new(Shape::new(vec![seq_len, seq_len]), combined_poly_data)?;

        // Check equality between the two tensors
        ensure!(
            masked_tensor == poly_tensor,
            "Mask tensor and poly tensor not equal for sequence length: {seq_len}, local span: {local_span}"
        );

        Ok(())
    }
}
