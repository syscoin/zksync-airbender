use prover::common_constants;

use super::*;

fn dummy_bytecode() -> Vec<u32> {
    vec![0u32; common_constants::rom::ROM_WORD_SIZE]
}

pub(crate) fn add_sub_lui_auipc_mop_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::add_sub_lui_auipc_mop::get_circuit(&dummy_bytecode()),
        setups::add_sub_lui_auipc_mop::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn jump_branch_slt_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::jump_branch_slt::get_circuit(&dummy_bytecode()),
        setups::jump_branch_slt::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn inits_and_teardowns_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::inits_and_teardowns::get_circuit(&dummy_bytecode()),
        setups::inits_and_teardowns::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn load_store_subword_only_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::load_store_subword_only::get_circuit(&dummy_bytecode()),
        setups::load_store_subword_only::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn load_store_word_only_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::load_store_word_only::get_circuit(&dummy_bytecode()),
        setups::load_store_word_only::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn mul_div_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::mul_div::get_circuit(&dummy_bytecode()),
        setups::mul_div::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn mul_div_unsigned_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::mul_div_unsigned::get_circuit(&dummy_bytecode()),
        setups::mul_div_unsigned::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn shift_binary_csr_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::shift_binary_csr::get_circuit(&dummy_bytecode()),
        setups::shift_binary_csr::dump_ssa_form(&dummy_bytecode()),
    )
}

pub(crate) fn unified_reduced_machine_circuit_layout() -> (
    CompiledCircuitArtifact<Mersenne31Field>,
    Vec<Vec<RawExpression<Mersenne31Field>>>,
) {
    (
        setups::unified_reduced_machine::get_circuit(&dummy_bytecode()),
        setups::unified_reduced_machine::dump_ssa_form(&dummy_bytecode()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use prover::cs::definitions::ColumnAddress;
    use prover::field::{Field, PrimeField};

    #[test]
    fn unified_delegation_multiplicity_requires_executed_row() {
        // Compile the real unified relation, rather than constructing a toy CSR
        // constraint or relying on the honest witness generator to zero padding.
        let circuit = setups::unified_reduced_machine::get_circuit(&dummy_bytecode());
        let delegation = circuit
            .memory_layout
            .delegation_request_layout
            .expect("unified circuit must contain a delegation request");
        let state = circuit
            .memory_layout
            .intermediate_state_layout
            .expect("unified circuit must contain its execution-state permutation");
        assert_eq!(delegation.multiplicity.num_elements(), 1);
        assert_eq!(state.execute.num_elements(), 1);

        let multiplicity = ColumnAddress::MemorySubtree(delegation.multiplicity.start());
        let execute = ColumnAddress::MemorySubtree(state.execute.start());
        assert_ne!(multiplicity, execute);

        // Require the compiled relation m * (1 - execute) = 0 (up to a
        // nonzero scalar). No unrelated variable may account for rejection.
        // The pre-fix unified circuit fails here: this constraint is absent.
        let constraint = circuit
            .degree_2_constraints
            .iter()
            .find(|constraint| {
                let [(quadratic_coefficient, left, right)] = constraint.quadratic_terms.as_ref()
                else {
                    return false;
                };
                let [(linear_coefficient, linear_address)] = constraint.linear_terms.as_ref()
                else {
                    return false;
                };
                let mut coefficient_sum = *quadratic_coefficient;
                coefficient_sum.add_assign(linear_coefficient);

                constraint.constant_term.is_zero()
                    && !linear_coefficient.is_zero()
                    && *linear_address == multiplicity
                    && ((*left == multiplicity && *right == execute)
                        || (*left == execute && *right == multiplicity))
                    && coefficient_sum.is_zero()
            })
            .expect(
                "compiled unified circuit must constrain delegation multiplicity by row execution",
            );

        let witness_row = vec![Mersenne31Field::ZERO; circuit.witness_layout.total_width];
        let mut memory_row = vec![Mersenne31Field::ZERO; circuit.memory_layout.total_width];
        let cases = [
            (0, Mersenne31Field::ZERO, true),
            (1, Mersenne31Field::ZERO, true),
            (1, Mersenne31Field::ONE, true),
            (0, Mersenne31Field::ONE, false),
            (0, Mersenne31Field::from_u64_unchecked(2), false),
            (0, Mersenne31Field::from_u64_unchecked(7), false),
            (0, Mersenne31Field::MINUS_ONE, false),
        ];
        for (row_execute, request_multiplicity, satisfies_relation) in cases {
            memory_row[state.execute.start()] = Mersenne31Field::from_u64_unchecked(row_execute);
            memory_row[delegation.multiplicity.start()] = request_multiplicity;
            let residual = constraint.evaluate_at_row_on_main_domain(&witness_row, &memory_row);
            assert_eq!(
                residual.is_zero(),
                satisfies_relation,
                "compiled delegation gate: execute={row_execute}, multiplicity={request_multiplicity}, residual={residual}",
            );
        }
        // These are adversarial assignments to the selected compiled relation,
        // not a claim that the zero-filled rows satisfy every other constraint.
    }
}
