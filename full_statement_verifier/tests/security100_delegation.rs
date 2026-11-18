#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use blake2_with_compression_verifier::verify_100;
use verifier_common::prover::nd_source_std::{set_iterator, try_read_word};
use verifier_common::{ProofOutput, ProofPublicInputs};

#[test]
fn verify_honest_security100_delegation() {
    let oracle = include_bytes!("fixtures/security100-blake2s-delegation.bin");
    let (words, remainder) = oracle.as_chunks::<4>();
    assert!(remainder.is_empty());

    std::thread::Builder::new()
        .stack_size(1 << 27)
        .spawn(move || {
            set_iterator(words.iter().copied().map(u32::from_le_bytes));
            // The oracle contains a complete proof for this concrete verifier and security level.
            unsafe { verify_100(&mut ProofOutput::empty(), &mut ProofPublicInputs::uninit()) };
            assert!(try_read_word().is_none());
        })
        .unwrap()
        .join()
        .unwrap();
}
