use super::A;
use crate::circuit_type::{CircuitType, UnrolledCircuitType};
use crate::execution::messages::SimulationResult;
use crate::execution::messages::{InitsAndTeardownsData, WorkerResult};
use crate::execution::simulation_runner::{
    EmptyInitsAndTeardownsStreamer, LockedBoxedMemoryHolder, LockedBoxedTraceChunk,
    SimulationRunner, Snapshot,
};
use crate::execution::tracing::{Tracer, TracingType};
use crate::machine_type::MachineType;
use crate::witness::trace_unrolled::ShuffleRamInitsAndTeardownsHost;
use crossbeam_channel::{Receiver, Sender};
use cs::definitions::TimestampData;
use itertools::Itertools;
use log::{debug, trace};
use prover::common_constants;
use prover::definitions::LazyInitAndTeardown;
use riscv_transpiler::abstractions::non_determinism::QuasiUARTSource;
use riscv_transpiler::common_constants::{INITIAL_TIMESTAMP, TIMESTAMP_STEP};
use riscv_transpiler::jit::{MemoryHolder, ReplayerMemChunks};
use riscv_transpiler::replayer::ReplayerVM;
use riscv_transpiler::vm::{InstructionTape, NonDeterminismCSRSource, State};
use std::cmp::min;
use std::ops::Deref;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use trace_and_split::FinalRegisterValue;
use type_map::concurrent::TypeMap;
use worker::Worker;

pub(crate) fn run_simulator<
    ND: NonDeterminismCSRSource + Send + 'static,
    T: TracingType + 'static,
>(
    batch_id: u64,
    machine_type: MachineType,
    binary_image: impl Deref<Target = impl Deref<Target = [u32]>>,
    text_section: impl Deref<Target = impl Deref<Target = [u32]>>,
    cycles_bound: Option<u32>,
    jit_cache: Arc<Mutex<TypeMap>>,
    memory_holder: &mut LockedBoxedMemoryHolder,
    non_determinism: Arc<Mutex<Option<ND>>>,
    free_trace_chunks_sender: Sender<LockedBoxedTraceChunk>,
    free_trace_chunks_receiver: Receiver<LockedBoxedTraceChunk>,
    snapshots: Sender<Snapshot<T::Ranges>>,
    results: Sender<WorkerResult<A>>,
    free_allocators: Receiver<A>,
    abort: Arc<AtomicBool>,
    worker: &Worker,
) {
    trace!("BATCH[{batch_id}] SIMULATOR started");
    let mut non_determinism_guard = non_determinism.lock().unwrap();
    let non_determinism_source = non_determinism_guard.take().unwrap();
    let mut runner = SimulationRunner::<_, T>::new(
        batch_id,
        machine_type,
        non_determinism_source,
        free_trace_chunks_sender,
        free_trace_chunks_receiver,
        snapshots,
        results,
        free_allocators.clone(),
        abort,
    );
    // The pinned unified circuit has one I&T record per usable row. Collection
    // emits at most one record per RAM word, so its trailing I&T occupancy is
    // bounded even if a delegation touches many words in a single VM cycle.
    runner.empty_it_streamer = (!T::IS_SPLIT).then(|| {
        EmptyInitsAndTeardownsStreamer::new(
            memory_holder.memory.len(),
            setups::unified_reduced_machine::NUM_CYCLES,
        )
    });
    let runner = runner.run(
        binary_image,
        text_section,
        cycles_bound,
        jit_cache,
        memory_holder,
    );
    let SimulationRunner {
        batch_id,
        non_determinism_source,
        results,
        abort,
        state,
        is_aborted,
        empty_it_streamer,
        ..
    } = runner;
    *non_determinism_guard = Some(non_determinism_source);
    let should_abort = abort.load(std::sync::atomic::Ordering::Relaxed);
    if !should_abort {
        assert!(!is_aborted);
        let results = results.unwrap();
        let instant = Instant::now();
        let inits_and_teardowns = collect_inits_and_teardowns(memory_holder, worker);
        let elapsed = instant.elapsed();
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let count = inits_and_teardowns.iter().map(|v| v.len()).sum::<usize>();
        trace!("BATCH[{batch_id}] SIMULATOR collected INITS_AND_TEARDOWNS with {count} entries in {elapsed_ms:.3} ms");
        let mut instant = Instant::now();
        let (circuit_type, per_circuit_count, sequence_id_offset) = if T::IS_SPLIT {
            let per_circuit_count = setups::inits_and_teardowns::NUM_INIT_AND_TEARDOWN_SETS
                * setups::inits_and_teardowns::NUM_CYCLES;
            (UnrolledCircuitType::InitsAndTeardowns, per_circuit_count, 0)
        } else {
            let per_circuit_count = setups::unified_reduced_machine::NUM_CYCLES;
            let timestamp_diff = state.timestamp - INITIAL_TIMESTAMP;
            assert!(timestamp_diff.is_multiple_of(TIMESTAMP_STEP));
            let total_cycles = (timestamp_diff / TIMESTAMP_STEP) as usize;
            let empty_cycles = total_cycles - count;
            let empty_circuits = empty_cycles / per_circuit_count;
            // Preserve this revision's exact final empty prefix, including a
            // partial final circuit; emit only markers not already streamed.
            let remaining_empty = empty_it_streamer
                .expect("unified simulation requires an empty init/teardown streamer")
                .finish(empty_circuits);
            for sequence_id in remaining_empty {
                let data = InitsAndTeardownsData {
                    circuit_type: CircuitType::Unrolled(UnrolledCircuitType::Unified),
                    sequence_id,
                    inits_and_teardowns: None,
                };
                let result = WorkerResult::InitsAndTeardownsData(data);
                results.send(result).unwrap();
            }
            (
                UnrolledCircuitType::Unified,
                per_circuit_count,
                empty_circuits,
            )
        };
        let circuit_type = CircuitType::Unrolled(circuit_type);
        for (sequence_id, inits_and_teardowns_data) in
            get_inits_and_teardowns_chunks(inits_and_teardowns, per_circuit_count, free_allocators)
                .enumerate()
        {
            let sequence_id = sequence_id + sequence_id_offset;
            let count = inits_and_teardowns_data
                .chunks
                .iter()
                .map(|c| c.len())
                .sum::<usize>();
            let elapsed = instant.elapsed();
            let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
            trace!("BATCH[{batch_id}] SIMULATOR produced INITS_AND_TEARDOWNS[{sequence_id}] with {count} entries in {elapsed_ms:.3} ms");
            let data = InitsAndTeardownsData {
                circuit_type,
                sequence_id,
                inits_and_teardowns: Some(inits_and_teardowns_data),
            };
            let result = WorkerResult::InitsAndTeardownsData(data);
            results.send(result).unwrap();
            instant = Instant::now();
        }
        let final_register_values = state
            .registers
            .into_iter()
            .zip(state.register_timestamps.into_iter())
            .map(|(value, last_access_timestamp)| FinalRegisterValue {
                value,
                last_access_timestamp,
            })
            .collect_array()
            .unwrap();
        let simulation_result = SimulationResult {
            final_register_values,
            final_pc: state.pc,
            final_timestamp: state.timestamp,
        };
        let result = WorkerResult::SimulationResult(simulation_result);
        results.send(result).unwrap();
    } else {
        trace!("BATCH[{batch_id}] SIMULATOR resetting memory due to abort");
        MemoryHolder::reset(&mut memory_holder.holder);
    }
    trace!("BATCH[{batch_id}] SIMULATOR finished");
}

pub(crate) fn run_replayer<T: TracingType>(
    batch_id: u64,
    worker_id: usize,
    tape: impl Deref<Target = impl InstructionTape>,
    snapshots: Receiver<Snapshot<T::Ranges>>,
    free_trace_chunks: Sender<LockedBoxedTraceChunk>,
    results: Sender<WorkerResult<A>>,
    abort: Arc<AtomicBool>,
) {
    trace!("BATCH[{batch_id}] REPLAYER[{worker_id}] started");
    let mut total_elapsed = Duration::default();
    let mut total_cycles = 0;
    let mut is_aborted = false;
    for snapshot in snapshots {
        if !is_aborted & abort.load(std::sync::atomic::Ordering::Relaxed) {
            debug!("BATCH[{batch_id}] REPLAYER[{worker_id}] aborting");
            is_aborted = true;
            if total_cycles != 0 {
                let elapsed_ms = total_elapsed.as_secs_f64() * 1000.0;
                let mhz = (total_cycles as f64) / (elapsed_ms * 1000.0);
                debug!("BATCH[{batch_id}] REPLAYER[{worker_id}] aborted replay after {total_cycles} cycles in {elapsed_ms:.3} ms @ {mhz:.3} MHz");
            }
        }
        let Snapshot {
            index,
            cycles_count,
            initial_state,
            trace,
            final_state,
            trace_ranges,
        } = snapshot;
        if is_aborted {
            free_trace_chunks.send(trace).unwrap();
            continue;
        }
        let trace_len = trace.len as usize;
        let mut state = initial_state.into();
        let final_state: State<T::Counters> = final_state.into();
        let mut ram = ReplayerMemChunks {
            chunks: &mut [(&trace.values[..trace_len], &trace.timestamps[..trace_len])],
        };
        let mut nd = QuasiUARTSource::new_with_reads(vec![]);
        let mut tracer = T::Tracer::new(trace_ranges);
        let instant = Instant::now();
        ReplayerVM::<T::Counters>::replay_basic_unrolled(
            &mut state,
            &mut ram,
            tape.deref(),
            &mut nd,
            cycles_count,
            &mut tracer,
        );
        let elapsed = instant.elapsed();
        drop(tracer);
        free_trace_chunks.send(trace).unwrap();
        assert_eq!(state.pc, final_state.pc);
        assert_eq!(state.timestamp, final_state.timestamp);
        assert_eq!(state.registers, final_state.registers);
        total_elapsed += elapsed;
        total_cycles += cycles_count;
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let mhz = (cycles_count as f64) / (elapsed_ms * 1000.0);
        trace!("BATCH[{batch_id}] REPLAYER[{worker_id}] processed SNAPSHOT[{index}] with {cycles_count} cycles in {elapsed_ms:.3} ms @ {mhz:.3} MHz");
        let result = WorkerResult::SnapshotReplayed(index);
        results.send(result).unwrap()
    }
    let elapsed_ms = total_elapsed.as_secs_f64() * 1000.0;
    let mhz = (total_cycles as f64) / (elapsed_ms * 1000.0);
    if !is_aborted && total_cycles != 0 {
        debug!("BATCH[{batch_id}] REPLAYER[{worker_id}] replayed {total_cycles} cycles in {elapsed_ms:.3} ms @ {mhz:.3} MHz");
    }
    trace!("BATCH[{batch_id}] REPLAYER[{worker_id}] finished");
}

pub(super) fn collect_inits_and_teardowns(
    holder: &mut MemoryHolder,
    worker: &Worker,
) -> Vec<Vec<LazyInitAndTeardown>> {
    let mut chunks = vec![vec![]; worker.get_num_cores()];
    let mut dst = &mut chunks[..];
    let mut memory = &mut holder.memory[..];
    let mut timestamps_rest = &mut holder.timestamps[..];
    let memory_len = memory.len();
    worker.scope(memory_len, |scope, geometry| {
        for thread_idx in 0..geometry.len() {
            let chunk_size = geometry.get_chunk_size(thread_idx);
            let chunk_start = geometry.get_chunk_start_pos(thread_idx);
            let (el, rest) = dst.split_at_mut(1);
            dst = rest;
            debug_assert_eq!(chunk_start, memory_len - memory.len());
            debug_assert!(thread_idx + 1 < geometry.len() || memory.len() == chunk_size);
            // Each worker owns disjoint mutable slices while collecting and clearing them.
            let (values, rest) = memory.split_at_mut(chunk_size);
            memory = rest;
            let (timestamps, rest) = timestamps_rest.split_at_mut(chunk_size);
            timestamps_rest = rest;
            Worker::smart_spawn(scope, thread_idx == geometry.len() - 1, move |_| {
                let el = &mut el[0];
                for (idx, (value, timestamp)) in values.iter_mut().zip(timestamps).enumerate() {
                    if *timestamp != 0 {
                        let teardown_timestamp = TimestampData::from_scalar(*timestamp);
                        *timestamp = 0;
                        let mut teardown_value = core::mem::take(value);
                        let address = (chunk_start + idx) << 2;
                        if address < common_constants::rom::ROM_BYTE_SIZE {
                            teardown_value = 0;
                        }
                        el.push(LazyInitAndTeardown {
                            address: address as u32,
                            teardown_value,
                            teardown_timestamp,
                        });
                    }
                }
            });
        }
    });

    chunks
}

pub(super) fn get_inits_and_teardowns_chunks(
    values: Vec<Vec<LazyInitAndTeardown>>,
    partition_size: usize,
    free_allocators: Receiver<A>,
) -> impl Iterator<Item = ShuffleRamInitsAndTeardownsHost<A>> {
    let inits_and_teardowns_count = values.iter().map(|v| v.len()).sum::<usize>();
    let mut iterator = values.into_iter().flat_map(|v| v.into_iter());
    let partitions_count = inits_and_teardowns_count.div_ceil(partition_size);
    (0..partitions_count).into_iter().map(move |index| {
        let mut values_count = if index == 0 {
            inits_and_teardowns_count - (partitions_count - 1) * partition_size
        } else {
            partition_size
        };
        let mut chunks = vec![];
        while values_count != 0 {
            let allocator = free_allocators.recv().unwrap();
            let capacity = allocator.capacity() / size_of::<LazyInitAndTeardown>();
            let count = min(capacity, values_count);
            let mut chunk = Vec::with_capacity_in(count, allocator);
            unsafe {
                chunk.set_len(count);
                for i in 0..count {
                    *chunk.get_unchecked_mut(i) = iterator.next().unwrap_unchecked();
                }
            };
            chunks.push(Arc::new(chunk));
            values_count -= count;
        }
        ShuffleRamInitsAndTeardownsHost { chunks }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_collection_preserves_partitions_and_clears_reused_holder() {
        // Zero is valid for both arrays, and heap allocation avoids a multi-GiB stack value.
        let mut holder = unsafe { Box::<MemoryHolder>::new_zeroed().assume_init() };
        let worker = Worker::new_with_num_threads(3);
        let geometry = worker.get_geometry(holder.memory.len());
        let rom_words = common_constants::rom::ROM_BYTE_SIZE / size_of::<u32>();
        let mut indices = vec![0, rom_words - 1, rom_words, holder.memory.len() - 1];
        for partition in 1..geometry.len() {
            let start = geometry.get_chunk_start_pos(partition);
            indices.extend([start - 1, start]);
        }
        indices.sort_unstable();
        indices.dedup();
        holder.memory[7] = 0x12345678;

        let expected = indices
            .iter()
            .enumerate()
            .map(|(offset, &index)| {
                let value = 100 + offset as u32;
                let timestamp = INITIAL_TIMESTAMP + (offset as u64 + 1) * TIMESTAMP_STEP;
                holder.memory[index] = value;
                holder.timestamps[index] = timestamp;
                LazyInitAndTeardown {
                    address: (index * size_of::<u32>()) as u32,
                    teardown_value: if index < rom_words { 0 } else { value },
                    teardown_timestamp: TimestampData::from_scalar(timestamp),
                }
            })
            .collect::<Vec<_>>();
        let chunks = collect_inits_and_teardowns(&mut holder, &worker);
        for (partition, values) in chunks.iter().enumerate() {
            let start = geometry.get_chunk_start_pos(partition) * size_of::<u32>();
            let end = start + geometry.get_chunk_size(partition) * size_of::<u32>();
            assert!(values.iter().all(|value| {
                start <= value.address as usize && (value.address as usize) < end
            }));
        }
        assert_eq!(chunks.concat(), expected);
        for &index in &indices {
            assert_eq!(holder.memory[index], 0);
            assert_eq!(holder.timestamps[index], 0);
        }
        assert_eq!(holder.memory[7], 0x12345678);
        assert!(collect_inits_and_teardowns(&mut holder, &worker)
            .iter()
            .all(Vec::is_empty));

        let reused_index = geometry.get_chunk_start_pos(1);
        holder.memory[reused_index] = 0xabcdef01;
        holder.timestamps[reused_index] = INITIAL_TIMESTAMP + 9 * TIMESTAMP_STEP;
        assert_eq!(
            collect_inits_and_teardowns(&mut holder, &worker).concat(),
            vec![LazyInitAndTeardown {
                address: (reused_index * size_of::<u32>()) as u32,
                teardown_value: 0xabcdef01,
                teardown_timestamp: TimestampData::from_scalar(
                    INITIAL_TIMESTAMP + 9 * TIMESTAMP_STEP
                ),
            }],
        );
        assert_eq!(holder.memory[reused_index], 0);
        assert_eq!(holder.timestamps[reused_index], 0);
    }
}
