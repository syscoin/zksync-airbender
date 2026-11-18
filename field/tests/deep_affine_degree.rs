//! Exact-field algebra underlying the V2 DEEP factor `1 + beta * X`.
//!
//! These tests do not construct proofs. The FMA comparisons exercise the host
//! field operation, not the RISC-V delegation instruction or a CUDA kernel.

use field::{Field, FieldExtension, Mersenne31Field, Mersenne31Quartic};

type F = Mersenne31Quartic;

fn ext(words: [u32; 4]) -> F {
    F::from_array_of_base(words.map(Mersenne31Field::new))
}

fn eval(coefficients: &[F], x: F) -> F {
    coefficients.iter().rev().fold(F::ZERO, |mut value, c| {
        value.mul_assign(&x);
        value.add_assign(c);
        value
    })
}

fn eval_fma(coefficients: &[F], x: F) -> F {
    coefficients.iter().rev().fold(F::ZERO, |mut value, c| {
        value.fused_mul_add_assign(&x, c);
        value
    })
}

fn degree(coefficients: &[F]) -> Option<usize> {
    coefficients.iter().rposition(|c| !c.is_zero())
}

// Synthetic division gives (P(X) - P(z)) / (X - z), without evaluation-point
// division. It also covers X = z when checking polynomial identities.
fn opening_quotient(coefficients: &[F], z: F) -> Vec<F> {
    let mut quotient = vec![F::ZERO; coefficients.len().saturating_sub(1)];
    let mut carry = F::ZERO;
    for i in (1..coefficients.len()).rev() {
        carry.mul_assign(&z);
        carry.add_assign(&coefficients[i]);
        quotient[i - 1] = carry;
    }
    quotient
}

fn lift(quotient: &[F], beta: F) -> Vec<F> {
    let mut result = vec![F::ZERO; quotient.len() + 1];
    for (i, c) in quotient.iter().enumerate() {
        result[i].add_assign(c);
        let mut leading = *c;
        leading.mul_assign(&beta);
        result[i + 1].add_assign(&leading);
    }
    result
}

fn source(degree: usize) -> Vec<F> {
    (0..=degree).map(|i| ext([i as u32 + 1, 3, 5, 7])).collect()
}

#[test]
fn affine_lift_preserves_honest_source_degree_bound() {
    let beta = ext([2, 3, 5, 7]);
    let z = ext([11, 13, 17, 19]);
    for bound in [2, 4, 8, 16, 32] {
        for source_degree in 1..bound {
            let polynomial = source(source_degree);
            let quotient = opening_quotient(&polynomial, z);
            let lifted = lift(&quotient, beta);
            assert_eq!(degree(&quotient), Some(source_degree - 1));
            assert_eq!(degree(&lifted), Some(source_degree));
            assert!(degree(&lifted).unwrap() < bound);
        }
    }
    assert_eq!(degree(&lift(&opening_quotient(&[F::ONE], z), beta)), None);
}

#[test]
fn affine_lift_retains_the_source_degree_at_the_boundary() {
    let beta = ext([2, 3, 5, 7]);
    let z = ext([11, 13, 17, 19]);
    for bound in [2, 4, 8, 16, 32] {
        let polynomial = source(bound);
        let quotient = opening_quotient(&polynomial, z);
        let lifted = lift(&quotient, beta);
        assert_eq!(degree(&quotient), Some(bound - 1));
        assert_eq!(degree(&lifted), Some(bound));
        let mut leading = polynomial[bound];
        leading.mul_assign(&beta);
        assert_eq!(lifted[bound], leading);

        // Without the X coefficient the quotient loses a degree. This control
        // detects tests that accidentally omit the affine multiplication.
        assert_eq!(degree(&lift(&quotient, F::ZERO)), Some(bound - 1));
    }
}

#[test]
fn opening_and_affine_identities_agree_for_ordinary_and_fma_evaluation() {
    let beta = ext([2, 3, 5, 7]);
    let z = ext([11, 13, 17, 19]);
    for source_degree in [0, 1, 7, 8, 15, 16] {
        let polynomial = source(source_degree);
        let quotient = opening_quotient(&polynomial, z);
        let lifted = lift(&quotient, beta);
        for x in [F::ZERO, F::ONE, z, ext([23, 29, 31, 37])] {
            let q = eval(&quotient, x);
            assert_eq!(q, eval_fma(&quotient, x));
            let mut x_minus_z = x;
            x_minus_z.sub_assign(&z);
            let mut lhs = q;
            lhs.mul_assign(&x_minus_z);
            let mut rhs = eval(&polynomial, x);
            rhs.sub_assign(&eval(&polynomial, z));
            assert_eq!(lhs, rhs);

            let mut factor = beta;
            factor.mul_assign(&x);
            factor.add_assign(&F::ONE);
            let mut expected = q;
            expected.mul_assign(&factor);
            assert_eq!(eval(&lifted, x), expected);
            assert_eq!(eval_fma(&lifted, x), expected);
            let mut factor_fma = beta;
            factor_fma.fused_mul_add_assign(&x, &F::ONE);
            assert_eq!(factor_fma, factor);
        }
    }
}

#[test]
fn independent_affine_factors_do_not_share_a_nonzero_root() {
    // The remainder upon division by X - a is 1 + beta*a. Test this
    // polynomial fact for both base-valued and full-extension points.
    for a in [ext([23, 0, 0, 0]), ext([23, 29, 31, 37])] {
        let mut cancelling_beta = a.inverse().unwrap();
        cancelling_beta.negate();
        let mut other_beta = cancelling_beta;
        other_beta.add_assign(&F::ONE);
        assert_eq!(eval(&[F::ONE, cancelling_beta], a), F::ZERO);
        assert_eq!(eval_fma(&[F::ONE, cancelling_beta], a), F::ZERO);
        assert_eq!(eval(&[F::ONE, other_beta], a), a);
        assert_eq!(eval_fma(&[F::ONE, other_beta], a), a);
        assert!(!a.is_zero());
    }
}

#[test]
fn two_opening_batches_match_power_sum_and_fma_reconstruction() {
    let alpha = ext([2, 3, 5, 7]);
    let beta = ext([11, 13, 17, 19]);
    let x = ext([23, 29, 31, 37]);
    let zs = [ext([41, 43, 47, 53]), ext([59, 61, 67, 71])];
    let batches = [
        vec![source(1), source(7), source(8)],
        vec![source(2), source(5)],
    ];
    let mut ordinary = F::ZERO;
    let mut fma = F::ZERO;
    let mut power = F::ONE;
    for (batch, z) in batches.iter().zip(zs) {
        let initial_power = power;
        let mut at_z = Vec::new();
        let mut at_x = Vec::new();
        for polynomial in batch {
            at_z.push(eval(polynomial, z));
            at_x.push(eval(polynomial, x));
            let mut q = eval(&opening_quotient(polynomial, z), x);
            q.mul_assign(&power);
            ordinary.add_assign(&q);
            power.mul_assign(&alpha);
        }
        let mut reconstructed = eval_fma(&at_z, alpha);
        reconstructed.sub_assign(&eval_fma(&at_x, alpha));
        let mut denominator = z;
        denominator.sub_assign(&x);
        reconstructed.mul_assign(&denominator.inverse().unwrap());
        reconstructed.mul_assign(&initial_power);
        fma.add_assign(&reconstructed);
    }
    assert_eq!(ordinary, fma);
    let mut factor = beta;
    factor.fused_mul_add_assign(&x, &F::ONE);
    ordinary.mul_assign(&factor);
    fma.mul_assign(&factor);
    assert_eq!(ordinary, fma);
}
