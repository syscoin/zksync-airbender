use crate::cycle::status_registers::TrapReason;
use cs::definitions::{TimestampData, TimestampScalar};
use sha256_u32::{delegated_round, SHA256_STATE_WORDS, SYSCOIN_SHA256_ROUND_FUNCTION_ACCESS_ID};

use super::*;

// SYSCOIN: One SHA-256 round; x11 points to W[t] and x12 supplies K[t].
pub const X10_NUM_WRITES: usize = SHA256_STATE_WORDS;
pub const X11_NUM_READS: usize = 1;
const BASE_ABI_REGISTER: u32 = 10;
const STATE_SPAN_BYTES: u32 = ((SHA256_STATE_WORDS - 1) * core::mem::size_of::<u32>()) as u32;

// SYSCOIN: Keep normal and unrolled execution aligned with the circuit's u32 address derivation.
pub(super) fn validate_sha256_round_addresses(x10: u32, x11: u32) {
    assert_eq!(x10 % 4, 0, "state pointer is not word-aligned");
    assert_eq!(x11 % 4, 0, "schedule pointer is not word-aligned");
    assert!(x10 >= 1 << 21, "delegation must not write into ROM");
    assert!(x11 >= 1 << 21, "delegation schedule must reside in RAM");
    assert!(
        x10 <= u32::MAX - STATE_SPAN_BYTES,
        "SHA-256 state span exceeds the u32 address space"
    );
    assert!(
        x11.wrapping_sub(x10) >= (X10_NUM_WRITES * core::mem::size_of::<u32>()) as u32,
        "schedule pointer overlaps writable state"
    );
}

pub fn sha256_round_function<
    M: MemorySource,
    TR: Tracer<C>,
    MMU: MMUImplementation<M, TR, C>,
    C: MachineConfig,
>(
    machine_state: &mut RiscV32State<C>,
    memory_source: &mut M,
    tracer: &mut TR,
    _mmu: &mut MMU,
    rs1_value: u32,
    _trap: &mut TrapReason,
) {
    assert_eq!(
        rs1_value, 0,
        "SHA-256 delegation requires the rs1 value to be zero"
    );

    let x10 = machine_state.observable.registers[10];
    let x11 = machine_state.observable.registers[11];
    let round_constant = machine_state.observable.registers[12];

    validate_sha256_round_addresses(x10, x11);

    let mut state_accesses: [RegisterOrIndirectReadWriteData; X10_NUM_WRITES] =
        register_indirect_read_write_continuous::<_, X10_NUM_WRITES>(x10 as usize, memory_source);
    let state_addresses: [u32; X10_NUM_WRITES] = std::array::from_fn(|i| {
        x10.checked_add((core::mem::size_of::<u32>() * i) as u32)
            .expect("validated SHA-256 state span")
    });
    let mut schedule_accesses: [RegisterOrIndirectReadData; X11_NUM_READS] =
        register_indirect_read_continuous::<_, X11_NUM_READS>(x11 as usize, memory_source);
    let schedule_addresses = [x11];
    let mut words = state_accesses.map(|access| access.read_value);

    delegated_round(&mut words, schedule_accesses[0].read_value, round_constant);

    for (word, access) in words.into_iter().zip(state_accesses.iter_mut()) {
        access.write_value = word;
    }
    write_indirect_accesses::<_, X10_NUM_WRITES>(x10 as usize, &state_accesses, memory_source);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abstractions::memory::{AccessType, MemorySource, VectorMemoryImpl};
    use crate::cycle::state_new::RiscV32StateForUnrolledProver;
    use crate::cycle::IMStandardIsaConfig;
    use crate::delegations::unrolled::sha256_round_function::sha256_round_function_over_unrolled_state;
    use crate::mmu::NoMMU;
    use sha2::compress256;
    use sha2::digest::generic_array::GenericArray;
    use sha256_u32::{small_sigma0, small_sigma1, IV, ROUND_CONSTANTS};
    use std::cell::Cell;
    use std::collections::HashMap;

    const ADDRESS: u32 = 1 << 21;
    const SCHEDULE_ADDRESS: u32 = ADDRESS + 64;

    #[derive(Default)]
    struct SparseMemory {
        words: HashMap<u64, u32>,
        accesses: Cell<usize>,
    }

    impl SparseMemory {
        fn populate(&mut self, address: u32, value: u32) {
            assert_eq!(address % 4, 0);
            self.words.insert(address as u64, value);
        }
    }

    impl MemorySource for SparseMemory {
        fn set(
            &mut self,
            phys_address: u64,
            value: u32,
            _access_type: AccessType,
            _trap: &mut TrapReason,
        ) {
            assert_eq!(phys_address % 4, 0);
            self.accesses.set(self.accesses.get() + 1);
            self.words.insert(phys_address, value);
        }

        fn get(&self, phys_address: u64, _access_type: AccessType, _trap: &mut TrapReason) -> u32 {
            assert_eq!(phys_address % 4, 0);
            self.accesses.set(self.accesses.get() + 1);
            self.words.get(&phys_address).copied().unwrap_or_default()
        }
    }

    fn next(seed: &mut u64) -> u32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed as u32
    }

    fn normal_accepts_addresses(state_address: u32, schedule_address: u32) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut memory = VectorMemoryImpl::new_for_byte_size(1 << 22);
            let mut state = RiscV32State::<IMStandardIsaConfig>::initial(0);
            state.observable.registers[10] = state_address;
            state.observable.registers[11] = schedule_address;
            let mut mmu = NoMMU::default();
            sha256_round_function(
                &mut state,
                &mut memory,
                &mut (),
                &mut mmu,
                0,
                &mut TrapReason::NoTrap,
            );
        }))
        .is_ok()
    }

    fn unrolled_accepts_addresses(state_address: u32, schedule_address: u32) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut memory = VectorMemoryImpl::new_for_byte_size(1 << 22);
            let mut state = RiscV32StateForUnrolledProver::<IMStandardIsaConfig>::initial(0);
            state.observable.registers[10] = state_address;
            state.observable.registers[11] = schedule_address;
            sha256_round_function_over_unrolled_state(&mut state, &mut memory, &mut ());
        }))
        .is_ok()
    }

    #[test]
    fn normal_and_unrolled_simulators_match_reference_rounds() {
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        let mut memory = VectorMemoryImpl::new_for_byte_size(1 << 22);
        for case in 0..256 {
            let mut input = [0u32; SHA256_STATE_WORDS];
            for word in &mut input {
                *word = next(&mut seed);
            }
            let w = next(&mut seed);
            let k = next(&mut seed);
            let mut expected = input;
            delegated_round(&mut expected, w, k);

            for (i, word) in input.into_iter().enumerate() {
                memory.populate(ADDRESS + (i * 4) as u32, word);
            }
            memory.populate(SCHEDULE_ADDRESS, w);
            let mut state = RiscV32State::<IMStandardIsaConfig>::initial(0);
            state.observable.registers[10] = ADDRESS;
            state.observable.registers[11] = SCHEDULE_ADDRESS;
            state.observable.registers[12] = k;
            let mut mmu = NoMMU::default();
            sha256_round_function(
                &mut state,
                &mut memory,
                &mut (),
                &mut mmu,
                0,
                &mut TrapReason::NoTrap,
            );
            let got =
                std::array::from_fn(|i| memory.get_noexcept((ADDRESS as usize + i * 4) as u64));
            assert_eq!(got, expected, "normal simulator case {case}");

            for (i, word) in input.into_iter().enumerate() {
                memory.populate(ADDRESS + (i * 4) as u32, word);
            }
            memory.populate(SCHEDULE_ADDRESS, w);
            let mut state = RiscV32StateForUnrolledProver::<IMStandardIsaConfig>::initial(0);
            state.observable.registers[10] = ADDRESS;
            state.observable.registers[11] = SCHEDULE_ADDRESS;
            state.observable.registers[12] = k;
            sha256_round_function_over_unrolled_state(&mut state, &mut memory, &mut ());
            let got =
                std::array::from_fn(|i| memory.get_noexcept((ADDRESS as usize + i * 4) as u64));
            assert_eq!(got, expected, "unrolled simulator case {case}");
        }
    }

    #[test]
    fn sixty_four_delegated_rounds_match_reference_compression() {
        let initial = IV;
        let block =
            std::array::from_fn::<_, 16, _>(|i| (i as u32).wrapping_mul(0x9e37_79b9) ^ 0xa5a5_5a5a);
        let mut schedule = [0u32; 64];
        schedule[..16].copy_from_slice(&block);
        for round in 16..64 {
            schedule[round] = small_sigma1(schedule[round - 2])
                .wrapping_add(schedule[round - 7])
                .wrapping_add(small_sigma0(schedule[round - 15]))
                .wrapping_add(schedule[round - 16]);
        }

        let mut bytes = [0u8; 64];
        for (dst, word) in bytes.chunks_exact_mut(4).zip(block) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
        let mut expected = initial;
        compress256(&mut expected, &[GenericArray::clone_from_slice(&bytes)]);

        let mut memory = VectorMemoryImpl::new_for_byte_size(1 << 22);
        for (i, word) in initial.into_iter().enumerate() {
            memory.populate(ADDRESS + (i * 4) as u32, word);
        }
        let mut state = RiscV32State::<IMStandardIsaConfig>::initial(0);
        let mut mmu = NoMMU::default();
        state.observable.registers[10] = ADDRESS;
        for round in 0..64 {
            memory.populate(SCHEDULE_ADDRESS, schedule[round]);
            state.observable.registers[11] = SCHEDULE_ADDRESS;
            state.observable.registers[12] = ROUND_CONSTANTS[round];
            sha256_round_function(
                &mut state,
                &mut memory,
                &mut (),
                &mut mmu,
                0,
                &mut TrapReason::NoTrap,
            );
        }
        let working: [u32; 8] =
            std::array::from_fn(|i| memory.get_noexcept((ADDRESS as usize + i * 4) as u64));
        let got = std::array::from_fn(|i| initial[i].wrapping_add(working[i]));
        assert_eq!(got, expected, "normal simulator compression");

        for (i, word) in initial.into_iter().enumerate() {
            memory.populate(ADDRESS + (i * 4) as u32, word);
        }
        let mut state = RiscV32StateForUnrolledProver::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = ADDRESS;
        for round in 0..64 {
            memory.populate(SCHEDULE_ADDRESS, schedule[round]);
            state.observable.registers[11] = SCHEDULE_ADDRESS;
            state.observable.registers[12] = ROUND_CONSTANTS[round];
            sha256_round_function_over_unrolled_state(&mut state, &mut memory, &mut ());
        }
        let working: [u32; 8] =
            std::array::from_fn(|i| memory.get_noexcept((ADDRESS as usize + i * 4) as u64));
        let got = std::array::from_fn(|i| initial[i].wrapping_add(working[i]));
        assert_eq!(got, expected, "unrolled simulator compression");
    }

    #[test]
    fn simulator_address_guards_match_the_canonical_abi() {
        // SYSCOIN: Both dispatch paths reject ROM pointers and every aligned alias in the
        // 32-byte state span. Modular subtraction permits a schedule word immediately before the
        // state and avoids overflow near the end of the address space.
        assert!(!normal_accepts_addresses(ADDRESS - 4, SCHEDULE_ADDRESS));
        assert!(!unrolled_accepts_addresses(ADDRESS - 4, SCHEDULE_ADDRESS));
        assert!(!normal_accepts_addresses(ADDRESS, ADDRESS - 4));
        assert!(!unrolled_accepts_addresses(ADDRESS, ADDRESS - 4));
        for offset in (0..32).step_by(4) {
            assert!(!normal_accepts_addresses(ADDRESS, ADDRESS + offset));
            assert!(!unrolled_accepts_addresses(ADDRESS, ADDRESS + offset));
        }
        assert!(normal_accepts_addresses(ADDRESS + 64, ADDRESS + 60));
        assert!(unrolled_accepts_addresses(ADDRESS + 64, ADDRESS + 60));
        assert!(normal_accepts_addresses(ADDRESS, ADDRESS + 32));
        assert!(unrolled_accepts_addresses(ADDRESS, ADDRESS + 32));

        // The last legal state base covers exactly through 0xffff_fffc. The next aligned
        // address would make the eighth word wrap to zero and is rejected before memory access.
        assert!(std::panic::catch_unwind(|| {
            validate_sha256_round_addresses(0xffff_ffe0, 0xffff_ffdc)
        })
        .is_ok());
        assert!(std::panic::catch_unwind(|| {
            validate_sha256_round_addresses(0xffff_ffe4, 0xffff_ffe0)
        })
        .is_err());
    }

    #[test]
    fn simulators_execute_last_legal_u32_state_span_and_reject_the_next_before_memory() {
        const LAST_STATE_BASE: u32 = 0xffff_ffe0;
        const SCHEDULE_BEFORE_STATE: u32 = 0xffff_ffdc;
        const FIRST_OVERFLOWING_STATE_BASE: u32 = 0xffff_ffe4;
        let input = IV;
        let w = 0x6162_6380;
        let k = ROUND_CONSTANTS[0];
        let mut expected = input;
        delegated_round(&mut expected, w, k);

        // SYSCOIN: A sparse memory makes this a non-vacuous execution test at the top of the u32
        // address space, rather than testing only the shared validation predicate.
        let mut memory = SparseMemory::default();
        for (i, word) in input.into_iter().enumerate() {
            memory.populate(LAST_STATE_BASE + (i * 4) as u32, word);
        }
        memory.populate(SCHEDULE_BEFORE_STATE, w);
        let mut state = RiscV32State::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = LAST_STATE_BASE;
        state.observable.registers[11] = SCHEDULE_BEFORE_STATE;
        state.observable.registers[12] = k;
        let mut mmu = NoMMU::default();
        sha256_round_function(
            &mut state,
            &mut memory,
            &mut (),
            &mut mmu,
            0,
            &mut TrapReason::NoTrap,
        );
        assert!(memory.accesses.get() > 0);
        let got =
            std::array::from_fn(|i| memory.get_noexcept((LAST_STATE_BASE + (i * 4) as u32) as u64));
        assert_eq!(got, expected);

        let mut memory = SparseMemory::default();
        for (i, word) in input.into_iter().enumerate() {
            memory.populate(LAST_STATE_BASE + (i * 4) as u32, word);
        }
        memory.populate(SCHEDULE_BEFORE_STATE, w);
        let mut state = RiscV32StateForUnrolledProver::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = LAST_STATE_BASE;
        state.observable.registers[11] = SCHEDULE_BEFORE_STATE;
        state.observable.registers[12] = k;
        sha256_round_function_over_unrolled_state(&mut state, &mut memory, &mut ());
        assert!(memory.accesses.get() > 0);
        let got =
            std::array::from_fn(|i| memory.get_noexcept((LAST_STATE_BASE + (i * 4) as u32) as u64));
        assert_eq!(got, expected);

        let mut memory = SparseMemory::default();
        let mut state = RiscV32State::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = FIRST_OVERFLOWING_STATE_BASE;
        state.observable.registers[11] = LAST_STATE_BASE;
        let mut mmu = NoMMU::default();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sha256_round_function(
                &mut state,
                &mut memory,
                &mut (),
                &mut mmu,
                0,
                &mut TrapReason::NoTrap,
            );
        }))
        .is_err());
        assert_eq!(memory.accesses.get(), 0);

        let mut memory = SparseMemory::default();
        let mut state = RiscV32StateForUnrolledProver::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = FIRST_OVERFLOWING_STATE_BASE;
        state.observable.registers[11] = LAST_STATE_BASE;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sha256_round_function_over_unrolled_state(&mut state, &mut memory, &mut ());
        }))
        .is_err());
        assert_eq!(memory.accesses.get(), 0);
    }

    #[test]
    #[should_panic(expected = "SHA-256 delegation requires the rs1 value to be zero")]
    fn simulator_rejects_nonzero_rs1() {
        let mut memory = VectorMemoryImpl::new_for_byte_size(1 << 22);
        let mut state = RiscV32State::<IMStandardIsaConfig>::initial(0);
        state.observable.registers[10] = ADDRESS;
        let mut mmu = NoMMU::default();
        sha256_round_function(
            &mut state,
            &mut memory,
            &mut (),
            &mut mmu,
            1,
            &mut TrapReason::NoTrap,
        );
    }
}
