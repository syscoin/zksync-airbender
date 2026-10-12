//! Compact host-side recursion metadata, not GPU proving precomputations.
//!
//! Callers loading these summaries must authenticate the complete serialized artifact,
//! the bin/text pairs and circuit implementation. `validate` checks internal consistency
//! against the actual binaries; it deliberately does not rederive the Merkle caps.

use crate::unified_circuit::compute_unified_setup_for_machine_configuration;
use crate::unrolled::{compute_setup_for_machine_configuration, UnrolledProgramSetup};
use crate::{verifier_binaries, RecursionArtifact, RecursionLayer};
use riscv_transpiler::common_constants::circuit_families::*;
use riscv_transpiler::cycle::{
    IMStandardIsaConfigWithUnsignedMulDiv, IWithoutByteAccessIsaConfigWithDelegation,
};
use sha3::Digest;
use verifier_common::SecurityModel;

const BASE_FAMILIES: &[u8] = &[
    ADD_SUB_LUI_AUIPC_MOP_CIRCUIT_FAMILY_IDX,
    JUMP_BRANCH_SLT_CIRCUIT_FAMILY_IDX,
    SHIFT_BINARY_CSR_CIRCUIT_FAMILY_IDX,
    MUL_DIV_CIRCUIT_FAMILY_IDX,
    LOAD_STORE_WORD_ONLY_CIRCUIT_FAMILY_IDX,
    LOAD_STORE_SUBWORD_ONLY_CIRCUIT_FAMILY_IDX,
];
const UNROLLED_FAMILIES: &[u8] = &[
    ADD_SUB_LUI_AUIPC_MOP_CIRCUIT_FAMILY_IDX,
    JUMP_BRANCH_SLT_CIRCUIT_FAMILY_IDX,
    SHIFT_BINARY_CSR_CIRCUIT_FAMILY_IDX,
    LOAD_STORE_WORD_ONLY_CIRCUIT_FAMILY_IDX,
];
const UNIFIED_FAMILIES: &[u8] = &[REDUCED_MACHINE_CIRCUIT_FAMILY_IDX];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramSetupSummaries {
    pub security_bits: u32,
    pub base: UnrolledProgramSetup,
    pub recursion_unrolled: UnrolledProgramSetup,
    pub recursion_unified: UnrolledProgramSetup,
}

fn security_bits(security: SecurityModel) -> u32 {
    match security {
        SecurityModel::Security80 => 80,
        SecurityModel::Security100 => 100,
    }
}

fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    setups::pad_bytecode_bytes_for_proving(&mut bytes);
    bytes
}

impl ProgramSetupSummaries {
    /// Explicit slow reference path. This computes all three CPU setup trees.
    pub fn compute(security: SecurityModel, base_binary: &[u8], base_text: &[u8]) -> Self {
        let recursion =
            |layer, kind| padded(verifier_binaries::recursion_artifact(security, layer, kind));
        Self {
            security_bits: security_bits(security),
            base: compute_setup_for_machine_configuration::<IMStandardIsaConfigWithUnsignedMulDiv>(
                &padded(base_binary),
                &padded(base_text),
            ),
            recursion_unrolled: compute_setup_for_machine_configuration::<
                IWithoutByteAccessIsaConfigWithDelegation,
            >(
                &recursion(RecursionLayer::Unrolled, RecursionArtifact::Bin),
                &recursion(RecursionLayer::Unrolled, RecursionArtifact::Txt),
            ),
            recursion_unified: compute_unified_setup_for_machine_configuration::<
                IWithoutByteAccessIsaConfigWithDelegation,
            >(
                &recursion(RecursionLayer::Unified, RecursionArtifact::Bin),
                &recursion(RecursionLayer::Unified, RecursionArtifact::Txt),
            ),
        }
    }

    /// Cheap consistency checks only: callers must separately authenticate cap provenance.
    pub fn validate(&self, security: SecurityModel, base_binary: &[u8]) -> Result<(), String> {
        if self.security_bits != security_bits(security) {
            return Err("setup summaries security mismatch".into());
        }
        validate_level("base", &self.base, base_binary, BASE_FAMILIES)?;
        validate_level(
            "recursion-unrolled",
            &self.recursion_unrolled,
            verifier_binaries::recursion_artifact(
                security,
                RecursionLayer::Unrolled,
                RecursionArtifact::Bin,
            ),
            UNROLLED_FAMILIES,
        )?;
        validate_level(
            "recursion-unified",
            &self.recursion_unified,
            verifier_binaries::recursion_artifact(
                security,
                RecursionLayer::Unified,
                RecursionArtifact::Bin,
            ),
            UNIFIED_FAMILIES,
        )
    }

    pub fn program_commitment(&self) -> [u32; 8] {
        let (chain, preimage) = UnrolledProgramSetup::begin_recursion_chain(&self.base.end_params);
        let (chain, preimage) = UnrolledProgramSetup::continue_recursion_chain(
            &self.recursion_unrolled.end_params,
            &chain,
            &preimage,
        );
        UnrolledProgramSetup::continue_recursion_chain(
            &self.recursion_unified.end_params,
            &chain,
            &preimage,
        )
        .0
    }
}

fn validate_level(
    name: &str,
    setup: &UnrolledProgramSetup,
    binary: &[u8],
    families: &[u8],
) -> Result<(), String> {
    let invalid = |field| format!("{name} setup summary {field} mismatch");
    if !setup
        .circuit_families_setups
        .keys()
        .copied()
        .eq(families.iter().copied())
    {
        return Err(invalid("circuit families"));
    }
    if binary.len() % 4 != 0 || binary.len() > riscv_transpiler::common_constants::ROM_WORD_SIZE * 4
    {
        return Err(invalid("binary length"));
    }
    let binary = padded(binary);
    let words: Vec<_> = binary
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect();
    let mut exits = words
        .windows(crate::EXIT_SEQUENCE.len())
        .enumerate()
        .filter(|(_, window)| *window == crate::EXIT_SEQUENCE);
    let Some((offset, _)) = exits.next() else {
        return Err(invalid("exit sequence"));
    };
    if exits.next().is_some() {
        return Err(invalid("exit sequence"));
    }
    let final_pc = ((offset + crate::EXIT_SEQUENCE.len() - 1) * 4) as u32;
    if setup.expected_final_pc != final_pc {
        return Err(invalid("final PC"));
    }
    let binary_hash: [u8; 32] = sha3::Keccak256::digest(&binary).into();
    if setup.binary_hash != binary_hash {
        return Err(invalid("binary hash"));
    }
    let caps: Vec<_> = setup.circuit_families_setups.values().collect();
    let end_params = if families == UNIFIED_FAMILIES {
        if !setup
            .inits_and_teardowns_setup
            .iter()
            .all(|cap| cap.cap.iter().all(|word| *word == [0; 8]))
        {
            return Err(invalid("unified dummy caps"));
        }
        crate::compute_end_parameters_for_unified_circuit(final_pc, caps[0])
    } else {
        crate::compute_end_parameters_for_unrolled_circuits(
            final_pc,
            &caps,
            &setup.inits_and_teardowns_setup,
        )
    };
    if setup.end_params != end_params {
        return Err(invalid("end parameters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CAP_SIZE, NUM_COSETS};
    use verifier_common::prover::definitions::MerkleTreeCap;

    fn synthetic_summary(binary: &[u8], families: &[u8]) -> UnrolledProgramSetup {
        let caps = [MerkleTreeCap::<CAP_SIZE>::dummy(); NUM_COSETS];
        UnrolledProgramSetup::new_from_setups_and_binary(
            &padded(binary),
            &families.iter().map(|id| (*id, caps)).collect::<Vec<_>>(),
            &caps,
        )
    }

    fn fixture(security: SecurityModel) -> (ProgramSetupSummaries, Vec<u8>) {
        let binary: Vec<_> = crate::EXIT_SEQUENCE
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let embedded =
            |layer| verifier_binaries::recursion_artifact(security, layer, RecursionArtifact::Bin);
        let summaries = ProgramSetupSummaries {
            security_bits: security_bits(security),
            base: synthetic_summary(&binary, BASE_FAMILIES),
            recursion_unrolled: synthetic_summary(
                embedded(RecursionLayer::Unrolled),
                UNROLLED_FAMILIES,
            ),
            recursion_unified: synthetic_summary(
                embedded(RecursionLayer::Unified),
                UNIFIED_FAMILIES,
            ),
        };
        (summaries, binary)
    }

    #[test]
    fn setup_summaries_validate_both_security_shapes_and_chain() {
        for security in [SecurityModel::Security80, SecurityModel::Security100] {
            let (summaries, binary) = fixture(security);
            summaries.validate(security, &binary).unwrap();
            let encoded = serde_json::to_vec(&summaries).unwrap();
            let decoded: ProgramSetupSummaries = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.program_commitment(), summaries.program_commitment());
            let mut unknown = serde_json::to_value(&summaries).unwrap();
            unknown["unrecognized"] = serde_json::json!(true);
            assert!(serde_json::from_value::<ProgramSetupSummaries>(unknown).is_err());
        }
    }

    #[test]
    fn setup_summaries_reject_metadata_mutations() {
        let security = SecurityModel::Security100;
        let (original, binary) = fixture(security);
        assert!(original
            .validate(SecurityModel::Security80, &binary)
            .is_err());
        for mutate in [
            (|s: &mut ProgramSetupSummaries| s.base.expected_final_pc ^= 4)
                as fn(&mut ProgramSetupSummaries),
            |s| s.base.binary_hash[0] ^= 1,
            |s| s.base.end_params[0] ^= 1,
            |s| {
                s.base.circuit_families_setups.remove(&BASE_FAMILIES[0]);
            },
            |s| s.recursion_unrolled.end_params[0] ^= 1,
            |s| s.recursion_unified.inits_and_teardowns_setup[0].cap[0][0] ^= 1,
            |s| {
                s.recursion_unified
                    .circuit_families_setups
                    .get_mut(&UNIFIED_FAMILIES[0])
                    .unwrap()[0]
                    .cap[0][0] ^= 1
            },
        ] {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert!(changed.validate(security, &binary).is_err());
        }
        assert!(original.validate(security, &[]).is_err());
        assert!(original.validate(security, &[0]).is_err());
        let mut changed_binary = binary.clone();
        changed_binary.extend_from_slice(&binary);
        assert!(original.validate(security, &changed_binary).is_err());
    }
}
