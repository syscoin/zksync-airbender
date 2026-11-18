//! Exercise the verifier's FRI leaf layout and folding with known polynomials.
use field::{
    Field, FieldExtension, Mersenne31Complex as C, Mersenne31Field as B, Mersenne31Quartic as E,
};
use verifier_common::fri_folding::{fri_fold_by_log_n, fri_fold_by_log_n_with_fma};

const COSET_OFFSETS: [C; 2] = [C::ONE, C::TWO_ADICITY_GENERATORS[5]];
const TERMINAL_COEFFICIENTS: usize = 2;

fn ext(words: [u32; 4]) -> E {
    E::from_array_of_base(words.map(B::new))
}

fn evaluate(coefficients: &[E], point: C) -> E {
    coefficients.iter().rev().fold(E::ZERO, |mut acc, c| {
        acc.mul_assign_by_base(&point);
        acc.add_assign(c);
        acc
    })
}

fn coset_point(log_size: usize, index: usize, offset: C) -> C {
    let mut point = C::TWO_ADICITY_GENERATORS[log_size].pow(index as u32);
    point.mul_assign(&offset);
    point
}

// For P(X) = X^d + 1, the opening quotient at z has coefficients z^(d-1-i).
fn lifted_opening(point: C, degree: usize, z: E, beta: E) -> E {
    let quotient: Vec<_> = (0..degree)
        .map(|i| z.pow((degree - 1 - i) as u32))
        .collect();
    let mut factor = beta;
    factor.mul_assign_by_base(&point);
    factor.add_assign(&E::ONE);
    let mut result = evaluate(&quotient, point);
    result.mul_assign(&factor);
    result
}

fn fold_coset(values: &[E], offset: C, challenge: E) -> Vec<E> {
    let log_size = values.len().ilog2() as usize;
    let half = values.len() / 2;
    (0..half)
        .map(|i| {
            let mut leaf = [B::ZERO; 8];
            for (dst, value) in leaf
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip([values[i], values[i + half]])
            {
                *dst = <E as FieldExtension<B>>::into_coeffs_in_base(value);
            }
            // Alternating sides exercises the check against each position in the leaf.
            let index = i + (i % 2) * half;
            let mut value = values[index];
            let mut position = index;
            let mut tree_index = index.reverse_bits() >> (usize::BITS as usize - log_size);
            let mut point = coset_point(log_size, index, offset);
            let mut inverse = offset.inverse().unwrap();
            let mut log = log_size;
            // SAFETY: This factor-two fold reads two flattened quartics, one challenge,
            // and one shared root. The indices describe this leaf in bit-reversed order.
            unsafe {
                if E::PREFER_FMA
                    && E::USE_SPEC_MUL_BY_BASE_VIA_MUL_BY_SELF
                    && E::CAN_PROJECT_FROM_BASE
                {
                    fri_fold_by_log_n_with_fma::<1>(
                        &mut value,
                        &mut point,
                        &mut log,
                        &mut position,
                        &mut tree_index,
                        &mut inverse,
                        &leaf,
                        &challenge,
                        &[C::ONE],
                    );
                } else {
                    fri_fold_by_log_n::<1>(
                        &mut value,
                        &mut point,
                        &mut log,
                        &mut position,
                        &mut tree_index,
                        &mut inverse,
                        &leaf,
                        &[challenge],
                        &[C::ONE],
                    );
                }
            }
            assert_eq!(log, log_size - 1);
            assert_eq!(position, i);
            assert_eq!(point, coset_point(log, i, offset.pow(2)));
            value
        })
        .collect()
}

fn interpolate_on_coset(values: &[E], offset: C) -> Vec<E> {
    let size = values.len();
    let inverse_generator = C::TWO_ADICITY_GENERATORS_INVERSED[size.ilog2() as usize];
    let inverse_size = B::new(size as u32).inverse().unwrap();
    (0..size)
        .map(|k| {
            let mut coefficient = E::ZERO;
            for (j, value) in values.iter().enumerate() {
                let mut term = *value;
                term.mul_assign_by_base(&inverse_generator.pow((j * k) as u32));
                coefficient.add_assign(&term);
            }
            coefficient.mul_assign_by_base(&offset.inverse().unwrap().pow(k as u32));
            coefficient.mul_assign_by_base(&inverse_size);
            coefficient
        })
        .collect()
}

fn fold_to_terminal(mut offset: C, challenges: [E; 2], sample: impl Fn(C) -> E) -> (Vec<E>, C) {
    let mut values: Vec<_> = (0..16)
        .map(|index| sample(coset_point(4, index, offset)))
        .collect();
    for challenge in challenges {
        values = fold_coset(&values, offset, challenge);
        offset.square();
    }
    (values, offset)
}

#[test]
fn verifier_folds_both_cosets_to_the_expected_terminal_degree() {
    let z = ext([11, 13, 17, 19]);
    let beta = ext([2, 3, 5, 7]);
    let challenges = [ext([23, 29, 31, 37]), ext([41, 43, 47, 53])];

    // Two folds reduce a degree-below-eight codeword to a linear terminal polynomial.
    // A degree-eight polynomial retains a quadratic term that a linear terminal cannot fit.
    for source_degree in [1, 4, 7, 8] {
        let mut terminals = Vec::new();
        for offset in COSET_OFFSETS {
            let (values, offset) = fold_to_terminal(offset, challenges, |point| {
                lifted_opening(point, source_degree, z, beta)
            });
            let coefficients = interpolate_on_coset(&values, offset);
            assert_eq!(
                coefficients.iter().rposition(|c| !c.is_zero()),
                Some(source_degree / 4)
            );
            let mut all_fit = true;
            for (i, value) in values.iter().enumerate() {
                let x = coset_point(2, i, offset);
                assert_eq!(evaluate(&coefficients, x), *value);
                all_fit &= evaluate(&coefficients[..TERMINAL_COEFFICIENTS], x) == *value;
            }
            assert_eq!(all_fit, source_degree < 8);
            terminals.push(coefficients);
        }
        assert_eq!(terminals[0], terminals[1]);
    }
}

fn assert_reciprocal_opening_exceeds_terminal_bound(linear: E, inverse: E) {
    let opening_point = ext([11, 13, 17, 19]);
    let beta = ext([2, 3, 5, 7]);
    let challenges = [ext([23, 29, 31, 37]), ext([41, 43, 47, 53])];
    let source = |x: E| {
        let mut value = inverse;
        value.mul_assign(&x.inverse().unwrap());
        let mut linear_term = linear;
        linear_term.mul_assign(&x);
        value.add_assign(&linear_term);
        value
    };
    let at_opening = source(opening_point);

    // For aX + b/X, the opening quotient is a - b/(zX). After lifting, the first
    // challenge sends the linear term to the constant. Both challenges multiply
    // the reciprocal term. Each unnormalized fold doubles all coefficients.
    let mut reciprocal = inverse;
    reciprocal.mul_assign(&opening_point.inverse().unwrap());
    reciprocal.negate();
    let mut terminal_constant = reciprocal;
    terminal_constant.mul_assign(&beta);
    terminal_constant.add_assign(&linear);
    let mut lifted_linear = linear;
    lifted_linear.mul_assign(&beta);
    lifted_linear.mul_assign(&challenges[0]);
    terminal_constant.add_assign(&lifted_linear);
    terminal_constant.mul_assign_by_base(&B::new(4));
    for challenge in challenges {
        reciprocal.mul_assign(&challenge);
    }
    reciprocal.mul_assign_by_base(&B::new(4));
    assert!(!reciprocal.is_zero());

    let mut terminals = Vec::new();
    for offset in COSET_OFFSETS {
        let (values, offset) = fold_to_terminal(offset, challenges, |point| {
            let x = E::from_base(point);
            let mut divisor = x;
            divisor.sub_assign(&opening_point);
            let mut opening = source(x);
            opening.sub_assign(&at_opening);
            opening.mul_assign(&divisor.inverse().unwrap());
            let mut lift = beta;
            lift.mul_assign_by_base(&point);
            lift.add_assign(&E::ONE);
            opening.mul_assign(&lift);
            opening
        });
        let coefficients = interpolate_on_coset(&values, offset);
        for (index, value) in values.iter().enumerate() {
            let point = coset_point(2, index, offset);
            let mut expected = reciprocal;
            expected.mul_assign_by_base(&point.inverse().unwrap());
            expected.add_assign(&terminal_constant);
            assert_eq!(*value, expected);
            assert_eq!(evaluate(&coefficients, point), *value);
        }
        // Four distinct evaluations uniquely determine the cubic. If its
        // cubic coefficient is nonzero, no linear terminal fits this coset.
        let mut expected_cubic = reciprocal;
        expected_cubic.mul_assign_by_base(&offset.pow(4).inverse().unwrap());
        assert_eq!(coefficients[3], expected_cubic);
        assert!(!coefficients[3].is_zero());
        terminals.push(coefficients);
    }
    assert_ne!(terminals[0], terminals[1]);
}

#[test]
fn quartic_reciprocal_opening_exceeds_terminal_bound_on_both_cosets() {
    assert_reciprocal_opening_exceeds_terminal_bound(E::ZERO, ext([3, 5, 7, 11]));
}

#[test]
fn base_linear_plus_reciprocal_opening_exceeds_terminal_bound_on_both_cosets() {
    assert_reciprocal_opening_exceeds_terminal_bound(E::ONE, E::ONE);
}
