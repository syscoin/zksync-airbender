use std::fs::File;
use std::path::Path;
use verifier_common::cs::one_row_compiler::CompiledCircuitArtifact;
use verifier_common::field::Mersenne31Field;
use verifier_common::proof_flattener::flatten_full_unrolled_proof;
use verifier_common::prover::nd_source_std::set_iterator;
use verifier_common::prover::prover_stages::unrolled_prover::UnrolledModeProof;
use verifier_common::{ProofOutput, ProofPublicInputs};

#[test]
fn v2_rejects_the_historical_shift_csr_proof() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let proof: UnrolledModeProof = serde_json::from_reader(
        File::open(workspace.join("prover/shift_binop_csrrw_unrolled_proof.json")).unwrap(),
    ).unwrap();
    let layout: CompiledCircuitArtifact<Mersenne31Field> = serde_json::from_reader(
        File::open(workspace.join("cs/shift_binop_csrrw_preprocessed_layout.json")).unwrap(),
    ).unwrap();
    assert_eq!(proof.queries.len(), 63);
    let oracle = flatten_full_unrolled_proof(&proof, &layout);
    let rejected = std::thread::Builder::new()
        .name("legacy CSR fixture".into())
        .stack_size(1 << 27)
        .spawn(move || {
            set_iterator(oracle.into_iter());
            // The CSR fixture belongs to this concrete circuit and predates the V2 FRI opening.
            std::panic::catch_unwind(|| unsafe {
                shift_binary_csr_verifier::verify_80(&mut ProofOutput::empty(), &mut ProofPublicInputs::uninit());
            }).is_err()
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(rejected);
}
