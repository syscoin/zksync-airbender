use crate::cycle::state_new::RiscV32StateForUnrolledProver;
use cs::definitions::{TimestampData, TimestampScalar};
use sha256_u32::{delegated_round, SHA256_STATE_WORDS, SYSCOIN_SHA256_ROUND_FUNCTION_ACCESS_ID};

use super::*;
use crate::delegations::sha256_round_function::validate_sha256_round_addresses;

// SYSCOIN: Unrolled simulator twin; x11 points to W[t] and x12 supplies K[t].
pub const X10_NUM_WRITES: usize = SHA256_STATE_WORDS;
pub const X11_NUM_READS: usize = 1;
const BASE_ABI_REGISTER: u32 = 10;

pub fn sha256_round_function_over_unrolled_state<
    M: MemorySource,
    TR: Tracer<C>,
    C: MachineConfig,
>(
    machine_state: &mut RiscV32StateForUnrolledProver<C>,
    memory_source: &mut M,
    tracer: &mut TR,
) {
    let x10 = machine_state.observable.registers[10];
    let x11 = machine_state.observable.registers[11];
    let round_constant = machine_state.observable.registers[12];

    validate_sha256_round_addresses(x10, x11);

    let mut state_accesses: [RegisterOrIndirectReadWriteData; X10_NUM_WRITES] =
        register_indirect_read_write_continuous_noexcept::<_, X10_NUM_WRITES>(
            x10 as usize,
            memory_source,
        );
    let state_addresses: [u32; X10_NUM_WRITES] = std::array::from_fn(|i| {
        x10.checked_add((core::mem::size_of::<u32>() * i) as u32)
            .expect("validated SHA-256 state span")
    });
    let mut schedule_accesses: [RegisterOrIndirectReadData; X11_NUM_READS] =
        register_indirect_read_continuous_noexcept::<_, X11_NUM_READS>(x11 as usize, memory_source);
    let schedule_addresses = [x11];
    let mut words = state_accesses.map(|access| access.read_value);

    delegated_round(&mut words, schedule_accesses[0].read_value, round_constant);

    for (word, access) in words.into_iter().zip(state_accesses.iter_mut()) {
        access.write_value = word;
    }
    write_indirect_accesses_noexcept::<_, X10_NUM_WRITES>(
        x10 as usize,
        &state_accesses,
        memory_source,
    );

    let mut register_accesses = [
        RegisterOrIndirectReadWriteData {
            read_value: x10,
            write_value: x10,
            timestamp: TimestampData::EMPTY,
        },
        RegisterOrIndirectReadWriteData {
            read_value: x11,
            write_value: x11,
            timestamp: TimestampData::EMPTY,
        },
        RegisterOrIndirectReadWriteData {
            read_value: round_constant,
            write_value: round_constant,
            timestamp: TimestampData::EMPTY,
        },
    ];

    tracer.record_delegation(
        SYSCOIN_SHA256_ROUND_FUNCTION_ACCESS_ID,
        BASE_ABI_REGISTER,
        &mut register_accesses,
        &schedule_addresses,
        &mut schedule_accesses,
        &state_addresses,
        &mut state_accesses,
    );
}
