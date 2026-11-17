use cli_lib::prover_utils::{ProofArtifact, ProofTarget, SecurityLevel};
use execution_utils::setups::{
    binary_u8_to_u32, get_unified_circuit_artifact_for_machine_type,
    get_unrolled_circuits_artifacts_for_machine_type, pad_bytecode_bytes_for_proving,
    pad_bytecode_for_proving, CompiledCircuitsSet,
};
use execution_utils::unified_circuit::{
    compute_combined_recursion_layers_output, flatten_proof_into_responses_for_unified_recursion,
    flatten_proofs_into_responses_for_combined_unified_recursion,
    verify_combined_proofs_in_unified_layer, verify_proof_in_unified_layer, CombinedRecursionInput,
};
use execution_utils::unrolled::{
    compute_setup_for_machine_configuration, flatten_proof_into_responses_for_unrolled_recursion,
    verify_unrolled_layer_proof, UnrolledProgramSetup,
};
use riscv_transpiler::abstractions::non_determinism::QuasiUARTSource;
use riscv_transpiler::cycle::{
    IMStandardIsaConfigWithUnsignedMulDiv, IWithoutByteAccessIsaConfigWithDelegation,
};
use riscv_transpiler::ir::{preprocess_bytecode, ReducedMachineDecoderConfig};
use riscv_transpiler::vm::{DelegationsCounters, RamWithRomRegion, SimpleTape, State, VM};
use serde::Deserialize;
use sha3::{Digest, Keccak256};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    app_bin: PathBuf,
    app_text: PathBuf,
    baseline_verifiers: PathBuf,
    candidate_verifiers: PathBuf,
    security: SecurityLevel,
    max_cycles: usize,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    expected_chain: [u32; 8],
    inputs: Vec<Input>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    proof: PathBuf,
    batch_id: u64,
    public_output: [u32; 8],
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let file = fs::File::open(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_reader(file).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn image_name(layer: &str, security: SecurityLevel) -> String {
    let suffix = match security {
        SecurityLevel::Security80 => "",
        SecurityLevel::Security100 => "_security_100_bits",
    };
    format!("recursion_in_{layer}_layer{suffix}")
}

struct ProofLayer {
    setup: UnrolledProgramSetup,
    layouts: CompiledCircuitsSet,
}

impl ProofLayer {
    fn base(bin: &[u8], text: &[u8]) -> Self {
        let mut words = binary_u8_to_u32(bin);
        pad_bytecode_for_proving(&mut words);
        let mut bin = bin.to_vec();
        let mut text = text.to_vec();
        pad_bytecode_bytes_for_proving(&mut bin);
        pad_bytecode_bytes_for_proving(&mut text);
        Self {
            setup: compute_setup_for_machine_configuration::<IMStandardIsaConfigWithUnsignedMulDiv>(
                &bin, &text,
            ),
            layouts: get_unrolled_circuits_artifacts_for_machine_type::<
                IMStandardIsaConfigWithUnsignedMulDiv,
            >(&words),
        }
    }

    fn recursion(dir: &Path, security: SecurityLevel, unrolled: bool) -> Self {
        let layer = if unrolled { "unrolled" } else { "unified" };
        let image = image_name(layer, security);
        let setup = load_json(&dir.join(format!("{image}.setup.json")));
        let mut words = binary_u8_to_u32(&fs::read(dir.join(format!("{image}.bin"))).unwrap());
        pad_bytecode_for_proving(&mut words);
        let layouts = if unrolled {
            get_unrolled_circuits_artifacts_for_machine_type::<
                IWithoutByteAccessIsaConfigWithDelegation,
            >(&words)
        } else {
            get_unified_circuit_artifact_for_machine_type::<IWithoutByteAccessIsaConfigWithDelegation>(
                &words,
            )
        };
        Self { setup, layouts }
    }
}

fn check_image(dir: &Path, image: &str, words: &[u32], expected: &[u32; 16], max_cycles: usize) {
    let bin = fs::read(dir.join(format!("{image}.bin"))).unwrap();
    let success_pc = execution_utils::find_binary_exit_point(&bin);
    let bin_words = binary_u8_to_u32(&bin);
    let text_words = binary_u8_to_u32(&fs::read(dir.join(format!("{image}.text"))).unwrap());
    let instructions = preprocess_bytecode::<ReducedMachineDecoderConfig>(&text_words);
    let tape = SimpleTape::new(&instructions);
    assert!(!words.is_empty());
    for incomplete in [false, true] {
        let len = words.len() - usize::from(incomplete);
        let mut source = QuasiUARTSource::new_with_reads(words[..len].to_vec());
        let mut ram = RamWithRomRegion::<{prover::common_constants::rom::ROM_SECOND_WORD_BITS}>::from_rom_content(&bin_words, 1 << 30);
        let mut state = State::initial_with_counters(DelegationsCounters::default());
        let result = catch_unwind(AssertUnwindSafe(|| {
            VM::<DelegationsCounters>::run_basic_unrolled(
                &mut state,
                &mut ram,
                &mut (),
                &tape,
                max_cycles,
                &mut source,
            )
        }));
        assert!(
            source.oracle.is_empty(),
            "{} left unconsumed words",
            dir.display()
        );
        if incomplete {
            let panic = result.expect_err("an incomplete oracle must request its missing word");
            let reason = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert_eq!(reason, Some("must have an answer"));
            assert_ne!(state.pc, success_pc);
        } else {
            assert!(
                result.unwrap(),
                "cycle budget exhausted for {}",
                dir.display()
            );
            assert_eq!(state.pc, success_pc);
            let regs = state.registers.map(|register| register.value);
            assert_eq!(
                &regs[10..26],
                expected,
                "public registers for {}",
                dir.display()
            );
        }
    }
}

#[test]
#[ignore = "supply an AIRBENDER_EQUIVALENCE_CASE manifest and authentic external proof files"]
fn replay_against_baseline() {
    let fixture = PathBuf::from(
        std::env::var_os("AIRBENDER_EQUIVALENCE_CASE").expect("set AIRBENDER_EQUIVALENCE_CASE"),
    );
    let paths = fixture.parent().unwrap();
    let manifest: Manifest = load_json(&fixture);
    assert!(!manifest.cases.is_empty() && manifest.max_cycles > 0);
    let baseline = paths.join(&manifest.baseline_verifiers);
    let candidate = paths.join(&manifest.candidate_verifiers);
    assert_ne!(
        baseline.canonicalize().unwrap(),
        candidate.canonicalize().unwrap()
    );
    let app_bin = fs::read(paths.join(&manifest.app_bin)).unwrap();
    let app_text = fs::read(paths.join(&manifest.app_text)).unwrap();
    let bin_hash: [u8; 32] = Keccak256::digest(&app_bin).into();
    let text_hash: [u8; 32] = Keccak256::digest(&app_text).into();
    let security = manifest.security.model();

    for case in manifest.cases {
        assert!(!case.inputs.is_empty());
        let mut artifacts = Vec::with_capacity(case.inputs.len());
        for spec in &case.inputs {
            let artifact: ProofArtifact = load_json(&paths.join(&spec.proof));
            assert_eq!(artifact.schema_version, 1);
            assert_eq!(artifact.security_level, manifest.security);
            assert_eq!(artifact.batch_id, spec.batch_id);
            assert_eq!(artifact.program_bin_keccak, bin_hash);
            assert_eq!(artifact.program_text_keccak, text_hash);
            artifacts.push(artifact);
        }
        let target = artifacts[0].target;
        let base = target == ProofTarget::Base;
        let unrolled = target == ProofTarget::RecursionUnrolled;
        assert!(base || unrolled || target == ProofTarget::RecursionUnified);
        if artifacts.len() > 1 {
            assert!(artifacts
                .iter()
                .all(|p| p.target == ProofTarget::RecursionUnified));
            assert!(case
                .inputs
                .windows(2)
                .all(|pair| pair[0].batch_id.checked_add(1) == Some(pair[1].batch_id)));
        }
        let layer = if base {
            ProofLayer::base(&app_bin, &app_text)
        } else {
            ProofLayer::recursion(&baseline, manifest.security, unrolled)
        };
        let mut outputs = Vec::with_capacity(artifacts.len());
        for (artifact, spec) in artifacts.iter().zip(&case.inputs) {
            let output = if base {
                verify_unrolled_layer_proof(
                    &artifact.proof,
                    &layer.setup,
                    &layer.layouts,
                    true,
                    security,
                )
            } else {
                verify_proof_in_unified_layer(
                    &artifact.proof,
                    &layer.setup,
                    &layer.layouts,
                    unrolled,
                    security,
                )
            }
            .expect("native authentication");
            assert_eq!(&output[..8], &spec.public_output);
            assert_eq!(&output[8..], &case.expected_chain);
            outputs.push(output);
        }
        let (words, expected) = if artifacts.len() > 1 {
            let inputs: Vec<_> = artifacts
                .iter()
                .map(|artifact| CombinedRecursionInput {
                    proof: &artifact.proof,
                    setup: &layer.setup,
                    compiled_layouts: &layer.layouts,
                    input_is_unrolled: false,
                })
                .collect();
            let expected = compute_combined_recursion_layers_output(&outputs);
            let mut hasher = Keccak256::new();
            for input in &case.inputs {
                for word in input.public_output {
                    hasher.update(word.to_le_bytes());
                }
            }
            let public_hash: [u8; 32] = hasher.finalize().into();
            let expected_bytes: Vec<_> = expected[..8]
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect();
            assert_eq!(expected_bytes.as_slice(), public_hash);
            assert_eq!(&expected[8..], &case.expected_chain);
            assert_eq!(
                verify_combined_proofs_in_unified_layer(&inputs, security)
                    .expect("native range verification"),
                expected
            );
            (
                flatten_proofs_into_responses_for_combined_unified_recursion(&inputs),
                expected,
            )
        } else {
            let proof = &artifacts[0].proof;
            let words = if base {
                flatten_proof_into_responses_for_unrolled_recursion(
                    proof,
                    &layer.setup,
                    &layer.layouts,
                    true,
                )
            } else {
                flatten_proof_into_responses_for_unified_recursion(
                    proof,
                    &layer.setup,
                    &layer.layouts,
                    unrolled,
                )
            };
            (words, outputs[0])
        };
        let image = image_name(if base { "unrolled" } else { "unified" }, manifest.security);
        let image_text = format!("{image}.text");
        let baseline_text = fs::read(baseline.join(&image_text)).unwrap();
        let candidate_text = fs::read(candidate.join(&image_text)).unwrap();
        assert!(
            baseline_text != candidate_text,
            "both paths supplied the same executable for {image}"
        );
        for dir in [&baseline, &candidate] {
            check_image(dir, &image, &words, &expected, manifest.max_cycles);
        }
        println!(
            "verified {:?} batch IDs {:?}, consumed {} oracle words",
            target,
            case.inputs
                .iter()
                .map(|input| input.batch_id)
                .collect::<Vec<_>>(),
            words.len()
        );
    }
}
