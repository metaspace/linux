// SPDX-License-Identifier: GPL-2.0

use crate::{
    alloc::mempool::{MemPool, MemPoolBox},
    bindings,
    block::error::{BlkError, BlkResult},
    device::Device,
    prelude::*,
    sync::Arc,
    types::{ARef, Opaque},
};

use super::{Operations, Request};

/// Type of the [`MemPool`] used to allocate storage for dma vector descriptors in [`DmaMapIter`].
pub type DmaMapMempool<const N: usize> = Arc<MemPool<[DmaVector; N]>>;

/// A descriptor of a memory segment that is mapped for DMA.
#[derive(Zeroable, Clone, Copy)]
pub struct DmaVector {
    address: u64,
    length: u32,
}

/// An iterator that DMA maps segments of a block request.
pub struct DmaMapIter<const N: usize, T: Operations> {
    iter: Opaque<bindings::blk_dma_iter>,
    inner: DmaMapIterInner<N, T>,
}

impl<const N: usize, T: Operations> DmaMapIter<N, T> {
    pub(crate) fn new(
        rq: ARef<Request<T>>,
        device: &Device,
        mempool: DmaMapMempool<N>,
    ) -> BlkResult<Self> {
        let mut this = Self {
            iter: Opaque::zeroed(),
            inner: DmaMapIterInner::new(rq, device, mempool)?,
        };

        let ok = unsafe {
            bindings::blk_rq_dma_map_iter_start(
                this.inner.request.as_raw(),
                device.as_raw(),
                this.inner.state.get(),
                this.iter.get(),
            )
        };

        if ok {
            this.add_vector()?;
            Ok(this)
        } else {
            Err(BlkError::from_blk_status(unsafe {
                (*this.iter.get()).status
            }))
        }
    }

    fn add_vector(&mut self) -> Result {
        self.inner.add_vector(self.address(), self.length())
    }

    /// Advance the iterator to the next DMA mapped segment.
    pub fn next(&mut self) -> Result {
        let ok = unsafe {
            bindings::blk_rq_dma_map_iter_next(
                self.inner.request.as_raw(),
                self.inner.device.as_raw(),
                self.inner.state.get(),
                self.iter.get(),
            )
        };

        if ok {
            self.add_vector()?;
            Ok(())
        } else {
            Err(kernel::error::code::EINVAL)
        }
    }

    /// Return the DMA address of the current segment.
    pub fn address(&self) -> u64 {
        unsafe { (*self.iter.get()).addr }
    }

    /// Return the length of the current segment.
    pub fn length(&self) -> u32 {
        unsafe { (*self.iter.get()).len }
    }

    /// Consume the iterator and return the completed mapping result.
    pub fn finish(self) -> DmaMapIterMapped<N, T> {
        let Self { iter: _, inner } = self;
        DmaMapIterMapped { _inner: inner }
    }
}

/// The result of a completed DMA mapping iteration.
pub struct DmaMapIterInner<const N: usize, T: Operations> {
    state: Opaque<bindings::dma_iova_state>,
    request: ARef<Request<T>>,
    device: ARef<Device>,
    dma_vectors: MemPoolBox<[DmaVector; N]>,
    dma_vector_count: usize,
}

impl<const N: usize, T: Operations> DmaMapIterInner<N, T> {
    fn new(rq: ARef<Request<T>>, device: &Device, mempool: DmaMapMempool<N>) -> Result<Self> {
        Ok(Self {
            state: Opaque::zeroed(),
            request: rq,
            device: device.into(),
            dma_vectors: mempool.alloc_zeroed(GFP_ATOMIC)?,
            dma_vector_count: 0,
        })
    }

    fn total_length(&self) -> usize {
        self.dma_vectors.iter().fold(0usize, |acc, vector| {
            let length: usize = vector
                .length
                .try_into()
                .expect("expected u32 to fit in usize");
            acc + length
        })
    }

    fn add_vector(&mut self, address: u64, length: u32) -> Result {
        *self
            .dma_vectors
            .get_mut(self.dma_vector_count)
            .ok_or(ENOMEM)? = DmaVector { address, length };
        self.dma_vector_count += 1;

        Ok(())
    }
}

impl<const N: usize, T: Operations> Drop for DmaMapIterInner<N, T> {
    fn drop(&mut self) {
        // TODO: map type via flags
        let flags = 0;
        // In some cases the following call can unmap the mapping for us. If not, we use our own
        // recording of the vectos to unmap.
        if !unsafe {
            bindings::blk_rq_dma_unmap(
                self.request.as_raw(),
                self.device.as_raw(),
                self.state.get(),
                self.total_length(),
                flags,
            )
        } {
            for mapping in &self.dma_vectors[0..self.dma_vector_count] {
                unsafe {
                    bindings::dma_unmap_phys(
                        self.device.as_raw(),
                        mapping.address,
                        mapping.length as usize,
                        self.request.dma_direction().into(),
                        0,
                    )
                };
            }
        }
    }
}

// SAFETY: `DmaMapIterInner` can be dropped from any thread.
unsafe impl<const N: usize, T: Operations> Send for DmaMapIterInner<N, T> {}

// SAFETY: `DmaMapIterInner` is shareable across threads.
unsafe impl<const N: usize, T: Operations> Sync for DmaMapIterInner<N, T> {}

/// A set of mapped pages produced by [`DmsMapIter`].
pub struct DmaMapIterMapped<const N: usize, T: Operations> {
    _inner: DmaMapIterInner<N, T>,
}

impl<const N: usize, T: Operations> Unpin for DmaMapIterMapped<N, T> {}
