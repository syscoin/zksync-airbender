use super::*;

// SYSCOIN: Setup/precomputation constructor for delegation CSR 0x7cb.
pub fn get_sha256_round_circuit_setup<A: GoodAllocator, B: GoodAllocator>(
    worker: &Worker,
) -> DelegationCircuitPrecomputations<A, B> {
    let machine: DelegationProcessorDescription = sha256_round::get_delegation_circuit();
    let table_driver = sha256_round::get_table_driver();

    let twiddles: Twiddles<_, A> = Twiddles::new(sha256_round::DOMAIN_SIZE, worker);
    let lde_precomputations = LdePrecomputations::new(
        sha256_round::DOMAIN_SIZE,
        sha256_round::LDE_FACTOR,
        sha256_round::LDE_SOURCE_COSETS,
        worker,
    );
    let setup = SetupPrecomputations::<
        DEFAULT_TRACE_PADDING_MULTIPLE,
        A,
        DefaultTreeConstructor,
    >::from_tables_and_trace_len(
        &table_driver,
        sha256_round::DOMAIN_SIZE,
        &machine.compiled_circuit.setup_layout,
        &twiddles,
        &lde_precomputations,
        sha256_round::LDE_FACTOR,
        sha256_round::TREE_CAP_SIZE,
        worker,
    );

    DelegationCircuitPrecomputations {
        trace_len: sha256_round::DOMAIN_SIZE,
        lde_factor: sha256_round::LDE_FACTOR,
        tree_cap_size: sha256_round::TREE_CAP_SIZE,
        compiled_circuit: machine,
        twiddles,
        lde_precomputations,
        setup,
        witness_eval_fn_for_gpu_tracer: sha256_round::witness_eval_fn_for_gpu_tracer,
    }
}
