use fft::GoodAllocator;
use std::sync::Arc;

pub(crate) struct PendingTraceChunk<T: Copy, A: GoodAllocator> {
    allocation: Arc<Vec<T, A>>,
    base: *mut T,
    reserved: usize,
}

// SAFETY: Moving the owner preserves its allocation. Only its producer reserves
// slots, and replay workers only write to their disjoint reserved ranges.
unsafe impl<T: Copy + Send + Sync, A: GoodAllocator> Send for PendingTraceChunk<T, A> {}

impl<T: Copy, A: GoodAllocator> PendingTraceChunk<T, A> {
    pub(crate) fn new(capacity: usize, allocator: A) -> Self {
        let mut allocation = Vec::with_capacity_in(capacity, allocator);
        let base = allocation.as_mut_ptr();
        Self {
            allocation: Arc::new(allocation),
            base,
            reserved: 0,
        }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.allocation.capacity() - self.reserved
    }

    pub(crate) fn reserved(&self) -> usize {
        self.reserved
    }

    pub(crate) fn reserve(&mut self, count: usize) -> (*mut T, *mut T, Arc<Vec<T, A>>) {
        assert!(count <= self.remaining());
        // SAFETY: The Vec never reallocates and its length stays zero while
        // replay workers hold raw pointers. Advance the cursor before handing
        // the range out so subsequent reservations are disjoint.
        unsafe {
            let start = self.base.add(self.reserved);
            self.reserved += count;
            let end = self.base.add(self.reserved);
            (start, end, self.allocation.clone())
        }
    }

    /// # Safety
    /// Every reserved slot must have been written and all replay accesses must have ended.
    pub(crate) unsafe fn assume_initialized(mut self) -> Arc<Vec<T, A>> {
        let allocation =
            Arc::get_mut(&mut self.allocation).expect("replay still owns trace storage");
        // SAFETY: The caller established completion, and reserve bounds the length.
        unsafe { allocation.set_len(self.reserved) };
        self.allocation
    }

    pub(crate) fn into_allocator(self) -> A {
        Arc::into_inner(self.allocation)
            .expect("replay still owns trace storage")
            .allocator()
            .clone()
    }
}

#[derive(Clone)]
pub(crate) struct ChunkedTraceHolder<T, A: GoodAllocator> {
    pub chunks: Vec<Arc<Vec<T, A>>>,
}

impl<T, A: GoodAllocator> ChunkedTraceHolder<T, A> {
    pub fn len(&self) -> usize {
        self.chunks.iter().map(|chunk| chunk.len()).sum()
    }

    pub fn get(&self, index: usize) -> T
    where
        T: Copy,
    {
        let mut current_index = index;
        for chunk in self.chunks.iter() {
            if current_index < chunk.len() {
                return chunk[current_index];
            } else {
                current_index -= chunk.len();
            }
        }
        panic!("Index out of bounds");
    }

    pub fn into_allocators(self) -> Vec<A> {
        self.chunks
            .into_iter()
            .map(|c| Arc::into_inner(c).unwrap().allocator().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::PendingTraceChunk;
    use std::alloc::Global;
    use std::num::NonZeroU32;

    #[test]
    fn replay_ranges_publish_in_reservation_order_after_completion() {
        let mut chunk = PendingTraceChunk::<NonZeroU32, _>::new(3, Global);
        let (first, first_end, first_owner) = chunk.reserve(2);
        let (second, second_end, second_owner) = chunk.reserve(1);
        assert_eq!(first_end, second);
        assert!(first_owner.is_empty() && second_owner.is_empty());
        assert_eq!(chunk.remaining(), 0);

        // SAFETY: Each write falls within its exclusive reserved range. Replay
        // may complete snapshots out of order.
        unsafe {
            assert_eq!(second.add(1), second_end);
            second.write(NonZeroU32::new(3).unwrap());
            first.write(NonZeroU32::new(1).unwrap());
            first.add(1).write(NonZeroU32::new(2).unwrap());
        }
        drop((first_owner, second_owner));
        // SAFETY: All three reserved slots are initialized and no writer remains.
        let initialized = unsafe { chunk.assume_initialized() };
        assert_eq!(
            initialized
                .iter()
                .map(|value| value.get())
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn interrupted_replay_recycles_storage_with_unwritten_slots() {
        let mut chunk = PendingTraceChunk::<NonZeroU32, _>::new(3, Global);
        let (first, _, owner) = chunk.reserve(3);
        // SAFETY: Only one reserved slot is written before replay is interrupted.
        unsafe { first.write(NonZeroU32::new(1).unwrap()) };
        assert!(owner.is_empty());
        assert_eq!(chunk.reserved(), 3);
        drop(owner);
        let Global = chunk.into_allocator();
    }
}
