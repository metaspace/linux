// SPDX-License-Identifier: GPL-2.0

//! Memory pool backed by kmalloc.
//!
//! This module provides a Rust abstraction for the C `mempool` API. Memory pools are used for
//! guaranteed, deadlock-free memory allocations during extreme VM load. A pool pre-allocates a
//! minimum number of elements at creation time. When the regular allocator cannot satisfy a
//! request, the pool falls back to its pre-allocated reserve, ensuring that allocations with
//! sleeping-capable flags (such as [`GFP_KERNEL`]) will always succeed.
//!
//! C header: [`include/linux/mempool.h`](srctree/include/linux/mempool.h).

use core::{
    marker::PhantomData,
    ops::{
        Deref,
        DerefMut, //
    },
    ptr::{
        drop_in_place,
        NonNull, //
    },
};

use kernel::{
    prelude::*,
    sync::Arc,
    types::Opaque, //
};
use pin_init::Zeroable;

/// A memory pool for guaranteed allocations.
///
/// [`MemPool`] wraps the C `mempool` API, backed by `kmalloc`. It pre-allocates a minimum number
/// of elements so that allocations from the pool can succeed even when the system is under extreme
/// memory pressure. This is particularly useful in I/O paths where allocation failure is not
/// acceptable.
///
/// The pool is reference-counted internally, so cloning a [`MemPool`] yields a handle to the same
/// underlying pool. Allocated [`MemPoolBox`] elements hold a reference to the pool, so the
/// backing pool remains alive as long as any allocation is outstanding.
///
/// # Invariants
///
/// - `inner` holds a valid `mempool` C struct.
#[pin_data]
pub struct MemPool<T> {
    #[pin]
    inner: Opaque<bindings::mempool>,
    _p: PhantomData<T>,
}

impl<T> MemPool<T> {
    /// Create a new memory pool with `min_elements` pre-allocated elements.
    ///
    /// The pool guarantees that at least `min_elements` allocations can succeed without
    /// accessing the global allocator. When `min_elements` is zero, the pool still
    /// pre-allocates a single element internally.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::alloc::mempool::MemPool;
    ///
    /// let pool = MemPool::<u64>::new(4)?;
    /// # Ok::<(), Error>(())
    /// ```
    pub fn new(min_elements: usize) -> Result<Arc<Self>> {
        Ok(Arc::pin_init(
            try_pin_init!(
            // INVARIANT: We only return `Ok` if initialization of the mempool succeeds.
            Self {
                inner <- Opaque::try_ffi_init(|place| {
                    // SAFETY: `ffi_init` promises that place is valid for writes.
                    unsafe { core::ptr::write(place, bindings::mempool::zeroed()) };
                    let size: usize = size_of::<T>();
                    let size: *const c_void = core::ptr::without_provenance(size);
                    kernel::error::to_result(
                        // SAFETY: `place` points to a valid allocation. `size` is a valid pointer.
                        unsafe {
                            bindings::mempool_init_noprof(
                                place,
                                min_elements.try_into()?,
                                Some(bindings::mempool_kmalloc),
                                Some(bindings::mempool_kfree),
                                size.cast_mut(),
                            )
                        },
                    )
                }),
                _p: PhantomData,
            }),
            GFP_KERNEL,
        )?)
    }

    /// Allocate an element from the pool and initialize it with `init`.
    ///
    /// The element is first allocated from the underlying allocator using `flags`. If that
    /// fails and the pool has pre-allocated reserve elements, one of those is used instead.
    /// When `flags` includes `__GFP_DIRECT_RECLAIM` (e.g. [`GFP_KERNEL`]), this function is
    /// guaranteed to succeed. With non-sleeping flags such as [`GFP_ATOMIC`], the allocation
    /// may fail if both the allocator and the reserve are exhausted.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::alloc::mempool::MemPool;
    ///
    /// struct Cmd {
    ///     opcode: u32,
    ///     length: u64,
    /// }
    ///
    /// let pool = MemPool::<Cmd>::new(4)?;
    /// let cmd = pool.alloc(init!(Cmd { opcode: 1, length: 512 }), GFP_ATOMIC)?;
    ///
    /// assert_eq!(cmd.opcode, 1);
    /// assert_eq!(cmd.length, 512);
    /// # Ok::<(), Error>(())
    /// ```
    pub fn alloc(
        self: &Arc<Self>,
        init: impl Init<T>,
        flags: kernel::alloc::Flags,
    ) -> Result<MemPoolBox<T>> {
        // SAFETY: By type invariant `self.inner` is a valid mempool.
        let ptr = unsafe { bindings::mempool_alloc_noprof(self.inner.get(), flags.as_raw()) };
        let ptr = NonNull::new(ptr.cast()).ok_or(ENOMEM)?;
        // SAFETY: By C API contract, if `ptr` is not null, it points to a live allocation that we
        // own.
        unsafe { init.__init(ptr.as_ptr()) }?;
        Ok(MemPoolBox {
            data: ptr,
            allocator: self.clone(),
        })
    }
}

impl<T> MemPool<T>
where
    T: Zeroable,
{
    /// Allocate a zeroed element from the pool.
    ///
    /// This is a convenience wrapper around [`MemPool::alloc`] that zero-initializes the
    /// allocated element. See [`MemPool::alloc`] for details on allocation semantics.
    ///
    /// # Examples
    ///
    /// ```
    /// use kernel::alloc::mempool::MemPool;
    ///
    /// let pool = MemPool::<u64>::new(4)?;
    /// let item = pool.alloc_zeroed(GFP_ATOMIC)?;
    ///
    /// assert_eq!(*item, 0);
    /// # Ok::<(), Error>(())
    /// ```
    pub fn alloc_zeroed(self: &Arc<Self>, flags: kernel::alloc::Flags) -> Result<MemPoolBox<T>> {
        // SAFETY: By type invariant `self.inner` is a valid mempool.
        let ptr = unsafe { bindings::mempool_alloc_noprof(self.inner.get(), flags.as_raw()) };
        let ptr = NonNull::new(ptr.cast()).ok_or(ENOMEM)?;
        // SAFETY: By C API contract, if `ptr` is not null, it points to a live allocation that we
        // own.
        let initializer = T::init_zeroed();
        unsafe { initializer.__init(ptr.as_ptr()) }?;
        Ok(MemPoolBox {
            data: ptr,
            allocator: self.clone(),
        })
    }
}

/// An owned allocation from a [`MemPool`].
///
/// [`MemPoolBox`] dereferences to `T` and returns the element to the pool when dropped. It holds
/// a reference to the originating pool, so the pool is kept alive for as long as any
/// [`MemPoolBox`] exists.
///
/// # Invariants
///
/// - `data` points to a valid `T`, owned by `self`.
pub struct MemPoolBox<T> {
    data: NonNull<T>,
    allocator: Arc<MemPool<T>>,
}

impl<T> Drop for MemPoolBox<T> {
    fn drop(&mut self) {
        // SAFETY:
        //  - By type invariant, `data` is valid for reads and writes, is properly aligned and is
        //    non-null.
        //  - `self.data` is valid for dropping, as we own the value.
        //  - As we have exclusive access to `self.data.` no other accesses to `self.data` can
        //    happen.
        unsafe { drop_in_place(self.data.as_ptr()) };
        // SAFETY: `self.data` is no longer initialized, as we dropped it above.
        unsafe {
            bindings::mempool_free(self.data.as_ptr().cast(), self.allocator.inner.get().cast())
        };
    }
}

impl<T> Deref for MemPoolBox<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: By type invariant and existence of `&self`, `self.data` is convertible to a
        // reference.
        unsafe { self.data.as_ref() }
    }
}

impl<T> DerefMut for MemPoolBox<T> {
    // SAFETY: By type invariant and existence of `&mut self`, `self.data` is convertible to a
    // mutable reference.
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.data.as_mut() }
    }
}

#[macros::kunit_tests(rust_mempool)]
mod tests {
    use super::*;
    use kernel::alloc::flags::GFP_ATOMIC;

    #[test]
    fn test_mempool_create() -> Result {
        let _pool = MemPool::<u64>::new(4)?;
        Ok(())
    }

    #[test]
    fn test_mempool_alloc() -> Result {
        let pool = MemPool::<u64>::new(4)?;
        let _item = pool.alloc_zeroed(GFP_ATOMIC)?;
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_is_zeroed() -> Result {
        let pool = MemPool::<u64>::new(4)?;
        let item = pool.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*item, 0u64);
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_zeroed_array() -> Result {
        let pool = MemPool::<[u8; 64]>::new(4)?;
        let item = pool.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*item, [0u8; 64]);
        Ok(())
    }

    #[test]
    fn test_mempool_multiple_alloc() -> Result {
        let pool = MemPool::<u32>::new(8)?;
        let a = pool.alloc_zeroed(GFP_ATOMIC)?;
        let b = pool.alloc_zeroed(GFP_ATOMIC)?;
        let c = pool.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*a, 0u32);
        assert_eq!(*b, 0u32);
        assert_eq!(*c, 0u32);
        Ok(())
    }

    #[test]
    fn test_mempool_deref() -> Result {
        let pool = MemPool::<[u32; 4]>::new(2)?;
        let item = pool.alloc_zeroed(GFP_ATOMIC)?;
        let val: &[u32; 4] = &item;
        assert_eq!(val[0], 0);
        assert_eq!(val[3], 0);
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_drop_realloc() -> Result {
        let pool = MemPool::<u64>::new(2)?;
        let item = pool.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*item, 0u64);
        drop(item);
        let item2 = pool.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*item2, 0u64);
        Ok(())
    }

    #[test]
    fn test_mempool_clone_shares_pool() -> Result {
        let pool = MemPool::<u64>::new(4)?;
        let pool2 = pool.clone();
        let a = pool.alloc_zeroed(GFP_ATOMIC)?;
        let b = pool2.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*a, 0u64);
        assert_eq!(*b, 0u64);
        Ok(())
    }

    #[test]
    fn test_mempool_outlives_pool() -> Result {
        let item;
        {
            let pool = MemPool::<u64>::new(2)?;
            item = pool.alloc_zeroed(GFP_ATOMIC)?;
        }
        assert_eq!(*item, 0u64);
        Ok(())
    }

    #[test]
    fn test_mempool_min_elements_zero() -> Result {
        let pool = MemPool::<u8>::new(0)?;
        let item = pool.alloc_zeroed(GFP_ATOMIC)?;
        assert_eq!(*item, 0u8);
        Ok(())
    }

    struct Pair {
        key: u32,
        value: u64,
    }

    #[test]
    fn test_mempool_alloc_with_init() -> Result {
        let pool = MemPool::<Pair>::new(4)?;
        let item = pool.alloc(init!(Pair { key: 7, value: 42 }), GFP_ATOMIC)?;
        assert_eq!(item.key, 7);
        assert_eq!(item.value, 42);
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_multiple_with_init() -> Result {
        let pool = MemPool::<Pair>::new(4)?;
        let a = pool.alloc(init!(Pair { key: 1, value: 10 }), GFP_ATOMIC)?;
        let b = pool.alloc(init!(Pair { key: 2, value: 20 }), GFP_ATOMIC)?;
        assert_eq!(a.key, 1);
        assert_eq!(a.value, 10);
        assert_eq!(b.key, 2);
        assert_eq!(b.value, 20);
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_init_deref_mut() -> Result {
        let pool = MemPool::<Pair>::new(2)?;
        let mut item = pool.alloc(init!(Pair { key: 0, value: 0 }), GFP_ATOMIC)?;
        item.key = 99;
        item.value = 1000;
        assert_eq!(item.key, 99);
        assert_eq!(item.value, 1000);
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_init_drop_realloc() -> Result {
        let pool = MemPool::<Pair>::new(2)?;
        let item = pool.alloc(init!(Pair { key: 1, value: 2 }), GFP_ATOMIC)?;
        assert_eq!(item.key, 1);
        drop(item);
        let item2 = pool.alloc(init!(Pair { key: 3, value: 4 }), GFP_ATOMIC)?;
        assert_eq!(item2.key, 3);
        assert_eq!(item2.value, 4);
        Ok(())
    }

    #[test]
    fn test_mempool_alloc_init_beyond_min() -> Result {
        let pool = MemPool::<Pair>::new(2)?;
        let a = pool.alloc(init!(Pair { key: 1, value: 1 }), GFP_ATOMIC)?;
        let b = pool.alloc(init!(Pair { key: 2, value: 2 }), GFP_ATOMIC)?;
        let c = pool.alloc(init!(Pair { key: 3, value: 3 }), GFP_ATOMIC)?;
        assert_eq!(a.key, 1);
        assert_eq!(b.key, 2);
        assert_eq!(c.key, 3);
        Ok(())
    }
}
