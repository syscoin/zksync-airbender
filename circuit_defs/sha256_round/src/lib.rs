#![allow(incomplete_features)]
#![feature(allocator_api)]
#![feature(generic_const_exprs)]

// SYSCOIN: Canonical circuit description for delegation CSR 0x7cb.

use prover::cs;
use prover::cs::cs::witness_placer::graph_description::RawExpression;
use prover::fft::GoodAllocator;
use prover::field::Mersenne31Field;
use prover::tracers::oracles::delegation_oracle::DelegationCircuitOracle;
use prover::*;

pub const DELEGATION_TYPE_ID: u32 = sha256_u32::SYSCOIN_SHA256_ROUND_FUNCTION_ACCESS_ID;
// The compiler uses 19-bit timestamp limbs and requires log2(domain) > 19.
pub const DOMAIN_SIZE: usize = 1 << 20;
pub const NUM_DELEGATION_CYCLES: usize = DOMAIN_SIZE - 1;
pub const LDE_FACTOR: usize = 2;
pub const LDE_SOURCE_COSETS: &[usize] = &[0, 1];
pub const TREE_CAP_SIZE: usize = 32;

pub fn get_delegation_circuit() -> DelegationProcessorDescription {
    use cs::cs::circuit::Circuit;
    use cs::cs::cs_reference::BasicAssembly;
    use cs::delegation::sha256_round::define_sha256_round_delegation_circuit;
    use cs::one_row_compiler::OneRowCompiler;

    let mut cs = BasicAssembly::<Mersenne31Field>::new();
    define_sha256_round_delegation_circuit(&mut cs);
    let (circuit_output, _) = cs.finalize();
    let table_driver = circuit_output.table_driver.clone();
    let compiled_circuit = OneRowCompiler::default()
        .compile_to_evaluate_delegations(circuit_output, DOMAIN_SIZE.trailing_zeros() as usize);

    DelegationProcessorDescription {
        delegation_type: DELEGATION_TYPE_ID,
        num_requests_per_circuit: NUM_DELEGATION_CYCLES,
        trace_len: DOMAIN_SIZE,
        table_driver,
        compiled_circuit,
    }
}

pub fn get_ssa_form() -> Vec<Vec<RawExpression<Mersenne31Field>>> {
    use cs::cs::circuit::Circuit;
    use cs::cs::cs_reference::BasicAssembly;
    use cs::cs::witness_placer::graph_description::WitnessGraphCreator;
    use cs::delegation::sha256_round::define_sha256_round_delegation_circuit;

    let mut cs = BasicAssembly::<Mersenne31Field, WitnessGraphCreator<Mersenne31Field>>::new();
    cs.witness_placer = Some(WitnessGraphCreator::<Mersenne31Field>::new());
    define_sha256_round_delegation_circuit(&mut cs);
    // SYSCOIN: `finalize` appends witness computations for packed RC8 lookups. Taking the graph directly
    // from the assembly would omit those mappings while the compiled circuit still constrains
    // them, producing an invalid stage-2 lookup witness.
    let (_, graph) = cs.finalize();
    let graph = graph.expect("witness graph creator must be returned by finalize");
    let (_, forms) = graph.compute_resolution_order();
    forms
}

pub fn get_table_driver() -> prover::cs::tables::TableDriver<Mersenne31Field> {
    cs::delegation::sha256_round::sha256_round_delegation_circuit_create_table_driver()
}

mod sealed {
    use crate::Mersenne31Field;
    use prover::cs::cs::witness_placer::*;
    use prover::witness_proxy::WitnessProxy;

    include!("../generated/witness_generation_fn.rs");
}

pub fn witness_eval_fn_for_gpu_tracer<'a, 'b>(
    proxy: &'_ mut SimpleWitnessProxy<'a, DelegationCircuitOracle<'b, impl GoodAllocator>>,
) {
    use cs::cs::witness_placer::scalar_witness_type_set::ScalarWitnessTypeSet;

    let fn_ptr = sealed::evaluate_witness_fn::<
        ScalarWitnessTypeSet<Mersenne31Field, true>,
        SimpleWitnessProxy<'a, DelegationCircuitOracle<'b, _>>,
    >;
    (fn_ptr)(proxy);
}

#[cfg(test)]
mod tests {
    use super::*;
    use prover::cs::cs::circuit::Circuit;
    use prover::cs::cs::cs_reference::BasicAssembly;
    use prover::cs::definitions::{
        timestamp_from_chunk_cycle_and_sequence, TimestampData, NUM_DELEGATION_ARGUMENT_KEY_PARTS,
        NUM_MEM_ARGUMENT_KEY_PARTS,
    };
    use prover::cs::types::{Num, Register};
    use prover::definitions::{
        ExternalChallenges, ExternalDelegationArgumentChallenges, ExternalMemoryArgumentChallenges,
        ExternalValues,
    };
    use prover::fft::{materialize_powers_serial_starting_with_elem, LdePrecomputations, Twiddles};
    use prover::field::{Field, FieldExtension, Mersenne31Complex, Mersenne31Quartic};
    use prover::prover_stages::{prove, SetupPrecomputations};
    use prover::risc_v_simulator::abstractions::non_determinism::QuasiUARTSource;
    use prover::risc_v_simulator::abstractions::tracer::{
        RegisterOrIndirectReadData, RegisterOrIndirectReadWriteData,
    };
    use prover::risc_v_simulator::cycle::state_new::RiscV32StateForUnrolledProver;
    use prover::risc_v_simulator::cycle::IMStandardIsaConfig;
    use prover::risc_v_simulator::delegations::DelegationsCSRProcessor;
    use prover::tracers::delegation::{
        sha256_round_factory_fn, DelegationWitness, IndirectAccessLocation,
    };
    use prover::tracers::main_cycle_optimized::{
        DelegationTracingData, GPUFriendlyTracer, RamTracingData,
    };
    use prover::tracers::oracles::delegation_oracle::DelegationCircuitOracle;
    use prover::worker::Worker;
    use prover::VectorMemoryImplWithRom;
    use sha256_round_verifier::{ConcreteProofOutput, ConcreteProofPublicInputs};
    use std::alloc::Global;
    use std::collections::HashMap;
    use std::mem::MaybeUninit;
    use verifier_common::proof_flattener::{flatten_proof_for_skeleton, flatten_query};
    use verifier_common::prover::nd_source_std::set_iterator;

    mod canonical_delegation_parameters {
        use prover::definitions::MerkleTreeCap;

        include!("../../setups/generated/all_delegation_circuits_params.rs");
    }

    #[test]
    fn circuit_identity_and_domain_are_canonical() {
        let circuit = get_delegation_circuit();
        assert_eq!(circuit.delegation_type, 0x7cb);
        assert_eq!(circuit.trace_len, 1 << 20);
        assert_eq!(circuit.num_requests_per_circuit, (1 << 20) - 1);
        let generated: serde_json::Value =
            serde_json::from_str(include_str!("../generated/layout")).unwrap();
        let compiled = serde_json::to_value(&circuit.compiled_circuit).unwrap();
        assert_eq!(
            compiled, generated,
            "runtime and generated layouts diverged"
        );
    }

    fn check_round_case(input: [u32; 8], w: u32, k: u32) -> [u32; 8] {
        const STATE_ADDRESS: u32 = 1 << 21;
        const SCHEDULE_ADDRESS: u32 = STATE_ADDRESS + 64;

        let mut expected = input;
        sha256_u32::delegated_round(&mut expected, w, k);

        let registers = [
            RegisterOrIndirectReadWriteData {
                read_value: STATE_ADDRESS,
                write_value: STATE_ADDRESS,
                timestamp: TimestampData::from_scalar(100),
            },
            RegisterOrIndirectReadWriteData {
                read_value: SCHEDULE_ADDRESS,
                write_value: SCHEDULE_ADDRESS,
                timestamp: TimestampData::from_scalar(101),
            },
            RegisterOrIndirectReadWriteData {
                read_value: k,
                write_value: k,
                timestamp: TimestampData::from_scalar(102),
            },
        ];
        let indirect_reads = [RegisterOrIndirectReadData {
            read_value: w,
            timestamp: TimestampData::from_scalar(99),
        }];
        let indirect_writes = std::array::from_fn::<_, 8, _>(|i| RegisterOrIndirectReadWriteData {
            read_value: input[i],
            write_value: expected[i],
            timestamp: TimestampData::from_scalar(99),
        });
        let witness: &'static DelegationWitness = Box::leak(Box::new(DelegationWitness {
            num_requests: NUM_DELEGATION_CYCLES,
            num_register_accesses_per_delegation: 3,
            num_indirect_reads_per_delegation: 1,
            num_indirect_writes_per_delegation: 8,
            base_register_index: 10,
            delegation_type: DELEGATION_TYPE_ID as u16,
            indirect_accesses_properties: vec![
                (0..8)
                    .map(|index| IndirectAccessLocation {
                        use_writes: true,
                        index,
                    })
                    .collect(),
                vec![IndirectAccessLocation {
                    use_writes: false,
                    index: 0,
                }],
            ],
            write_timestamp: vec![TimestampData::from_scalar(103)],
            register_accesses: registers.to_vec(),
            indirect_reads: indirect_reads.to_vec(),
            indirect_writes: indirect_writes.to_vec(),
        }));
        let oracle = DelegationCircuitOracle {
            cycle_data: witness,
        };
        let mut cs = BasicAssembly::<Mersenne31Field>::new_with_oracle(oracle);
        let output = cs::delegation::sha256_round::define_sha256_round_delegation_circuit(&mut cs);
        let got: [u32; 8] = output
            .into_iter()
            .map(|word| {
                Register::<Mersenne31Field>(word.map(Num::Var))
                    .get_value_unsigned(&cs)
                    .unwrap()
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert_eq!(got, expected);
        let _ = cs.finalize();
        got
    }

    fn single_round_witness(
        state_address: u32,
        schedule_address: u32,
        input: [u32; 8],
        w: u32,
        k: u32,
    ) -> DelegationWitness {
        let mut expected = input;
        sha256_u32::delegated_round(&mut expected, w, k);
        DelegationWitness {
            num_requests: NUM_DELEGATION_CYCLES,
            num_register_accesses_per_delegation: 3,
            num_indirect_reads_per_delegation: 1,
            num_indirect_writes_per_delegation: 8,
            base_register_index: 10,
            delegation_type: DELEGATION_TYPE_ID as u16,
            indirect_accesses_properties: vec![
                (0..8)
                    .map(|index| IndirectAccessLocation {
                        use_writes: true,
                        index,
                    })
                    .collect(),
                vec![IndirectAccessLocation {
                    use_writes: false,
                    index: 0,
                }],
            ],
            write_timestamp: vec![TimestampData::from_scalar(103)],
            register_accesses: vec![
                RegisterOrIndirectReadWriteData {
                    read_value: state_address,
                    write_value: state_address,
                    timestamp: TimestampData::from_scalar(100),
                },
                RegisterOrIndirectReadWriteData {
                    read_value: schedule_address,
                    write_value: schedule_address,
                    timestamp: TimestampData::from_scalar(101),
                },
                RegisterOrIndirectReadWriteData {
                    read_value: k,
                    write_value: k,
                    timestamp: TimestampData::from_scalar(102),
                },
            ],
            indirect_reads: vec![RegisterOrIndirectReadData {
                read_value: w,
                timestamp: TimestampData::from_scalar(99),
            }],
            indirect_writes: input
                .into_iter()
                .zip(expected)
                .map(
                    |(read_value, write_value)| RegisterOrIndirectReadWriteData {
                        read_value,
                        write_value,
                        timestamp: TimestampData::from_scalar(99),
                    },
                )
                .collect(),
        }
    }

    fn lookup_expression_value(
        expression: &prover::cs::definitions::LookupExpression<Mersenne31Field>,
        witness_row: &[Mersenne31Field],
        memory_row: &[Mersenne31Field],
    ) -> Mersenne31Field {
        match expression {
            prover::cs::definitions::LookupExpression::Variable(place) => match place {
                prover::cs::one_row_compiler::ColumnAddress::WitnessSubtree(offset) => {
                    witness_row[*offset]
                }
                prover::cs::one_row_compiler::ColumnAddress::MemorySubtree(offset) => {
                    memory_row[*offset]
                }
                other => panic!("unsupported lookup column in SHA test: {other:?}"),
            },
            prover::cs::definitions::LookupExpression::Expression(expression) => {
                expression.evaluate_at_row_on_main_domain(witness_row, memory_row)
            }
        }
    }

    fn row_satisfies_constraints_and_lookups(
        circuit: &DelegationProcessorDescription,
        witness: &DelegationWitness,
    ) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let oracle = DelegationCircuitOracle {
                cycle_data: witness,
            };
            let (witness_row, memory_row, lookup_mapping) = evaluate_witness_row_for_testing(
                &circuit.compiled_circuit,
                witness_eval_fn_for_gpu_tracer,
                &oracle,
                &circuit.table_driver,
                0,
            );
            if !check_satisfied_row(&circuit.compiled_circuit, &witness_row, &memory_row, 0) {
                return false;
            }

            // `check_satisfied_row` checks polynomial identities but not table membership.
            // SYSCOIN: cover both the dedicated RC16 argument and every fixed width-three lookup
            // so address-floor and modular-delta negatives cannot pass through an invalid lookup.
            if circuit
                .compiled_circuit
                .witness_layout
                .range_check_16_lookup_expressions
                .iter()
                .any(|expression| {
                    lookup_expression_value(expression, &witness_row, &memory_row).0 >= 1 << 16
                })
            {
                return false;
            }
            if lookup_mapping.len()
                != circuit
                    .compiled_circuit
                    .witness_layout
                    .width_3_lookups
                    .len()
            {
                return false;
            }
            for (index, lookup) in circuit
                .compiled_circuit
                .witness_layout
                .width_3_lookups
                .iter()
                .enumerate()
            {
                let inputs: [Mersenne31Field; 3] = std::array::from_fn(|input_index| {
                    lookup_expression_value(
                        &lookup.input_columns[input_index],
                        &witness_row,
                        &memory_row,
                    )
                });
                let prover::cs::definitions::TableIndex::Constant(table_type) = lookup.table_index
                else {
                    return false;
                };
                let expected = circuit
                    .table_driver
                    .enforce_values_and_get_absolute_index(&inputs, table_type.to_table_id());
                if lookup_mapping[index] != expected as u32 {
                    return false;
                }
            }
            true
        }))
        .unwrap_or(false)
    }

    #[test]
    fn circuit_matches_reference_across_boundaries_and_random_inputs() {
        check_round_case([0; 8], 0, 0);
        check_round_case([u32::MAX; 8], u32::MAX, u32::MAX);
        check_round_case(sha256_u32::IV, 0x8000_0000, sha256_u32::ROUND_CONSTANTS[0]);

        let mut seed = 0x1319_8a2e_0370_7344u64;
        for _ in 0..29 {
            let mut state = [0u32; 8];
            for value in &mut state {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *value = seed as u32;
            }
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let w = seed as u32;
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            check_round_case(state, w, seed as u32);
        }
    }

    #[test]
    fn circuit_address_guards_match_the_canonical_abi() {
        const RAM_FLOOR: u32 = 1 << 21;
        let circuit = get_delegation_circuit();
        let input = sha256_u32::IV;
        let w = 0x6162_6380;
        let k = sha256_u32::ROUND_CONSTANTS[0];

        assert!(row_satisfies_constraints_and_lookups(
            &circuit,
            &single_round_witness(RAM_FLOOR, RAM_FLOOR + 32, input, w, k),
        ));
        assert!(row_satisfies_constraints_and_lookups(
            &circuit,
            &single_round_witness(RAM_FLOOR + 64, RAM_FLOOR + 60, input, w, k),
        ));
        assert!(!row_satisfies_constraints_and_lookups(
            &circuit,
            &single_round_witness(RAM_FLOOR - 4, RAM_FLOOR + 64, input, w, k),
        ));
        assert!(!row_satisfies_constraints_and_lookups(
            &circuit,
            &single_round_witness(RAM_FLOOR, RAM_FLOOR - 4, input, w, k),
        ));
        for offset in (0..32).step_by(4) {
            assert!(!row_satisfies_constraints_and_lookups(
                &circuit,
                &single_round_witness(RAM_FLOOR, RAM_FLOOR + offset, input, w, k),
            ));
        }
        assert!(row_satisfies_constraints_and_lookups(
            &circuit,
            &single_round_witness(0xffff_ffe0, 0xffff_ffdc, input, w, k),
        ));
        assert!(!row_satisfies_constraints_and_lookups(
            &circuit,
            &single_round_witness(0xffff_ffe4, 0xffff_ffe0, input, w, k),
        ));
    }

    #[test]
    fn dispatcher_trace_and_compiled_witness_reject_mutated_write() {
        const TEST_CYCLES: usize = 3;
        const ROM_BOUND: usize = 1 << 20;
        const RAM_SIZE: usize = 1 << 22;
        const STATE_ADDRESS: u32 = 1 << 21;
        const SCHEDULE_ADDRESS: u32 = STATE_ADDRESS + 64;
        // csrrw x0, 0x7cb, x0
        const SHA256_ROUND_OPCODE: u32 = 0x7cb0_1073;

        let input = sha256_u32::IV;
        let w = 0x6162_6380;
        let k = sha256_u32::ROUND_CONSTANTS[0];
        let mut expected = input;
        sha256_u32::delegated_round(&mut expected, w, k);

        let mut memory = VectorMemoryImplWithRom::new_for_byte_size(RAM_SIZE, ROM_BOUND);
        memory.populate(0, SHA256_ROUND_OPCODE);
        for (i, word) in input.into_iter().enumerate() {
            memory.populate(STATE_ADDRESS + (i * 4) as u32, word);
        }
        memory.populate(SCHEDULE_ADDRESS, w);

        let delegation_type = DELEGATION_TYPE_ID as u16;
        let mut factories: HashMap<u16, Box<dyn Fn() -> DelegationWitness>> = HashMap::new();
        factories.insert(
            delegation_type,
            Box::new(move || sha256_round_factory_fn(delegation_type, TEST_CYCLES, Global)),
        );
        let delegation_tracer = DelegationTracingData {
            current_per_type_logs: HashMap::new(),
            num_traced_registers: 0,
            mem_reads_offset: 0,
            mem_writes_offset: 0,
            all_per_type_logs: HashMap::new(),
            delegation_witness_factories: factories,
        };
        let ram_tracer =
            RamTracingData::<true>::new_for_ram_size_and_rom_bound(RAM_SIZE, ROM_BOUND);
        let initial_timestamp = timestamp_from_chunk_cycle_and_sequence(0, TEST_CYCLES, 0);
        let mut tracer: GPUFriendlyTracer<IMStandardIsaConfig> = GPUFriendlyTracer::new(
            initial_timestamp,
            ram_tracer,
            delegation_tracer,
            TEST_CYCLES,
            1,
        );

        let mut state = RiscV32StateForUnrolledProver::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = STATE_ADDRESS;
        state.observable.registers[11] = SCHEDULE_ADDRESS;
        state.observable.registers[12] = k;
        let mut non_determinism = QuasiUARTSource::default();
        let mut csr_processor = DelegationsCSRProcessor;
        state.cycle(
            &mut memory,
            &mut tracer,
            &mut non_determinism,
            &mut csr_processor,
        );

        let witness = tracer
            .delegation_tracer
            .current_per_type_logs
            .remove(&delegation_type)
            .expect("CSR 0x7cb must reach the SHA delegation tracer");
        witness.assert_consistency();
        assert_eq!(witness.write_timestamp.len(), 1);
        assert_eq!(witness.num_register_accesses_per_delegation, 3);
        assert_eq!(witness.num_indirect_reads_per_delegation, 1);
        assert_eq!(witness.num_indirect_writes_per_delegation, 8);
        assert_eq!(witness.register_accesses[0].read_value, STATE_ADDRESS);
        assert_eq!(witness.register_accesses[1].read_value, SCHEDULE_ADDRESS);
        assert_eq!(witness.register_accesses[2].read_value, k);
        assert_eq!(witness.indirect_reads[0].read_value, w);
        for i in 0..8 {
            assert_eq!(witness.indirect_writes[i].read_value, input[i]);
            assert_eq!(witness.indirect_writes[i].write_value, expected[i]);
        }

        let circuit = get_delegation_circuit();
        let oracle = DelegationCircuitOracle {
            cycle_data: &witness,
        };
        let (witness_row, memory_row, lookup_mapping) = evaluate_witness_row_for_testing(
            &circuit.compiled_circuit,
            witness_eval_fn_for_gpu_tracer,
            &oracle,
            &circuit.table_driver,
            0,
        );
        assert!(check_satisfied_row(
            &circuit.compiled_circuit,
            &witness_row,
            &memory_row,
            0,
        ));
        assert_eq!(
            lookup_mapping.len(),
            circuit
                .compiled_circuit
                .witness_layout
                .width_3_lookups
                .len()
        );
        assert_eq!(lookup_mapping.len(), 45);
        for (index, lookup) in circuit
            .compiled_circuit
            .witness_layout
            .width_3_lookups
            .iter()
            .enumerate()
        {
            let inputs: [Mersenne31Field; 3] =
                std::array::from_fn(|input_index| match &lookup.input_columns[input_index] {
                    prover::cs::definitions::LookupExpression::Variable(place) => match place {
                        prover::cs::one_row_compiler::ColumnAddress::WitnessSubtree(offset) => {
                            witness_row[*offset]
                        }
                        prover::cs::one_row_compiler::ColumnAddress::MemorySubtree(offset) => {
                            memory_row[*offset]
                        }
                        other => panic!("unsupported lookup column in SHA test: {other:?}"),
                    },
                    prover::cs::definitions::LookupExpression::Expression(expression) => {
                        expression.evaluate_at_row_on_main_domain(&witness_row, &memory_row)
                    }
                });
            let prover::cs::definitions::TableIndex::Constant(table_type) = lookup.table_index
            else {
                panic!("SHA delegation uses only fixed lookup tables")
            };
            let expected = circuit
                .table_driver
                .enforce_values_and_get_absolute_index(&inputs, table_type.to_table_id());
            assert_eq!(
                lookup_mapping[index], expected as u32,
                "generated witness omitted or mis-mapped lookup {index}"
            );
        }
        let first_rc8_lookup = lookup_mapping.len() - 4;
        for index in first_rc8_lookup..lookup_mapping.len() {
            assert_eq!(
                circuit.compiled_circuit.witness_layout.width_3_lookups[index].table_index,
                prover::cs::definitions::TableIndex::Constant(
                    prover::cs::tables::TableType::RangeCheckSmall
                )
            );
        }
        assert_eq!(
            circuit
                .compiled_circuit
                .witness_layout
                .width_3_lookups
                .iter()
                .filter(|lookup| {
                    lookup.table_index
                        == prover::cs::definitions::TableIndex::Constant(
                            prover::cs::tables::TableType::ExtractLower5Bits,
                        )
                })
                .count(),
            1,
        );

        let mut mutated = witness.clone();
        mutated.indirect_writes[3].write_value ^= 1;
        let oracle = DelegationCircuitOracle {
            cycle_data: &mutated,
        };
        let (witness_row, memory_row, _) = evaluate_witness_row_for_testing(
            &circuit.compiled_circuit,
            witness_eval_fn_for_gpu_tracer,
            &oracle,
            &circuit.table_driver,
            0,
        );
        assert!(!check_satisfied_row(
            &circuit.compiled_circuit,
            &witness_row,
            &memory_row,
            0,
        ));

        let mut mutated = witness.clone();
        mutated.indirect_reads[0].read_value ^= 1;
        let oracle = DelegationCircuitOracle {
            cycle_data: &mutated,
        };
        let (witness_row, memory_row, _) = evaluate_witness_row_for_testing(
            &circuit.compiled_circuit,
            witness_eval_fn_for_gpu_tracer,
            &oracle,
            &circuit.table_driver,
            0,
        );
        assert!(!check_satisfied_row(
            &circuit.compiled_circuit,
            &witness_row,
            &memory_row,
            0,
        ));
    }

    #[test]
    fn padding_row_executes_with_zero_inputs_and_zero_writes() {
        let witness: &'static DelegationWitness = Box::leak(Box::new(DelegationWitness {
            num_requests: NUM_DELEGATION_CYCLES,
            num_register_accesses_per_delegation: 3,
            num_indirect_reads_per_delegation: 1,
            num_indirect_writes_per_delegation: 8,
            base_register_index: 10,
            delegation_type: DELEGATION_TYPE_ID as u16,
            indirect_accesses_properties: vec![
                (0..8)
                    .map(|index| IndirectAccessLocation {
                        use_writes: true,
                        index,
                    })
                    .collect(),
                vec![IndirectAccessLocation {
                    use_writes: false,
                    index: 0,
                }],
            ],
            write_timestamp: vec![],
            register_accesses: vec![],
            indirect_reads: vec![],
            indirect_writes: vec![],
        }));
        let oracle = DelegationCircuitOracle {
            cycle_data: witness,
        };
        let mut cs = BasicAssembly::<Mersenne31Field>::new_with_oracle(oracle);
        let output = cs::delegation::sha256_round::define_sha256_round_delegation_circuit(&mut cs);
        for word in output {
            assert_eq!(
                Register::<Mersenne31Field>(word.map(Num::Var)).get_value_unsigned(&cs),
                Some(0)
            );
        }
        let _ = cs.finalize();
        assert!(row_satisfies_constraints_and_lookups(
            &get_delegation_circuit(),
            witness,
        ));
    }

    /// This is intentionally ignored in the default unit-test suite: it materializes and proves
    /// the complete 2^20-row production circuit. Run it before freezing generated verifier/VK
    /// artifacts with:
    ///
    /// `cargo test --release -p sha256_round cpu_proves_and_generated_verifier_accepts_sha_round -- --ignored --nocapture`
    #[test]
    #[ignore = "full 2^20-row CPU proof"]
    fn cpu_proves_and_generated_verifier_accepts_sha_round() {
        const STATE_ADDRESS: u32 = 1 << 21;
        const SCHEDULE_ADDRESS: u32 = STATE_ADDRESS + 64;

        let input = sha256_u32::IV;
        let w = 0x6162_6380;
        let k = sha256_u32::ROUND_CONSTANTS[0];
        let mut expected = input;
        sha256_u32::delegated_round(&mut expected, w, k);

        let registers = [
            RegisterOrIndirectReadWriteData {
                read_value: STATE_ADDRESS,
                write_value: STATE_ADDRESS,
                timestamp: TimestampData::from_scalar(100),
            },
            RegisterOrIndirectReadWriteData {
                read_value: SCHEDULE_ADDRESS,
                write_value: SCHEDULE_ADDRESS,
                timestamp: TimestampData::from_scalar(101),
            },
            RegisterOrIndirectReadWriteData {
                read_value: k,
                write_value: k,
                timestamp: TimestampData::from_scalar(102),
            },
        ];
        let indirect_reads = [RegisterOrIndirectReadData {
            read_value: w,
            timestamp: TimestampData::from_scalar(99),
        }];
        let indirect_writes = std::array::from_fn::<_, 8, _>(|i| RegisterOrIndirectReadWriteData {
            read_value: input[i],
            write_value: expected[i],
            timestamp: TimestampData::from_scalar(99),
        });
        let witness = DelegationWitness {
            num_requests: NUM_DELEGATION_CYCLES,
            num_register_accesses_per_delegation: 3,
            num_indirect_reads_per_delegation: 1,
            num_indirect_writes_per_delegation: 8,
            base_register_index: 10,
            delegation_type: DELEGATION_TYPE_ID as u16,
            indirect_accesses_properties: vec![
                (0..8)
                    .map(|index| IndirectAccessLocation {
                        use_writes: true,
                        index,
                    })
                    .collect(),
                vec![IndirectAccessLocation {
                    use_writes: false,
                    index: 0,
                }],
            ],
            write_timestamp: vec![TimestampData::from_scalar(103)],
            register_accesses: registers.to_vec(),
            indirect_reads: indirect_reads.to_vec(),
            indirect_writes: indirect_writes.to_vec(),
        };
        let oracle = DelegationCircuitOracle {
            cycle_data: &witness,
        };

        let circuit = get_delegation_circuit();
        let worker = Worker::new();
        let witness_data = evaluate_witness(
            &circuit.compiled_circuit,
            witness_eval_fn_for_gpu_tracer,
            NUM_DELEGATION_CYCLES,
            &oracle,
            &[],
            &circuit.table_driver,
            0,
            &worker,
            Global,
        );
        assert!(check_satisfied(
            &circuit.compiled_circuit,
            &witness_data.exec_trace,
            witness_data.num_witness_columns,
        ));

        let twiddles: Twiddles<_, Global> = Twiddles::new(DOMAIN_SIZE, &worker);
        let lde_precomputations =
            LdePrecomputations::new(DOMAIN_SIZE, LDE_FACTOR, LDE_SOURCE_COSETS, &worker);
        let setup = SetupPrecomputations::from_tables_and_trace_len(
            &circuit.table_driver,
            DOMAIN_SIZE,
            &circuit.compiled_circuit.setup_layout,
            &twiddles,
            &lde_precomputations,
            LDE_FACTOR,
            TREE_CAP_SIZE,
            &worker,
        );

        let memory_argument_alpha = Mersenne31Quartic::from_array_of_base([
            Mersenne31Field(2),
            Mersenne31Field(5),
            Mersenne31Field(42),
            Mersenne31Field(123),
        ]);
        let memory_argument_gamma = Mersenne31Quartic::from_array_of_base([
            Mersenne31Field(11),
            Mersenne31Field(7),
            Mersenne31Field(1024),
            Mersenne31Field(8000),
        ]);
        let memory_argument_linearization_challenges =
            materialize_powers_serial_starting_with_elem::<_, Global>(
                memory_argument_alpha,
                NUM_MEM_ARGUMENT_KEY_PARTS - 1,
            )
            .try_into()
            .unwrap();

        let delegation_argument_alpha = Mersenne31Quartic::from_array_of_base([
            Mersenne31Field(5),
            Mersenne31Field(8),
            Mersenne31Field(32),
            Mersenne31Field(16),
        ]);
        let delegation_argument_gamma = Mersenne31Quartic::from_array_of_base([
            Mersenne31Field(200),
            Mersenne31Field(100),
            Mersenne31Field(300),
            Mersenne31Field(400),
        ]);
        let delegation_argument_linearization_challenges =
            materialize_powers_serial_starting_with_elem::<_, Global>(
                delegation_argument_alpha,
                NUM_DELEGATION_ARGUMENT_KEY_PARTS - 1,
            )
            .try_into()
            .unwrap();
        let external_values = ExternalValues {
            challenges: ExternalChallenges {
                memory_argument: ExternalMemoryArgumentChallenges {
                    memory_argument_linearization_challenges,
                    memory_argument_gamma,
                },
                delegation_argument: Some(ExternalDelegationArgumentChallenges {
                    delegation_argument_linearization_challenges,
                    delegation_argument_gamma,
                }),
            },
            aux_boundary_values: Default::default(),
        };

        let now = std::time::Instant::now();
        let (prover_data, proof) = prove::<DEFAULT_TRACE_PADDING_MULTIPLE, _>(
            &circuit.compiled_circuit,
            &[],
            &external_values,
            witness_data,
            &setup,
            &twiddles,
            &lde_precomputations,
            0,
            Some(DELEGATION_TYPE_ID as u16),
            LDE_FACTOR,
            TREE_CAP_SIZE,
            53,
            28,
            &worker,
        );
        eprintln!("SHA-256 delegation CPU proving time: {:?}", now.elapsed());

        // Compare the generated quotient evaluator directly against the prover's opening at the
        // Fiat-Shamir point. Unlike a raw coset-row comparison, these are already ordinary
        // polynomial values rather than Airbender's grinded LDE representation.
        {
            let stage_2_num_openings = circuit
                .compiled_circuit
                .stage_2_layout
                .num_base_field_polys()
                + circuit
                    .compiled_circuit
                    .stage_2_layout
                    .num_ext4_field_polys();
            let (setup_values, rest) = proof
                .evaluations_at_random_points
                .split_at(circuit.compiled_circuit.setup_layout.total_width);
            let (witness, rest) =
                rest.split_at(circuit.compiled_circuit.witness_layout.total_width);
            let (memory, rest) = rest.split_at(circuit.compiled_circuit.memory_layout.total_width);
            let (stage_2, rest) = rest.split_at(stage_2_num_openings);
            let (quotient_opening, openings_at_z_omega) = rest.split_first().unwrap();

            let mut witness_next =
                vec![Mersenne31Quartic::ZERO; circuit.compiled_circuit.witness_layout.total_width];
            let memory_next =
                vec![Mersenne31Quartic::ZERO; circuit.compiled_circuit.memory_layout.total_width];
            let mut stage_2_next = vec![Mersenne31Quartic::ZERO; stage_2_num_openings];
            let (witness_next_openings, rest) = openings_at_z_omega
                .split_at(circuit.compiled_circuit.state_linkage_constraints.len());
            for ((_, destination), opening) in circuit
                .compiled_circuit
                .state_linkage_constraints
                .iter()
                .zip(witness_next_openings)
            {
                let prover::cs::one_row_compiler::ColumnAddress::WitnessSubtree(index) =
                    destination
                else {
                    unreachable!()
                };
                witness_next[*index] = *opening;
            }
            assert!(circuit
                .compiled_circuit
                .memory_layout
                .shuffle_ram_inits_and_teardowns
                .is_none());
            let (stage_2_next_opening, rest) = rest.split_first().unwrap();
            assert!(rest.is_empty());
            let grand_product_offset = circuit
                .compiled_circuit
                .stage_2_layout
                .intermediate_polys_for_memory_argument
                .num_elements()
                - 1;
            let grand_product_index = circuit
                .compiled_circuit
                .stage_2_layout
                .get_intermediate_polys_for_memory_argument_absolute_poly_idx_for_verifier(
                    grand_product_offset,
                );
            stage_2_next[grand_product_index] = *stage_2_next_opening;

            let x = prover_data.deep_poly_result.z;

            let omega =
                Mersenne31Complex::TWO_ADICITY_GENERATORS[DOMAIN_SIZE.trailing_zeros() as usize];
            let omega_inv = Mersenne31Complex::TWO_ADICITY_GENERATORS_INVERSED
                [DOMAIN_SIZE.trailing_zeros() as usize];
            let omega_inv_squared = Mersenne31Complex::TWO_ADICITY_GENERATORS_INVERSED
                [DOMAIN_SIZE.trailing_zeros() as usize - 1];
            let mut vanishing = x;
            vanishing.exp_power_of_2(DOMAIN_SIZE.trailing_zeros() as usize);
            vanishing.sub_assign_base(&Mersenne31Field::ONE);
            let vanishing_inv = vanishing.inverse().unwrap();
            let mut everywhere_except_last = x;
            everywhere_except_last.sub_assign_base(&omega_inv);
            everywhere_except_last.mul_assign(&vanishing_inv);
            let mut everywhere_except_last_two_rows = everywhere_except_last;
            let mut x_minus_omega_inv_squared = x;
            x_minus_omega_inv_squared.sub_assign_base(&omega_inv_squared);
            everywhere_except_last_two_rows.mul_assign(&x_minus_omega_inv_squared);
            let mut first_row = x;
            first_row.sub_assign_base(&Mersenne31Field::ONE);
            first_row = first_row.inverse().unwrap();
            let mut one_before_last_row = x;
            one_before_last_row.sub_assign_base(&omega_inv_squared);
            one_before_last_row = one_before_last_row.inverse().unwrap();
            let mut last_row = x;
            last_row.sub_assign_base(&omega_inv);
            last_row = last_row.inverse().unwrap();
            let mut last_row_and_zero = last_row;
            last_row_and_zero.mul_assign(&x.inverse().unwrap());
            let divisors = [
                everywhere_except_last,
                everywhere_except_last_two_rows,
                first_row,
                one_before_last_row,
                last_row,
                last_row_and_zero,
            ];

            let mut lookup_two_gamma = prover_data.stage_2_result.lookup_argument_gamma;
            lookup_two_gamma.double();
            let mut delegation_interpolant = prover_data.stage_2_result.sum_over_delegation_poly;
            delegation_interpolant.mul_assign_by_base(&omega);
            delegation_interpolant.negate();
            let aux = prover::definitions::ProofAuxValues {
                memory_grand_product_accumulator_final_value: prover_data
                    .stage_2_result
                    .grand_product_accumulator,
                delegation_argument_accumulator_sum: prover_data
                    .stage_2_result
                    .sum_over_delegation_poly,
            };
            let quotient_generated = unsafe {
                sha256_round_verifier::concrete::evaluate_quotient(
                    x,
                    witness,
                    memory,
                    setup_values,
                    stage_2,
                    &witness_next,
                    &memory_next,
                    &stage_2_next,
                    prover_data.quotient_commitment_result.quotient_alpha,
                    prover_data.quotient_commitment_result.quotient_beta,
                    &divisors,
                    prover_data
                        .stage_2_result
                        .lookup_argument_linearization_challenges,
                    prover_data.stage_2_result.lookup_argument_gamma,
                    lookup_two_gamma,
                    external_values
                        .challenges
                        .memory_argument
                        .memory_argument_linearization_challenges,
                    external_values
                        .challenges
                        .memory_argument
                        .memory_argument_gamma,
                    external_values
                        .challenges
                        .delegation_argument
                        .unwrap()
                        .delegation_argument_linearization_challenges,
                    external_values
                        .challenges
                        .delegation_argument
                        .unwrap()
                        .delegation_argument_gamma,
                    &[],
                    &aux,
                    Default::default(),
                    Mersenne31Field::ZERO,
                    Mersenne31Field::new(DELEGATION_TYPE_ID),
                    delegation_interpolant,
                )
            };
            assert_eq!(
                quotient_generated, *quotient_opening,
                "generated quotient diverges from the prover at its Fiat-Shamir opening"
            );
        }

        let mut oracle_data = flatten_proof_for_skeleton(&proof, false);
        for query in &proof.queries {
            oracle_data.extend(flatten_query(query));
        }
        set_iterator(oracle_data.into_iter());

        #[allow(invalid_value)]
        let mut proof_output: ConcreteProofOutput = unsafe { MaybeUninit::uninit().assume_init() };
        let mut public_inputs = ConcreteProofPublicInputs::uninit();
        unsafe { sha256_round_verifier::verify(&mut proof_output, &mut public_inputs) };
        assert_eq!(proof_output.delegation_type, DELEGATION_TYPE_ID);
        assert_eq!(proof_output.circuit_sequence, 0);

        let parameters = canonical_delegation_parameters::ALL_DELEGATION_CIRCUITS_PARAMS;
        assert_eq!(parameters.len(), 3);
        assert_eq!(parameters[0].0, 1991);
        assert_eq!(parameters[1].0, 1994);
        assert_eq!(parameters[2].0, DELEGATION_TYPE_ID);
        assert_eq!(parameters[2].1, DOMAIN_SIZE as u32);
        assert!(prover::definitions::MerkleTreeCap::compare(
            &proof_output.setup_caps,
            &parameters[2].2,
        ));

        let quotient_opening_index = circuit.compiled_circuit.setup_layout.total_width
            + circuit.compiled_circuit.witness_layout.total_width
            + circuit.compiled_circuit.memory_layout.total_width
            + circuit
                .compiled_circuit
                .stage_2_layout
                .num_base_field_polys()
            + circuit
                .compiled_circuit
                .stage_2_layout
                .num_ext4_field_polys();
        let mut mutated_proof = proof;
        mutated_proof.evaluations_at_random_points[quotient_opening_index]
            .c0
            .c0
            .0 ^= 1;
        let mut mutated_oracle_data = flatten_proof_for_skeleton(&mutated_proof, false);
        for query in &mutated_proof.queries {
            mutated_oracle_data.extend(flatten_query(query));
        }
        let rejection = std::panic::catch_unwind(|| {
            set_iterator(mutated_oracle_data.into_iter());
            #[allow(invalid_value)]
            let mut proof_output: ConcreteProofOutput =
                unsafe { MaybeUninit::uninit().assume_init() };
            let mut public_inputs = ConcreteProofPublicInputs::uninit();
            unsafe { sha256_round_verifier::verify(&mut proof_output, &mut public_inputs) };
        });
        assert!(
            rejection.is_err(),
            "mutated quotient opening must be rejected"
        );

        // SYSCOIN: Prove (rather than merely witness-check) a trace whose schedule word aliases
        // the writable state. Proof construction is expected to complete, but the generated
        // verifier must reject the unsatisfied modular non-overlap constraint.
        let mut alias_witness = witness.clone();
        alias_witness.register_accesses[1].read_value = STATE_ADDRESS;
        alias_witness.register_accesses[1].write_value = STATE_ADDRESS;
        let alias_oracle = DelegationCircuitOracle {
            cycle_data: &alias_witness,
        };
        let alias_witness_data = evaluate_witness(
            &circuit.compiled_circuit,
            witness_eval_fn_for_gpu_tracer,
            NUM_DELEGATION_CYCLES,
            &alias_oracle,
            &[],
            &circuit.table_driver,
            0,
            &worker,
            Global,
        );
        assert!(!check_satisfied(
            &circuit.compiled_circuit,
            &alias_witness_data.exec_trace,
            alias_witness_data.num_witness_columns,
        ));
        let alias_proving_started = std::time::Instant::now();
        let (_, alias_proof) = prove::<DEFAULT_TRACE_PADDING_MULTIPLE, _>(
            &circuit.compiled_circuit,
            &[],
            &external_values,
            alias_witness_data,
            &setup,
            &twiddles,
            &lde_precomputations,
            0,
            Some(DELEGATION_TYPE_ID as u16),
            LDE_FACTOR,
            TREE_CAP_SIZE,
            53,
            28,
            &worker,
        );
        eprintln!(
            "SYSCOIN invalid-alias proof construction time: {:?}",
            alias_proving_started.elapsed()
        );
        let mut alias_oracle_data = flatten_proof_for_skeleton(&alias_proof, false);
        for query in &alias_proof.queries {
            alias_oracle_data.extend(flatten_query(query));
        }
        let alias_rejection = std::panic::catch_unwind(|| {
            set_iterator(alias_oracle_data.into_iter());
            #[allow(invalid_value)]
            let mut proof_output: ConcreteProofOutput =
                unsafe { MaybeUninit::uninit().assume_init() };
            let mut public_inputs = ConcreteProofPublicInputs::uninit();
            unsafe { sha256_round_verifier::verify(&mut proof_output, &mut public_inputs) };
        });
        assert!(
            alias_rejection.is_err(),
            "a generated proof with an overlapping schedule pointer must be rejected"
        );
    }
}
