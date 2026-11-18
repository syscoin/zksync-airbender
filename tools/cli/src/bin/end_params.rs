//! Prints the base-program end_params and the expected recursion chain (base -> unrolled ->
//! unified) for a given app binary, for embedding in downstream verifiers.

use clap::Parser;
use cli_lib::prover_utils::SecurityLevel;
use execution_utils::setups::{pad_bytecode_bytes_for_proving, read_binary};
use execution_utils::unified_circuit::compute_unified_setup_for_machine_configuration;
use execution_utils::unrolled::{compute_setup_for_machine_configuration, UnrolledProgramSetup};
use execution_utils::verifier_binaries::recursion_artifact;
use execution_utils::{RecursionArtifact, RecursionLayer};
use riscv_transpiler::cycle::{
    IMStandardIsaConfigWithUnsignedMulDiv, IWithoutByteAccessIsaConfigWithDelegation,
};
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    bin_path: PathBuf,
    text_path: PathBuf,
    #[arg(long, value_enum, default_value = "80")]
    security_level: SecurityLevel,
}

fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut padded = bytes.to_vec();
    pad_bytecode_bytes_for_proving(&mut padded);
    padded
}

fn main() {
    let args = Args::parse();
    let security = args.security_level.model();
    eprintln!("recursion security model: {security:?}");

    let (bin_bytes, _) = read_binary(&args.bin_path);
    let (text_bytes, _) = read_binary(&args.text_path);

    eprintln!("computing base setup (app program)...");
    let base_setup = compute_setup_for_machine_configuration::<IMStandardIsaConfigWithUnsignedMulDiv>(
        &padded(&bin_bytes),
        &padded(&text_bytes),
    );
    println!("app_end_params = {:?}", base_setup.end_params);
    let (base_chain, base_preimage) =
        UnrolledProgramSetup::begin_recursion_chain(&base_setup.end_params);
    println!("base_chain = {base_chain:?}");

    eprintln!("computing unrolled recursion setup...");
    let unrolled_bin =
        recursion_artifact(security, RecursionLayer::Unrolled, RecursionArtifact::Bin);
    let unrolled_text =
        recursion_artifact(security, RecursionLayer::Unrolled, RecursionArtifact::Txt);
    let unrolled_setup = compute_setup_for_machine_configuration::<
        IWithoutByteAccessIsaConfigWithDelegation,
    >(&padded(unrolled_bin), &padded(unrolled_text));
    println!("unrolled_end_params = {:?}", unrolled_setup.end_params);
    let (unrolled_chain, unrolled_preimage) = UnrolledProgramSetup::continue_recursion_chain(
        &unrolled_setup.end_params,
        &base_chain,
        &base_preimage,
    );
    println!("unrolled_chain = {unrolled_chain:?}");

    eprintln!("computing unified recursion setup...");
    let unified_bin = recursion_artifact(security, RecursionLayer::Unified, RecursionArtifact::Bin);
    let unified_text =
        recursion_artifact(security, RecursionLayer::Unified, RecursionArtifact::Txt);
    let unified_setup = compute_unified_setup_for_machine_configuration::<
        IWithoutByteAccessIsaConfigWithDelegation,
    >(&padded(unified_bin), &padded(unified_text));
    println!("unified_end_params = {:?}", unified_setup.end_params);
    let (unified_chain, _) = UnrolledProgramSetup::continue_recursion_chain(
        &unified_setup.end_params,
        &unrolled_chain,
        &unrolled_preimage,
    );
    println!("unified_chain (expected registers[8..16]) = {unified_chain:?}");
}
