use super::*;
use crate::constraint::{Constraint, Term};
use crate::cs::circuit::{Circuit, IndirectAccessType, RegisterAccessRequest, RegisterAccessType};
use crate::cs::utils::collapse_max_quadratic_constraint_into;
use crate::cs::witness_placer::{
    WitnessComputationCore, WitnessComputationalInteger, WitnessComputationalU16,
    WitnessComputationalU32, WitnessPlacer, WitnessTypeSet,
};
use crate::one_row_compiler::{LookupInput, Variable};
use crate::types::{Boolean, Num};
use field::PrimeField;
use sha256_u32::SHA256_STATE_WORDS;

// SYSCOIN: Constraint system for one SHA-256 round at delegation CSR 0x7cb.

type Word = [Variable; 2];
type ByteWord = [Variable; 4];
type BitWord = [Boolean; 32];

pub fn all_table_types() -> Vec<TableType> {
    vec![
        TableType::Xor,
        TableType::And,
        TableType::U16SplitAsBytes,
        TableType::ExtractLower5Bits,
        TableType::RangeCheckSmall,
        TableType::RangeCheckLarge,
    ]
}

pub fn sha256_round_delegation_circuit_create_table_driver<F: PrimeField>() -> TableDriver<F> {
    let mut table_driver = TableDriver::new();
    for table in all_table_types() {
        table_driver.materialize_table(table);
    }
    table_driver
}

fn materialize_tables_into_cs<F: PrimeField, CS: Circuit<F>>(cs: &mut CS) {
    for table in all_table_types() {
        cs.materialize_table(table);
    }
}

fn word_to_bits<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, word: Word) -> BitWord {
    let low = Boolean::split_into_bitmask::<F, CS, 16>(cs, Num::Var(word[0]));
    let high = Boolean::split_into_bitmask::<F, CS, 16>(cs, Num::Var(word[1]));
    std::array::from_fn(|i| if i < 16 { low[i] } else { high[i - 16] })
}

fn bits_to_word<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, bits: &BitWord) -> Word {
    std::array::from_fn(|limb| {
        let mut constraint = Constraint::<F>::empty();
        for bit in 0..16 {
            constraint += Term::from((
                F::from_u64_unchecked(1u64 << bit),
                bits[limb * 16 + bit].get_variable().unwrap(),
            ));
        }
        cs.add_variable_from_constraint_allow_explicit_linear(constraint)
    })
}

fn split_word_into_bytes<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, word: Word) -> ByteWord {
    let [byte0, byte1] = cs.get_variables_from_lookup_constrained::<1, 2>(
        &[LookupInput::Variable(word[0])],
        TableType::U16SplitAsBytes,
    );
    let [byte2, byte3] = cs.get_variables_from_lookup_constrained::<1, 2>(
        &[LookupInput::Variable(word[1])],
        TableType::U16SplitAsBytes,
    );
    [byte0, byte1, byte2, byte3]
}

fn bytes_to_word<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, bytes: ByteWord) -> Word {
    [
        cs.add_variable_from_constraint_allow_explicit_linear(
            Constraint::from(bytes[0]) + Term::from((F::from_u64_unchecked(1 << 8), bytes[1])),
        ),
        cs.add_variable_from_constraint_allow_explicit_linear(
            Constraint::from(bytes[2]) + Term::from((F::from_u64_unchecked(1 << 8), bytes[3])),
        ),
    ]
}

fn byte_op<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
    a: ByteWord,
    b: ByteWord,
    table: TableType,
) -> ByteWord {
    std::array::from_fn(|i| {
        cs.get_variables_from_lookup_constrained::<2, 1>(
            &[LookupInput::Variable(a[i]), LookupInput::Variable(b[i])],
            table,
        )[0]
    })
}

fn xor_bytes<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, a: ByteWord, b: ByteWord) -> ByteWord {
    byte_op(cs, a, b, TableType::Xor)
}

fn and_bytes<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, a: ByteWord, b: ByteWord) -> ByteWord {
    byte_op(cs, a, b, TableType::And)
}

fn choose_word<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
    x: ByteWord,
    y: ByteWord,
    z: ByteWord,
) -> Word {
    // z XOR (x AND (y XOR z)).
    let y_xor_z = xor_bytes(cs, y, z);
    let selected = and_bytes(cs, x, y_xor_z);
    let result = xor_bytes(cs, z, selected);
    bytes_to_word(cs, result)
}

fn majority_word<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
    x: ByteWord,
    y: ByteWord,
    z: ByteWord,
) -> Word {
    // (x AND y) XOR (z AND (x XOR y)).
    let x_and_y = and_bytes(cs, x, y);
    let x_xor_y = xor_bytes(cs, x, y);
    let selected_z = and_bytes(cs, z, x_xor_y);
    let result = xor_bytes(cs, x_and_y, selected_z);
    bytes_to_word(cs, result)
}

fn rotate_right(word: &BitWord, amount: usize) -> BitWord {
    std::array::from_fn(|i| word[(i + amount) % 32])
}

fn xor_bits<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, a: &BitWord, b: &BitWord) -> BitWord {
    std::array::from_fn(|i| Boolean::xor(&a[i], &b[i], cs))
}

fn big_sigma0<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, x: &BitWord) -> Word {
    let first = xor_bits(cs, &rotate_right(x, 2), &rotate_right(x, 13));
    let result = xor_bits(cs, &first, &rotate_right(x, 22));
    bits_to_word(cs, &result)
}

fn big_sigma1<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, x: &BitWord) -> Word {
    let first = xor_bits(cs, &rotate_right(x, 6), &rotate_right(x, 11));
    let result = xor_bits(cs, &first, &rotate_right(x, 25));
    bits_to_word(cs, &result)
}

fn add_words<F: PrimeField, CS: Circuit<F>>(cs: &mut CS, inputs: &[Word]) -> Word {
    assert!(inputs.len() >= 2 && inputs.len() <= 5);
    let inputs = inputs.to_vec();
    let result = [
        cs.add_variable_with_range_check(16).get_variable(),
        cs.add_variable_with_range_check(16).get_variable(),
    ];
    let carry_low = cs.add_variable_with_range_check(8).get_variable();
    let carry_high = cs.add_variable_with_range_check(8).get_variable();

    let witness_inputs = inputs.clone();
    let value_fn = move |placer: &mut CS::WitnessPlacer| {
        let mut low_sum = <CS::WitnessPlacer as WitnessTypeSet<F>>::U32::constant(0u32);
        for word in &witness_inputs {
            low_sum.add_assign(&placer.get_u16(word[0]).widen());
        }
        let low = low_sum.get_lowest_bits(16).truncate();
        let low_carry_u32 = low_sum.shr(16);
        placer.assign_u16(result[0], &low);
        placer.assign_u16(carry_low, &low_carry_u32.truncate());

        let mut high_sum = low_carry_u32;
        for word in &witness_inputs {
            high_sum.add_assign(&placer.get_u16(word[1]).widen());
        }
        let high = high_sum.get_lowest_bits(16).truncate();
        let high_carry_u32 = high_sum.shr(16);
        placer.assign_u16(result[1], &high);
        placer.assign_u16(carry_high, &high_carry_u32.truncate());
    };
    cs.set_values(value_fn);

    let mut low_constraint = Constraint::<F>::empty();
    let mut high_constraint = Constraint::<F>::from(carry_low);
    for word in inputs {
        low_constraint += Term::from(word[0]);
        high_constraint += Term::from(word[1]);
    }
    low_constraint -= Term::from(result[0]);
    low_constraint -= Term::from((F::from_u64_unchecked(1 << 16), carry_low));
    high_constraint -= Term::from(result[1]);
    high_constraint -= Term::from((F::from_u64_unchecked(1 << 16), carry_high));
    cs.add_constraint_allow_explicit_linear(low_constraint);
    cs.add_constraint_allow_explicit_linear(high_constraint);

    result
}

fn assign_masked_word<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
    execute: Boolean,
    source: Word,
    destination: Word,
) {
    for (source, destination) in source.into_iter().zip(destination) {
        let masked = cs
            .choose(execute, Num::Var(source), Num::Constant(F::ZERO))
            .get_variable();
        let mut constraint = Constraint::from(masked);
        collapse_max_quadratic_constraint_into(cs, constraint.clone(), destination);
        constraint -= Term::from(destination);
        cs.add_constraint_allow_explicit_linear(constraint);
    }
}

fn enforce_active_ram_pointer_floor<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
    execute: Boolean,
    pointer: Word,
) {
    // SYSCOIN: RAM starts at 2^21, so the high limb must be at least 32 on active rows. Masking
    // padding to exactly 32 keeps the padded row canonical. The wrapping U16 witness makes an
    // under-floor active pointer fail the field equation instead of aborting witness generation.
    let high_minus_ram_floor = cs.add_variable_with_range_check(16).get_variable();
    let execute_variable = execute.get_variable().unwrap();
    let high = pointer[1];
    let value_fn = move |placer: &mut CS::WitnessPlacer| {
        let execute = placer.get_boolean(execute_variable);
        let high = placer.get_u16(high);
        let ram_floor = <CS::WitnessPlacer as WitnessTypeSet<F>>::U16::constant(32);
        let mut difference =
            <CS::WitnessPlacer as WitnessTypeSet<F>>::U16::select(&execute, &high, &ram_floor);
        difference.sub_assign(&ram_floor);
        placer.assign_u16(high_minus_ram_floor, &difference);
    };
    cs.set_values(value_fn);

    cs.add_constraint(
        Term::from(execute) * (Term::from(pointer[1]) - Term::from(32u64))
            - Term::from(high_minus_ram_floor),
    );
}

fn enforce_schedule_does_not_alias_state<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
    execute: Boolean,
    state_pointer: Word,
    schedule_pointer: Word,
) {
    // SYSCOIN: Compute schedule_pointer - state_pointer modulo 2^32. The state write spans 32
    // bytes, so an active request is canonical exactly when the modular delta is >= 32. This also
    // permits a schedule word before the state and avoids host overflow near u32::MAX.
    let delta_low = cs.add_variable_with_range_check(16).get_variable();
    let delta_high = cs.add_variable_with_range_check(16).get_variable();
    let borrow_low = cs.add_boolean_variable();
    let borrow_high = cs.add_boolean_variable();
    let borrow_low_variable = borrow_low.get_variable().unwrap();
    let borrow_high_variable = borrow_high.get_variable().unwrap();

    let value_fn = move |placer: &mut CS::WitnessPlacer| {
        let state_low = placer.get_u16(state_pointer[0]);
        let state_high = placer.get_u16(state_pointer[1]);
        let schedule_low = placer.get_u16(schedule_pointer[0]);
        let schedule_high = placer.get_u16(schedule_pointer[1]);
        let (delta_low_value, borrow_low_value) = schedule_low.overflowing_sub(&state_low);
        let (delta_high_value, borrow_high_value) =
            schedule_high.overflowing_sub_with_borrow(&state_high, &borrow_low_value);
        placer.assign_u16(delta_low, &delta_low_value);
        placer.assign_u16(delta_high, &delta_high_value);
        placer.assign_mask(borrow_low_variable, &borrow_low_value);
        placer.assign_mask(borrow_high_variable, &borrow_high_value);
    };
    cs.set_values(value_fn);

    let two_to_16 = F::from_u64_unchecked(1 << 16);
    cs.add_constraint_allow_explicit_linear(
        Constraint::from(schedule_pointer[0]) + Term::from((two_to_16, borrow_low_variable))
            - Term::from(state_pointer[0])
            - Term::from(delta_low),
    );
    cs.add_constraint_allow_explicit_linear(
        Constraint::from(schedule_pointer[1]) + Term::from((two_to_16, borrow_high_variable))
            - Term::from(state_pointer[1])
            - Term::from(borrow_low_variable)
            - Term::from(delta_high),
    );

    let [delta_low_bits, unused] = cs.get_variables_from_lookup_constrained::<1, 2>(
        &[LookupInput::Variable(delta_low)],
        TableType::ExtractLower5Bits,
    );
    let delta_low_div_32 = cs.add_variable();
    let value_fn = move |placer: &mut CS::WitnessPlacer| {
        let quotient = placer.get_u16(delta_low).shr(5);
        placer.assign_u16(delta_low_div_32, &quotient);
    };
    cs.set_values(value_fn);
    cs.add_constraint_allow_explicit_linear(
        Constraint::from(delta_low)
            - Term::from(delta_low_bits)
            - Term::from((F::from_u64_unchecked(32), delta_low_div_32)),
    );
    cs.add_constraint_allow_explicit_linear(Constraint::from(unused));

    // This integer is zero exactly for modular deltas 0..31. Its maximum is below the M31
    // modulus, so the zero test cannot alias through the field representation.
    let scaled_delta = cs.add_variable_from_constraint_allow_explicit_linear(
        Constraint::from(delta_low_div_32) + Term::from((F::from_u64_unchecked(2048), delta_high)),
    );
    let is_overlapping = cs.is_zero(Num::Var(scaled_delta));
    cs.add_constraint(Term::from(execute) * Term::from(is_overlapping));
}

pub fn define_sha256_round_delegation_circuit<F: PrimeField, CS: Circuit<F>>(
    cs: &mut CS,
) -> Vec<Word> {
    materialize_tables_into_cs(cs);
    let execute = cs.process_delegation_request();

    let x10 = cs.create_register_and_indirect_memory_accesses(RegisterAccessRequest {
        register_index: 10,
        register_write: false,
        indirects_alignment_log2: 2,
        indirect_accesses: vec![true; SHA256_STATE_WORDS],
    });
    let x11 = cs.create_register_and_indirect_memory_accesses(RegisterAccessRequest {
        register_index: 11,
        register_write: false,
        indirects_alignment_log2: 2,
        indirect_accesses: vec![false],
    });
    let x12 = cs.create_register_and_indirect_memory_accesses(RegisterAccessRequest {
        register_index: 12,
        register_write: false,
        indirects_alignment_log2: 0,
        indirect_accesses: vec![],
    });
    let RegisterAccessType::Read {
        read_value: state_pointer,
    } = x10.register_access
    else {
        unreachable!()
    };
    let RegisterAccessType::Read {
        read_value: schedule_pointer,
    } = x11.register_access
    else {
        unreachable!()
    };
    enforce_active_ram_pointer_floor(cs, execute, state_pointer);
    enforce_active_ram_pointer_floor(cs, execute, schedule_pointer);
    enforce_schedule_does_not_alias_state(cs, execute, state_pointer, schedule_pointer);

    let mut state = Vec::with_capacity(SHA256_STATE_WORDS);
    let mut outputs = Vec::with_capacity(SHA256_STATE_WORDS);
    for access in x10.indirect_accesses {
        let IndirectAccessType::Write {
            read_value,
            write_value,
        } = access
        else {
            unreachable!()
        };
        state.push(read_value);
        outputs.push(write_value);
    }
    let [IndirectAccessType::Read {
        read_value: schedule_word,
    }]: [IndirectAccessType; 1] = x11.indirect_accesses.try_into().unwrap()
    else {
        unreachable!()
    };
    let RegisterAccessType::Read {
        read_value: round_constant,
    } = x12.register_access
    else {
        unreachable!()
    };

    let [a, b, c, d, e, f, g, h]: [Word; 8] = state.try_into().unwrap();
    let a_bits = word_to_bits(cs, a);
    let e_bits = word_to_bits(cs, e);
    let sigma1_e = big_sigma1(cs, &e_bits);
    let sigma0_a = big_sigma0(cs, &a_bits);

    let a_bytes = split_word_into_bytes(cs, a);
    let b_bytes = split_word_into_bytes(cs, b);
    let c_bytes = split_word_into_bytes(cs, c);
    let e_bytes = split_word_into_bytes(cs, e);
    let f_bytes = split_word_into_bytes(cs, f);
    let g_bytes = split_word_into_bytes(cs, g);
    let ch = choose_word(cs, e_bytes, f_bytes, g_bytes);
    let maj = majority_word(cs, a_bytes, b_bytes, c_bytes);

    let t1 = add_words(cs, &[h, sigma1_e, ch, round_constant, schedule_word]);
    let t2 = add_words(cs, &[sigma0_a, maj]);
    let next_state = [
        add_words(cs, &[t1, t2]),
        a,
        b,
        c,
        add_words(cs, &[d, t1]),
        e,
        f,
        g,
    ];

    for i in 0..SHA256_STATE_WORDS {
        assign_masked_word(cs, execute, next_state[i], outputs[i]);
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cs::cs_reference::BasicAssembly;
    use crate::one_row_compiler::OneRowCompiler;
    use field::Mersenne31Field;

    #[test]
    fn compile_sha256_round_delegation_at_minimum_supported_domain() {
        let mut cs = BasicAssembly::<Mersenne31Field>::new();
        define_sha256_round_delegation_circuit(&mut cs);
        let (output, _) = cs.finalize();
        let compiled = OneRowCompiler::default().compile_to_evaluate_delegations(output, 20);
        eprintln!(
            "SHA-256 delegation widths: memory={}, witness={}, stage2={}",
            compiled.memory_layout.total_width,
            compiled.witness_layout.total_width,
            compiled.stage_2_layout.total_width,
        );
        // SYSCOIN: The final memory/witness/stage-2 geometry is 75/281/380.
        // Effective-cycle accounting compares setup/witness/stage-2 widths to
        // the main VM: max(6/12, 281/192, 380/160) = 2.375, whose conservative
        // next power of two is coefficient 4. This is not a wall-time claim.
        assert_eq!(compiled.memory_layout.total_width, 75);
        assert_eq!(compiled.witness_layout.total_width, 281);
        assert_eq!(compiled.stage_2_layout.total_width, 380);
        assert!(compiled.witness_layout.total_width <= 647);
        let num_trivial = compiled
            .witness_layout
            .range_check_16_columns
            .num_elements();
        let range_checks = &compiled.witness_layout.range_check_16_lookup_expressions;
        assert_eq!(num_trivial, 12);
        assert_eq!((range_checks.len() - num_trivial) % 2, 0);
    }

    #[test]
    fn sha256_round_has_a_resolvable_witness_graph() {
        let forms = dump_ssa_witness_eval_form_for_delegation::<Mersenne31Field, _>(
            define_sha256_round_delegation_circuit,
        );
        assert!(!forms.is_empty());
    }
}
