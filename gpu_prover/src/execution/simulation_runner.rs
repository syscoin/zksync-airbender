use crate::circuit_type::{CircuitType, UnrolledCircuitType};
use crate::execution::messages::{InitsAndTeardownsData, WorkerResult};
use crate::execution::tracing::{DataTraceRanges, TracingDataProducers, TracingType};
use crate::execution::A;
use crate::machine_type::MachineType;
use crossbeam_channel::{Receiver, Sender};
use cs::definitions::{INITIAL_TIMESTAMP, TIMESTAMP_STEP};
use era_cudart::memory::{CudaHostAllocFlags, HostAllocation};
use itertools::Itertools;
use log::{debug, trace};
use riscv_transpiler::common_constants::ROM_WORD_SIZE;
use riscv_transpiler::jit::{
    ContextImpl, JitRunnerRam, MachineState, MemoryHolder, TraceChunk, MAX_NUM_COUNTERS,
};
use riscv_transpiler::vm::NonDeterminismCSRSource;
use std::mem::replace;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use type_map::concurrent::TypeMap;

#[path = "empty_inits_and_teardowns.rs"]
mod empty_inits_and_teardowns;
pub(crate) use empty_inits_and_teardowns::EmptyInitsAndTeardownsStreamer;

#[cfg(not(target_arch = "x86_64"))]
use self::compat::{Context, JittedCode};
#[cfg(target_arch = "x86_64")]
use riscv_transpiler::jit::{Context, JittedCode};

// We're depending on JIT unconditionally, so it can fail compilation on some platforms,
// since some of the exported paths are platform-dependent. So, we provide panicking
// replacements to allow compilation on the platforms, e.g. with
// `ZKSYNC_USE_CUDA_STUBS`.
#[cfg(not(target_arch = "x86_64"))]
mod compat {
    use super::{ContextImpl, MemoryHolder, TraceChunk};
    use std::marker::PhantomData;
    use std::ptr::NonNull;

    pub(super) struct Context<I: ContextImpl> {
        pub(super) implementation: I,
    }

    impl<I: ContextImpl> Context<I> {
        // The RAM configuration only means anything to the real JIT, which reads it
        // back out of the context in `process_csr`. This path panics before it could
        // be used.
        pub(super) fn new(implementation: I, _ram_config: super::JitRunnerRam) -> Self {
            Self { implementation }
        }

        pub(super) fn into_implementation(self) -> I {
            self.implementation
        }
    }

    pub(super) struct JittedCode<I: ContextImpl> {
        _marker: PhantomData<I>,
    }

    unsafe impl<I: ContextImpl> Send for JittedCode<I> {}

    unsafe impl<I: ContextImpl> Sync for JittedCode<I> {}

    impl<I: ContextImpl> JittedCode<I> {
        pub(super) fn preprocess_bytecode(
            _program: &[u32],
            _cycles_bound: Option<u32>,
            _ram_config: super::JitRunnerRam,
        ) -> Self {
            Self {
                _marker: PhantomData,
            }
        }

        pub(super) fn run_over_prepared_memory(
            &self,
            _context: &mut Context<I>,
            _memory: &mut MemoryHolder,
            _initial_trace_chunk: NonNull<TraceChunk>,
        ) {
            panic!("gpu_prover simulation requires the x86_64 JIT backend");
        }
    }
}

pub(crate) struct LockedBoxedTraceChunk {
    pub chunk: Box<TraceChunk, A>,
}

impl LockedBoxedTraceChunk {
    const LOG_CHUNK_SIZE: u32 = 20;

    pub fn new() -> Self {
        let allocation =
            HostAllocation::alloc(Self::pinned_bytes(), CudaHostAllocFlags::DEFAULT).unwrap();
        let allocator = A::new(vec![allocation], Self::LOG_CHUNK_SIZE);
        // SAFETY: TraceChunk contains only integer arrays and an integer length. Initialize the
        // pinned allocation in place so its size never becomes a temporary stack frame.
        let chunk = unsafe { Box::<TraceChunk, _>::new_zeroed_in(allocator).assume_init() };
        Self { chunk }
    }

    /// Page-locked bytes one chunk occupies: its backing `HostAllocation`, rounded up
    /// to the allocator's chunk size. Freed with `cudaFreeHost` when it is dropped.
    pub(crate) fn pinned_bytes() -> usize {
        size_of::<TraceChunk>().next_multiple_of(1 << Self::LOG_CHUNK_SIZE)
    }
}

impl Deref for LockedBoxedTraceChunk {
    type Target = TraceChunk;

    fn deref(&self) -> &Self::Target {
        &self.chunk
    }
}

impl DerefMut for LockedBoxedTraceChunk {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.chunk
    }
}

pub(crate) struct Snapshot<R: DataTraceRanges> {
    pub index: usize,
    pub cycles_count: usize,
    pub initial_state: MachineState,
    pub trace: LockedBoxedTraceChunk,
    pub final_state: MachineState,
    pub trace_ranges: R,
}

unsafe impl<R: DataTraceRanges> Send for Snapshot<R> {}

pub(crate) struct SimulationRunner<
    ND: NonDeterminismCSRSource + Send + 'static,
    T: TracingType + 'static,
> {
    pub batch_id: u64,
    pub non_determinism_source: ND,
    pub free_trace_chunks_sender: Sender<LockedBoxedTraceChunk>,
    pub free_trace_chunks_receiver: Receiver<LockedBoxedTraceChunk>,
    pub snapshots: Option<Sender<Snapshot<T::Ranges>>>,
    pub results: Option<Sender<WorkerResult<A>>>,
    pub abort: Arc<AtomicBool>,
    pub state: MachineState,
    pub trace: Option<LockedBoxedTraceChunk>,
    pub snapshot_index: usize,
    pub tracing_data_producers: Option<T::Producers>,
    pub empty_it_streamer: Option<EmptyInitsAndTeardownsStreamer>,
    pub instant: Option<Instant>,
    pub total_elapsed: Duration,
    pub is_aborted: bool,
    pub ram_config: JitRunnerRam,
}

impl<ND: NonDeterminismCSRSource + Send + 'static, T: TracingType + 'static>
    SimulationRunner<ND, T>
{
    pub fn new(
        batch_id: u64,
        machine_type: MachineType,
        non_determinism_source: ND,
        free_trace_chunks_sender: Sender<LockedBoxedTraceChunk>,
        free_trace_chunks_receiver: Receiver<LockedBoxedTraceChunk>,
        snapshots: Sender<Snapshot<T::Ranges>>,
        results: Sender<WorkerResult<A>>,
        free_allocators: Receiver<A>,
        abort: Arc<AtomicBool>,
        ram_config: JitRunnerRam,
    ) -> Self {
        assert_ne!(ram_config, JitRunnerRam::UninitPlaceholder);
        let tracing_data_producers =
            T::Producers::new(machine_type, free_allocators, results.clone());
        let tracing_data_producers = Some(tracing_data_producers);
        Self {
            batch_id,
            non_determinism_source,
            free_trace_chunks_sender,
            free_trace_chunks_receiver,
            snapshots: Some(snapshots),
            results: Some(results),
            abort,
            state: MachineState::initial(),
            trace: None,
            snapshot_index: 0,
            tracing_data_producers,
            empty_it_streamer: None,
            instant: None,
            total_elapsed: Default::default(),
            is_aborted: false,
            ram_config,
        }
    }

    pub fn run(
        mut self,
        binary_image: impl Deref<Target = impl Deref<Target = [u32]>>,
        text_section: impl Deref<Target = impl Deref<Target = [u32]>>,
        cycles_bound: Option<u32>,
        jit_cache: Arc<Mutex<TypeMap>>,
        memory_holder: &mut MemoryHolder,
    ) -> Self {
        let batch_id = self.batch_id;
        let jitted_code = {
            let mut guard = jit_cache.lock().unwrap();
            let entry = guard.get::<Arc<JittedCode<Self>>>();
            if let Some(entry) = entry {
                entry.clone()
            } else {
                trace!("BATCH[{batch_id}] SIMULATOR JIT compiling bytecode");
                let jitted_code =
                    JittedCode::preprocess_bytecode(&text_section, cycles_bound, self.ram_config);
                trace!("BATCH[{batch_id}] SIMULATOR JIT compiled bytecode");
                let jitted_code = Arc::new(jitted_code);
                guard.insert(jitted_code.clone());
                jitted_code
            }
        };
        let binary_image_len = binary_image.len();
        assert!(binary_image_len <= ROM_WORD_SIZE);
        // NOTE: on reuse the holder is already clean outside the ROM region: every RAM word
        // the JIT writes is timestamped, and `collect_inits_and_teardowns` zeroes both the
        // value and the timestamp of every timestamped word (the abort path resets the whole
        // buffer). The binary image is the exception: it is copied in without timestamps, so
        // ROM words the program never loads survive collection. Only the ROM tail beyond the
        // current image needs an explicit clear, in case the previous image was longer.
        memory_holder.memory_mut()[..binary_image_len].copy_from_slice(&binary_image);
        memory_holder.memory_mut()[binary_image_len..ROM_WORD_SIZE].fill(0);
        let mut trace = self
            .free_trace_chunks_receiver
            .recv()
            .expect("must receive a trace chunk for simulation");
        trace.chunk.len = 0;
        let trace_ref = unsafe { NonNull::new_unchecked(trace.chunk.as_mut()) };
        self.trace = Some(trace);
        self.instant = Some(Instant::now());
        let ram_config = self.ram_config;
        let mut context = Context::new(self, ram_config);
        jitted_code.run_over_prepared_memory(&mut context, memory_holder, trace_ref);
        let mut runner = context.into_implementation();
        if let Some(trace) = runner.trace.take() {
            runner.free_trace_chunks_sender.send(trace).unwrap();
        }
        if !runner.is_aborted {
            let final_timestamp = runner.state.timestamp;
            let timestamp_diff = final_timestamp - INITIAL_TIMESTAMP;
            assert!(timestamp_diff.is_multiple_of(TIMESTAMP_STEP));
            let cycles_count = (timestamp_diff / TIMESTAMP_STEP) as usize;
            let elapsed_ms = runner.total_elapsed.as_secs_f64() * 1000.0;
            let mhz = (cycles_count as f64) / (elapsed_ms * 1000.0);
            debug!("BATCH[{batch_id}] SIMULATOR finished execution with {cycles_count} cycles in {elapsed_ms:.3} ms @ {mhz:.3} MHz");
        }
        runner
    }

    fn process_trace(&mut self, machine_state: &MachineState, elapsed: Duration) {
        if self.is_aborted {
            return;
        }
        let batch_id = self.batch_id;
        let snapshot_index = self.snapshot_index;
        self.snapshot_index += 1;
        let mut machine_state = *machine_state;
        let timestamp = machine_state.timestamp.next_multiple_of(TIMESTAMP_STEP); // align timestamp, needs to be fixed in the VM
        machine_state.timestamp = timestamp;
        let final_state = machine_state;
        let initial_state = replace(&mut self.state, machine_state);
        let timestamp_diff = timestamp - initial_state.timestamp;
        assert!(timestamp_diff.is_multiple_of(TIMESTAMP_STEP));
        let cycles_count = (timestamp_diff / TIMESTAMP_STEP) as usize;
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let mhz = (cycles_count as f64) / (elapsed_ms * 1000.0);
        trace!("BATCH[{batch_id}] SIMULATOR produced SNAPSHOT[{snapshot_index}] with {cycles_count} cycles in {elapsed_ms:.3} ms @ {mhz:.3} MHz");
        if self.abort.load(std::sync::atomic::Ordering::Relaxed) {
            self.tracing_data_producers.take().unwrap().finalize();
            assert!(self.snapshots.take().is_some());
            assert!(self.results.take().is_some());
            let timestamp_diff = timestamp - INITIAL_TIMESTAMP;
            assert!(timestamp_diff.is_multiple_of(TIMESTAMP_STEP));
            let cycles_count = (timestamp_diff / TIMESTAMP_STEP) as usize;
            let elapsed_ms = self.total_elapsed.as_secs_f64() * 1000.0;
            let mhz = (cycles_count as f64) / (elapsed_ms * 1000.0);
            debug!("BATCH[{batch_id}] SIMULATOR stopping snapshot production due to abort signal after {cycles_count} cycles in {elapsed_ms:.3} ms @ {mhz:.3} MHz");
            self.is_aborted = true;
            return;
        }
        // Send provably empty unified markers before process_snapshot can block
        // receiving a host allocator. This lets earlier tracing results pair
        // and dispatch instead of pinning their allocators until the VM ends.
        if let (Some(streamer), Some(results)) =
            (self.empty_it_streamer.as_mut(), self.results.as_ref())
        {
            let cycles_so_far = ((timestamp - INITIAL_TIMESTAMP) / TIMESTAMP_STEP) as usize;
            for sequence_id in streamer.release(cycles_so_far) {
                let data = InitsAndTeardownsData {
                    circuit_type: CircuitType::Unrolled(UnrolledCircuitType::Unified),
                    sequence_id,
                    inits_and_teardowns: None,
                };
                results
                    .send(WorkerResult::InitsAndTeardownsData(data))
                    .expect(
                    "simulation results channel closed while streaming empty init/teardown markers",
                );
            }
        }
        let trace = self.trace.take().unwrap();
        let result = WorkerResult::SnapshotProduced;
        self.results.as_ref().unwrap().send(result).unwrap();
        let counters_diff = machine_state
            .counters
            .iter()
            .zip_eq(initial_state.counters.iter())
            .map(|(a, b)| a - b)
            .collect_array::<MAX_NUM_COUNTERS>()
            .unwrap();
        let expected_cycles = counters_diff.iter().take(6).sum::<u64>() as usize;
        assert_eq!(expected_cycles, cycles_count);
        let trace_ranges = self
            .tracing_data_producers
            .as_mut()
            .unwrap()
            .process_snapshot(
                snapshot_index,
                &initial_state.counters,
                &machine_state.counters,
            );
        let snapshot = Snapshot {
            index: snapshot_index,
            cycles_count,
            initial_state,
            trace,
            final_state,
            trace_ranges,
        };
        self.snapshots.as_ref().unwrap().send(snapshot).unwrap();
    }
}

impl<ND: NonDeterminismCSRSource + Send + 'static, T: TracingType> ContextImpl
    for SimulationRunner<ND, T>
{
    #[inline(always)]
    fn read_nondeterminism(&mut self) -> u32 {
        self.non_determinism_source.read()
    }

    #[inline(always)]
    fn write_nondeterminism(&mut self, value: u32, memory: &[u32]) {
        self.non_determinism_source
            .write_with_memory_access(&memory, value);
    }

    fn receive_trace(
        &mut self,
        trace_piece: NonNull<TraceChunk>,
        machine_state: &MachineState,
    ) -> NonNull<TraceChunk> {
        let elapsed = self.instant.take().unwrap().elapsed();
        self.total_elapsed += elapsed;
        let argument_ptr = trace_piece.as_ptr();
        let current_ptr = self.trace.as_mut().unwrap().deref_mut() as *mut TraceChunk;
        assert_eq!(argument_ptr, current_ptr);
        self.process_trace(machine_state, elapsed);
        if self.trace.is_none() {
            let trace = self.free_trace_chunks_receiver.recv().unwrap();
            self.trace = Some(trace);
        }
        self.trace.as_mut().unwrap().chunk.len = 0;
        let ptr = self.trace.as_mut().unwrap().deref_mut() as *mut TraceChunk;
        self.instant = Some(Instant::now());
        NonNull::new(ptr).unwrap()
    }

    fn receive_final_trace_piece(
        &mut self,
        trace_piece: NonNull<TraceChunk>,
        machine_state: &MachineState,
    ) {
        let elapsed = self.instant.take().unwrap().elapsed();
        self.total_elapsed += elapsed;
        debug_assert!(
            (machine_state as *const MachineState).is_aligned_to(align_of::<MachineState>())
        );
        let argument_ptr = trace_piece.as_ptr();
        let current_ptr = self.trace.as_mut().unwrap().deref_mut() as *mut TraceChunk;
        assert_eq!(argument_ptr, current_ptr);
        self.process_trace(machine_state, elapsed);
        if !self.is_aborted {
            self.tracing_data_producers.take().unwrap().finalize();
        }
    }

    fn take_final_state(&mut self) -> Option<MachineState> {
        unreachable!()
    }

    fn final_state_ref(&'_ self) -> Option<&'_ MachineState> {
        unreachable!()
    }
}
