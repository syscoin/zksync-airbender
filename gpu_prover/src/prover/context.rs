use crate::allocator::device::{
    NonConcurrentStaticDeviceAllocation, NonConcurrentStaticDeviceAllocator,
    StaticDeviceAllocationBackend,
};
use crate::allocator::host::{ConcurrentStaticHostAllocator, NonConcurrentStaticHostAllocator};
use crate::allocator::tracker::AllocationPlacement;
use crate::device_context::DeviceContext;
use crate::prover::gpu_memory::{
    allocate_arena_with, GpuMemoryPreset, ProductionMemoryPreset, PRODUCTION_SMALL_POOL_BYTES,
};
use era_cudart::device::{device_get_attribute, get_device, set_device};
use era_cudart::memory::CudaHostAllocFlags;
use era_cudart::result::CudaResult;
use era_cudart::slice::{CudaSlice, CudaSliceMut};
use era_cudart::stream::CudaStream;
use era_cudart_sys::{CudaDeviceAttr, CudaError};
use log::error;
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

pub struct DeviceProperties {
    pub l2_cache_size_bytes: usize,
    pub sm_count: usize,
}

impl DeviceProperties {
    pub fn new() -> CudaResult<Self> {
        let device_id = get_device()?;
        let l2_cache_size_bytes =
            device_get_attribute(CudaDeviceAttr::L2CacheSize, device_id)? as usize;
        let sm_count =
            device_get_attribute(CudaDeviceAttr::MultiProcessorCount, device_id)? as usize;
        Ok(Self {
            l2_cache_size_bytes,
            sm_count,
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ProverContextConfig {
    pub powers_of_w_coarse_log_count: u32,
    pub allocator_block_log_size: u32,
    pub device_slack_static_bytes: usize,
    pub device_slack_per_thread_bytes: usize,
    pub max_device_allocation_blocks_count: Option<usize>,
    pub host_allocator_blocks_count: usize,
    pub gpu_memory_preset: GpuMemoryPreset,
}

impl Default for ProverContextConfig {
    fn default() -> Self {
        Self {
            powers_of_w_coarse_log_count: 12,
            allocator_block_log_size: 20,             // 1 MB blocks
            device_slack_static_bytes: 1 << 27,       // 128 MB static slack
            device_slack_per_thread_bytes: 1 << 11,   // 2 KB per thread slack
            max_device_allocation_blocks_count: None, // use preset arena size
            host_allocator_blocks_count: 1024,        // 1 GB host allocator pool
            gpu_memory_preset: GpuMemoryPreset::Auto,
        }
    }
}

pub type DeviceAllocator = NonConcurrentStaticDeviceAllocator;
pub type DeviceAllocation<T> = NonConcurrentStaticDeviceAllocation<T>;
pub type HostAllocator = NonConcurrentStaticHostAllocator;

const SMALL_ALLOCATOR_BLOCK_LOG_SIZE: u32 = 8;

fn allocator_chunk_bytes(log_chunk_size: u32) -> CudaResult<usize> {
    let chunk_bytes = 1usize
        .checked_shl(log_chunk_size)
        .ok_or(CudaError::ErrorInvalidValue)?;
    if log_chunk_size <= SMALL_ALLOCATOR_BLOCK_LOG_SIZE
        || !PRODUCTION_SMALL_POOL_BYTES.is_multiple_of(chunk_bytes)
    {
        return Err(CudaError::ErrorInvalidValue);
    }
    Ok(chunk_bytes)
}

pub struct ProverContext {
    _device_context: DeviceContext,
    device_allocator: DeviceAllocator,
    host_pool: Rc<HostPool>,
    aux_stream: CudaStream,
    h2d_stream: CudaStream,
    device_allocator_mem_size: usize,
    memory_preset: ProductionMemoryPreset,
    device_id: i32,
    device_properties: DeviceProperties,
    reversed_allocation_placement: bool,
}

impl ProverContext {
    pub fn is_global_host_allocator_initialized() -> bool {
        ConcurrentStaticHostAllocator::is_initialized_global()
    }

    pub fn initialize_global_host_allocator(
        host_allocations_count: usize,
        blocks_per_allocation_count: usize,
        block_log_size: u32,
    ) -> CudaResult<()> {
        assert!(
            !Self::is_global_host_allocator_initialized(),
            "Global host allocator can only be initialized once"
        );
        let host_allocation_size = blocks_per_allocation_count << block_log_size;
        let allocations: Vec<CudaResult<era_cudart::memory::HostAllocation<u8>>> = (0
            ..host_allocations_count)
            .into_par_iter()
            .map(|_| {
                era_cudart::memory::HostAllocation::alloc(
                    host_allocation_size,
                    CudaHostAllocFlags::DEFAULT,
                )
            })
            .collect();
        let mut backends = vec![];
        for allocation in allocations {
            match allocation {
                Ok(alloc) => backends.push(alloc),
                Err(e) => return Err(e),
            }
        }
        ConcurrentStaticHostAllocator::initialize_global(backends, block_log_size);
        Ok(())
    }

    pub fn new(config: &ProverContextConfig) -> CudaResult<Self> {
        let chunk_bytes = allocator_chunk_bytes(config.allocator_block_log_size)?;
        let device_id = get_device()?;
        let mpc = device_get_attribute(CudaDeviceAttr::MultiProcessorCount, device_id)? as usize;
        let max_threads_per_mpc =
            device_get_attribute(CudaDeviceAttr::MaxThreadsPerMultiProcessor, device_id)? as usize;
        let max_threads_count = mpc * max_threads_per_mpc;
        let device_slack_threads_bytes = config.device_slack_per_thread_bytes * max_threads_count;
        let slack_size = config.device_slack_static_bytes + device_slack_threads_bytes;
        let slack = era_cudart::memory::DeviceAllocation::<u8>::alloc(slack_size)?;
        let allocator_block_log_size = config.allocator_block_log_size;
        let device_context = DeviceContext::create(config.powers_of_w_coarse_log_count)?;
        let exec_stream = CudaStream::create()?;
        let aux_stream = CudaStream::create()?;
        let h2d_stream = CudaStream::create()?;

        let exact_arena_bytes = config
            .max_device_allocation_blocks_count
            .map(|blocks| {
                blocks
                    .checked_mul(chunk_bytes)
                    .ok_or(CudaError::ErrorInvalidValue)
            })
            .transpose()?;
        let (memory_preset, (arena_bytes, device_allocation)) =
            allocate_arena_with(config.gpu_memory_preset, exact_arena_bytes, |arena_bytes| {
                if arena_bytes == 0
                    || !arena_bytes.is_multiple_of(chunk_bytes)
                    || PRODUCTION_SMALL_POOL_BYTES >= arena_bytes
                {
                    return Err(CudaError::ErrorInvalidValue);
                }
                era_cudart::memory::DeviceAllocation::<u8>::alloc(arena_bytes)
                    .map(|allocation| (arena_bytes, allocation))
            })?;
        if config.gpu_memory_preset == GpuMemoryPreset::Auto
            && memory_preset == ProductionMemoryPreset::Low
        {
            let _ = era_cudart::error::get_last_error();
        }
        slack.free()?;
        log::debug!(
            "GPU device memory pool created: {} bytes ({} blocks of 2^{} bytes) on device {device_id}",
            arena_bytes,
            arena_bytes >> allocator_block_log_size,
            allocator_block_log_size,
        );
        let device_allocation_backend =
            StaticDeviceAllocationBackend::DeviceAllocation(device_allocation);
        let device_allocator = NonConcurrentStaticDeviceAllocator::new_with_small_allocator(
            [device_allocation_backend],
            allocator_block_log_size,
            SMALL_ALLOCATOR_BLOCK_LOG_SIZE,
            PRODUCTION_SMALL_POOL_BYTES,
        );
        let device_allocator_mem_size = arena_bytes;
        let host_allocation_size = config.host_allocator_blocks_count << allocator_block_log_size;
        let host_allocation = era_cudart::memory::HostAllocation::alloc(
            host_allocation_size,
            CudaHostAllocFlags::DEFAULT,
        )?;
        let host_allocator =
            NonConcurrentStaticHostAllocator::new([host_allocation], allocator_block_log_size);
        let device_properties = DeviceProperties::new()?;
        let context = Self {
            _device_context: device_context,
            device_allocator,
            host_pool: Rc::new(HostPool {
                allocator: host_allocator,
                stream: exec_stream,
                retired: RefCell::new(Vec::new()),
            }),
            aux_stream,
            h2d_stream,
            device_allocator_mem_size,
            memory_preset,
            device_id,
            device_properties,
            reversed_allocation_placement: false,
        };
        Ok(context)
    }

    pub fn get_host_allocator(&self) -> HostAllocator {
        self.host_pool.allocator.clone()
    }

    pub fn get_device_id(&self) -> i32 {
        self.device_id
    }

    pub fn switch_to_device(&self) -> CudaResult<()> {
        set_device(self.device_id)
    }

    pub fn get_exec_stream(&self) -> &CudaStream {
        &self.host_pool.stream
    }

    pub fn get_aux_stream(&self) -> &CudaStream {
        &self.aux_stream
    }

    pub fn get_h2d_stream(&self) -> &CudaStream {
        &self.h2d_stream
    }

    pub fn alloc<T>(
        &self,
        size: usize,
        placement: AllocationPlacement,
    ) -> CudaResult<DeviceAllocation<T>> {
        let placement = if self.reversed_allocation_placement {
            match placement {
                AllocationPlacement::BestFit => AllocationPlacement::BestFit,
                AllocationPlacement::Bottom => AllocationPlacement::Top,
                AllocationPlacement::Top => AllocationPlacement::Bottom,
            }
        } else {
            placement
        };
        let result = self.device_allocator.alloc(size, placement);
        if result.is_err() {
            error!(
                "failed to allocate {} bytes from GPU memory allocator of device ID {}, currently allocated {} bytes (pool capacity {} bytes, peak used {} bytes, largest free range {} bytes)",
                size * size_of::<T>(),
                self.device_id,
                self.get_used_mem_current(),
                self.device_allocator.get_capacity(),
                self.get_used_mem_peak(),
                self.device_allocator.get_largest_free_range(),
            );
        }
        result
    }

    pub(crate) fn alloc_host<T: 'static>(&self, value: T) -> CudaResult<HostAllocation<T>> {
        self.host_pool.prepare(size_of::<T>())?;
        let data = Box::try_new_in(value, self.get_host_allocator())
            .map_err(|_| CudaError::ErrorMemoryAllocation)?;
        Ok(HostAllocation::new(data, self.host_pool.clone()))
    }

    pub(crate) fn alloc_host_slice<T: Default + 'static>(
        &self,
        len: usize,
    ) -> CudaResult<HostAllocation<[T]>> {
        let size = size_of::<T>()
            .checked_mul(len)
            .ok_or(CudaError::ErrorMemoryAllocation)?;
        self.host_pool.prepare(size)?;
        let mut buffer = Vec::new_in(self.get_host_allocator());
        buffer
            .try_reserve_exact(len)
            .map_err(|_| CudaError::ErrorMemoryAllocation)?;
        // CUDA copy APIs borrow typed slices even when filling them from the device.
        buffer.resize_with(len, T::default);
        Ok(HostAllocation::new(
            buffer.into_boxed_slice(),
            self.host_pool.clone(),
        ))
    }

    pub fn get_mem_size(&self) -> usize {
        self.device_allocator_mem_size
    }

    pub(crate) fn get_memory_preset(&self) -> ProductionMemoryPreset {
        self.memory_preset
    }

    pub fn get_used_mem_current(&self) -> usize {
        self.device_allocator.get_used_mem_current()
    }

    #[cfg(feature = "memory_sweep")]
    pub(crate) fn get_device_allocator(&self) -> &DeviceAllocator {
        &self.device_allocator
    }

    pub fn get_used_mem_peak(&self) -> usize {
        self.device_allocator.get_used_mem_peak()
    }

    pub fn reset_used_mem_peak(&self) {
        self.device_allocator.reset_used_mem_peak();
    }

    #[cfg(feature = "log_gpu_mem_usage")]
    pub fn log_gpu_mem_usage(&self, location: &str) {
        let used_mem_current = self.get_used_mem_current();
        let used_mem_peak = self.get_used_mem_peak();
        log::debug!(
            "GPU memory usage {location} current/peak: {}/{} GB",
            used_mem_current as f64 / ((1 << 30) as f64),
            used_mem_peak as f64 / ((1 << 30) as f64),
        );
    }

    pub fn get_device_properties(&self) -> &DeviceProperties {
        &self.device_properties
    }

    pub fn is_reversed_allocation_placement(&self) -> bool {
        self.reversed_allocation_placement
    }

    pub fn set_reversed_allocation_placement(&mut self, reversed: bool) {
        self.reversed_allocation_placement = reversed;
    }
}

#[repr(transparent)]
pub(crate) struct UnsafeAccessor<T: ?Sized>(*const T);

impl<T: ?Sized> UnsafeAccessor<T> {
    pub fn new(value: &T) -> Self {
        UnsafeAccessor(value as *const T)
    }

    pub unsafe fn get(&self) -> &T {
        &*self.0
    }
}

impl<T: ?Sized> Clone for UnsafeAccessor<T> {
    fn clone(&self) -> Self {
        UnsafeAccessor(self.0)
    }
}

impl<T: ?Sized> Copy for UnsafeAccessor<T> {}

unsafe impl<T: ?Sized> Send for UnsafeAccessor<T> {}
unsafe impl<T: ?Sized> Sync for UnsafeAccessor<T> {}

#[repr(transparent)]
pub(crate) struct UnsafeMutAccessor<T: ?Sized>(*mut T);

impl<T: ?Sized> UnsafeMutAccessor<T> {
    pub fn new(value: &mut T) -> Self {
        UnsafeMutAccessor(value as *mut T)
    }

    pub unsafe fn get(&self) -> &T {
        &*self.0
    }

    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *(self.0)
    }

    pub unsafe fn set(&self, value: T)
    where
        T: Sized,
    {
        *(self.0) = value;
    }
}

impl<T: ?Sized> Clone for UnsafeMutAccessor<T> {
    fn clone(&self) -> Self {
        UnsafeMutAccessor(self.0)
    }
}

impl<T: ?Sized> Copy for UnsafeMutAccessor<T> {}

unsafe impl<T: ?Sized> Send for UnsafeMutAccessor<T> {}
unsafe impl<T: ?Sized> Sync for UnsafeMutAccessor<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_block_size_must_support_the_small_pool() {
        assert_eq!(allocator_chunk_bytes(9).unwrap(), 1 << 9);
        assert_eq!(allocator_chunk_bytes(20).unwrap(), 1 << 20);
        assert_eq!(allocator_chunk_bytes(21).unwrap(), 1 << 21);
        assert_eq!(
            allocator_chunk_bytes(8).unwrap_err(),
            CudaError::ErrorInvalidValue
        );
        assert_eq!(
            allocator_chunk_bytes(22).unwrap_err(),
            CudaError::ErrorInvalidValue
        );
        assert_eq!(
            allocator_chunk_bytes(usize::BITS).unwrap_err(),
            CudaError::ErrorInvalidValue
        );
    }
}

// Scratch accessors are copied into CUDA host callbacks and asynchronous transfers on the
// execution stream. Keep their owning boxes on the prover thread until that stream is idle.
struct HostPool {
    allocator: HostAllocator,
    stream: CudaStream,
    retired: RefCell<Vec<Box<dyn Any>>>,
}

impl HostPool {
    fn prepare(&self, requested: usize) -> CudaResult<()> {
        let mut retired = self.retired.borrow_mut();
        if retired.is_empty() {
            return Ok(());
        }
        if self.stream.query()? {
            retired.clear();
        } else if self.allocator.get_largest_free_range() < requested {
            // Let independent kernels and callbacks overlap until another allocation actually
            // needs retired storage. Reclaiming earlier would let CPU writes race queued DMA.
            self.stream.synchronize()?;
            retired.clear();
        }
        Ok(())
    }
}

impl Drop for HostPool {
    fn drop(&mut self) {
        let retired = self.retired.get_mut();
        if !retired.is_empty() {
            if let Err(error) = self.stream.synchronize() {
                // The CUDA context cannot establish completion. Preserve the backing storage
                // for any in-flight callback until process teardown.
                error!("failed to drain pinned host scratch on shutdown: {error:?}");
                std::mem::forget(std::mem::take(retired));
            }
        }
    }
}

pub(crate) struct HostAllocation<T: ?Sized + 'static> {
    data: Option<Box<T, HostAllocator>>,
    pool: Rc<HostPool>,
}

impl<T: ?Sized + 'static> HostAllocation<T> {
    fn new(data: Box<T, HostAllocator>, pool: Rc<HostPool>) -> Self {
        Self {
            data: Some(data),
            pool,
        }
    }

    pub fn get_accessor(&self) -> UnsafeAccessor<T> {
        UnsafeAccessor::new(self.data.as_deref().unwrap())
    }

    pub fn get_mut_accessor(&mut self) -> UnsafeMutAccessor<T> {
        UnsafeMutAccessor::new(self.data.as_deref_mut().unwrap())
    }
}

impl<T: ?Sized + 'static> Drop for HostAllocation<T> {
    fn drop(&mut self) {
        let data = self.data.take().unwrap();
        self.pool.retired.borrow_mut().push(Box::new(data));
    }
}

impl<T: 'static> CudaSlice<T> for HostAllocation<[T]> {
    unsafe fn as_slice(&self) -> &[T] {
        self.data.as_deref().unwrap()
    }
}

impl<T: 'static> CudaSliceMut<T> for HostAllocation<[T]> {
    unsafe fn as_mut_slice(&mut self) -> &mut [T] {
        self.data.as_deref_mut().unwrap()
    }
}
