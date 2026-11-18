//! The JIT reads and writes this holder on the CPU. CUDA receives each trace and
//! its sparse teardown records through independently pinned staging allocations.

use era_cudart::memory::CudaHostRegisterFlags;
use era_cudart::result::CudaResultWrap;
use era_cudart_sys::{cudaHostRegister, cudaHostUnregister};
use riscv_transpiler::jit::{JitRunnerRam, MemoryHolder};
use std::ops::{Deref, DerefMut};

pub(crate) enum SimulatorMemoryHolder {
    Pinned(Box<MemoryHolder>),
    #[cfg(target_os = "linux")]
    DemandPaged(Box<MemoryHolder, AnonymousPages>),
}

impl SimulatorMemoryHolder {
    pub(crate) fn new(ram: JitRunnerRam) -> Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("AIRBENDER_SIMULATOR_MEMORY").as_deref()
            == Some(std::ffi::OsStr::new("demand-paged"))
        {
            return Self::DemandPaged(MemoryHolder::allocate_zeroed(ram, AnonymousPages));
        }
        let mut holder = MemoryHolder::allocate_zeroed(ram, Default::default());
        // SAFETY: the Box owns the complete aligned allocation until its matching
        // unregister in Drop. No registration is attempted for anonymous pages.
        unsafe {
            cudaHostRegister(
                holder.as_mut_ptr().as_ptr().cast(),
                holder.byte_size(),
                CudaHostRegisterFlags::DEFAULT.bits(),
            )
            .wrap()
            .unwrap();
        }
        Self::Pinned(holder)
    }

    pub(crate) fn pinned_bytes(&self) -> usize {
        match self {
            Self::Pinned(holder) => holder.byte_size(),
            #[cfg(target_os = "linux")]
            Self::DemandPaged(_) => 0,
        }
    }

    pub(crate) fn reset_after_abort(&mut self) {
        match self {
            Self::Pinned(holder) => MemoryHolder::reset(holder),
            #[cfg(target_os = "linux")]
            Self::DemandPaged(holder) => {
                // SAFETY: this exclusive reference owns an entire private anonymous
                // mapping. DONTNEED restores zero contents, including any addresses
                // a cancelled JIT run never emitted to the teardown collector.
                let result = unsafe {
                    libc::madvise(
                        holder.as_mut_ptr().as_ptr().cast(),
                        holder.byte_size(),
                        libc::MADV_DONTNEED,
                    )
                };
                if result != 0 {
                    MemoryHolder::reset(holder);
                }
            }
        }
    }
}

impl Deref for SimulatorMemoryHolder {
    type Target = MemoryHolder;

    fn deref(&self) -> &MemoryHolder {
        match self {
            Self::Pinned(holder) => holder,
            #[cfg(target_os = "linux")]
            Self::DemandPaged(holder) => holder,
        }
    }
}

impl DerefMut for SimulatorMemoryHolder {
    fn deref_mut(&mut self) -> &mut MemoryHolder {
        match self {
            Self::Pinned(holder) => holder,
            #[cfg(target_os = "linux")]
            Self::DemandPaged(holder) => holder,
        }
    }
}

impl Drop for SimulatorMemoryHolder {
    fn drop(&mut self) {
        match self {
            Self::Pinned(holder) => {
                // SAFETY: only the successful registration in new constructs this
                // variant. Unregister before the underlying Box frees the storage.
                unsafe {
                    cudaHostUnregister(holder.as_mut_ptr().as_ptr().cast())
                        .wrap()
                        .unwrap();
                }
            }
            #[cfg(target_os = "linux")]
            Self::DemandPaged(_) => {}
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub(crate) struct AnonymousPages;

#[cfg(target_os = "linux")]
// SAFETY: this allocator accepts whole 2 MiB-aligned simulator layouts only.
// Each allocation owns a private mapping of exactly the requested size.
unsafe impl std::alloc::Allocator for AnonymousPages {
    fn allocate(
        &self,
        layout: std::alloc::Layout,
    ) -> Result<std::ptr::NonNull<[u8]>, std::alloc::AllocError> {
        use std::alloc::AllocError;
        use std::ptr::NonNull;

        let align = MemoryHolder::ALIGNMENT;
        if layout.align() != align || layout.size() == 0 || !layout.size().is_multiple_of(align) {
            return Err(AllocError);
        }
        let bytes = layout.size().checked_add(align).ok_or(AllocError)?;
        // SAFETY: anonymous mmap returns page-aligned, zero-initialized storage.
        // Trimming disjoint whole pages leaves one aligned owned mapping. Normal
        // VM commit accounting still applies; demand paging does not need NORESERVE.
        unsafe {
            let base = libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if base == libc::MAP_FAILED {
                return Err(AllocError);
            }
            let base = base.cast::<u8>();
            let prefix = base.align_offset(align);
            let ptr = base.add(prefix);
            let suffix = align - prefix;
            if prefix != 0 {
                assert_eq!(libc::munmap(base.cast(), prefix), 0);
            }
            if suffix != 0 {
                assert_eq!(libc::munmap(ptr.add(layout.size()).cast(), suffix), 0);
            }
            Ok(NonNull::slice_from_raw_parts(
                NonNull::new(ptr).expect("non-null simulator mapping"),
                layout.size(),
            ))
        }
    }

    fn allocate_zeroed(
        &self,
        layout: std::alloc::Layout,
    ) -> Result<std::ptr::NonNull<[u8]>, std::alloc::AllocError> {
        self.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: std::ptr::NonNull<u8>, layout: std::alloc::Layout) {
        // SAFETY: Allocator callers return the original pointer and layout.
        assert_eq!(
            unsafe { libc::munmap(ptr.as_ptr().cast(), layout.size()) },
            0
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::execution::cpu_worker::collect_inits_and_teardowns;

    #[test]
    fn demand_paged_holder_survives_collection_and_cancellation() {
        let ram = JitRunnerRam::Medium;
        let mut holder =
            SimulatorMemoryHolder::DemandPaged(MemoryHolder::allocate_zeroed(ram, AnonymousPages));
        assert_eq!(
            holder.as_mut_ptr().as_ptr().addr() % MemoryHolder::ALIGNMENT,
            0
        );
        assert_eq!(holder.ram_size(), ram.ram_size());
        assert_eq!(holder.pinned_bytes(), 0);
        let index = holder.memory().len() - 1;
        let accessed = [1 << 20, index];
        let worker = worker::Worker::new_with_num_threads(2);
        for _ in 0..2 {
            for (n, idx) in accessed.into_iter().enumerate() {
                assert_eq!((holder.memory()[idx], holder.timestamps()[idx]), (0, 0));
                holder.memory_mut()[idx] = 9 + n as u32;
                holder.timestamps_mut()[idx] = 7 + n as u64;
            }
            let entries: Vec<_> = collect_inits_and_teardowns(&mut holder, &worker)
                .into_iter()
                .flatten()
                .collect();
            assert_eq!(entries.len(), accessed.len());
            for (n, idx) in accessed.into_iter().enumerate() {
                assert_eq!(entries[n].address, (idx * 4) as u32);
                assert_eq!(entries[n].teardown_value, 9 + n as u32);
                assert_eq!(entries[n].teardown_timestamp.as_scalar(), 7 + n as u64);
                holder.memory_mut()[idx] = 99;
                holder.timestamps_mut()[idx] = 100;
            }
            holder.reset_after_abort();
            assert!(collect_inits_and_teardowns(&mut holder, &worker)
                .into_iter()
                .all(|rows| rows.is_empty()));
        }
        assert_eq!((holder.memory()[index], holder.timestamps()[index]), (0, 0));
    }
}
