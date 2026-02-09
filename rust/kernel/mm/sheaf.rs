// SPDX-License-Identifier: GPL-2.0

//! Slub allocator sheaf abstraction.
//!
//! Sheaves are percpu array-based caching layers for the slub allocator.
//! They provide a mechanism for pre-allocating objects that can later
//! be retrieved without risking allocation failure, making them useful in
//! contexts where memory allocation must be guaranteed to succeed.
//!
//! The term "sheaf" is the english word for a bundle of straw. In this context
//! it means a bundle of pre-allocated objects. A per-NUMA-node cache of sheaves
//! is called a "barn". Because you store your sheafs in barns.
//!
//! # Use cases
//!
//! Sheaves are particularly useful when:
//!
//! - Allocations must be guaranteed to succeed in a restricted context (e.g.,
//!   while holding locks or in atomic context).
//! - Multiple allocations need to be performed as a batch operation.
//! - Fast-path allocation performance is critical, as sheaf allocations avoid
//!   atomic operations by using local locks with preemption disabled.
//!
//! # Architecture
//!
//! The sheaf system consists of three main components:
//!
//! - [`KMemCache`]: A slab cache configured with sheaf support.
//! - [`Sheaf`]: A pre-filled container of objects from a specific cache.
//! - [`SBox`]: An owned allocation from a sheaf, similar to a `Box`.
//!
//! # Example
//!
//! ```
//! use kernel::c_str;
//! use kernel::mm::sheaf::{KMemCache, KMemCacheInit, Sheaf, SBox};
//! use kernel::prelude::*;
//!
//! struct MyObject {
//!     value: u32,
//! }
//!
//! impl KMemCacheInit<MyObject> for MyObject {
//!     fn init() -> impl Init<MyObject> {
//!         init!(MyObject { value: 0 })
//!     }
//! }
//!
//! // Create a cache with sheaf capacity of 16 objects.
//! let cache = KMemCache::<MyObject>::new(c_str!("my_cache"), 16)?;
//!
//! // Pre-fill a sheaf with 8 objects.
//! let mut sheaf = cache.as_arc_borrow().sheaf(8, GFP_KERNEL)?;
//!
//! // Allocations from the sheaf are guaranteed to succeed until empty.
//! let obj = sheaf.alloc().unwrap();
//!
//! // Return the sheaf when done, attempting to refill it.
//! sheaf.return_refill(GFP_KERNEL);
//! # Ok::<(), Error>(())
//! ```
//!
//! # Constraints
//!
//! - Sheaves are slower when `CONFIG_SLUB_TINY` or `CONFIG_SLUB_DEBUG` is
//!   enabled due to cpu sheaves being disabled. All prefilled sheaves become
//!   "oversize" and go through a slower allocation path.
//! - The sheaf capacity is fixed at cache creation time.

use core::{
    convert::Infallible,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use kernel::prelude::*;

use crate::sync::{Arc, ArcBorrow};

/// A slab cache with sheaf support.
///
/// This type wraps a kernel `kmem_cache` configured with a sheaf capacity,
/// enabling pre-allocation of objects via [`Sheaf`].
///
/// For now, this type only exists for sheaf management.
///
/// # Type parameter
///
/// - `T`: The type of objects managed by this cache. Must implement
///   [`KMemCacheInit`] to provide initialization logic for new allocations.
///
/// # Invariants
///
/// - `cache` is a valid pointer to a `kmem_cache` created with
///   `__kmem_cache_create_args`.
/// - The cache is valid for the lifetime of this struct.
pub struct KMemCache<T: KMemCacheInit<T>> {
    cache: NonNull<bindings::kmem_cache>,
    _p: PhantomData<T>,
}

impl<T: KMemCacheInit<T>> KMemCache<T> {
    /// Creates a new slab cache with sheaf support.
    ///
    /// Creates a kernel slab cache for objects of type `T` with the specified
    /// sheaf capacity. The cache uses the provided `name` for identification
    /// in `/sys/kernel/slab/` and debugging output.
    ///
    /// # Arguments
    ///
    /// - `name`: A string identifying the cache. This name appears in sysfs and
    ///   debugging output.
    /// - `sheaf_capacity`: The maximum number of objects a sheaf from this
    ///   cache can hold. A capacity of zero disables sheaf support.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// - The cache could not be created due to memory pressure.
    /// - The size of `T` cannot be represented as a `c_uint`.
    pub fn new(name: &CStr, sheaf_capacity: u32) -> Result<Arc<Self>>
    where
        T: KMemCacheInit<T>,
    {
        let flags = 0;
        let mut args: bindings::kmem_cache_args = pin_init::zeroed();
        args.sheaf_capacity = sheaf_capacity;

        // NOTE: We are not initializing at object allocation time, because
        // there is no matching teardown function on the C side machinery.
        args.ctor = None;

        // SAFETY: `name` is a valid C string, `args` is properly initialized,
        // and the size of `T` has been validated to fit in a `c_uint`.
        let ptr = unsafe {
            bindings::__kmem_cache_create_args(
                name.as_ptr().cast::<u8>(),
                core::mem::size_of::<T>().try_into()?,
                &mut args,
                flags,
            )
        };

        // INVARIANT: `ptr` was returned by `__kmem_cache_create_args` and is
        // non-null (checked below). The cache is valid until
        // `kmem_cache_destroy` is called in `Drop`.
        Ok(Arc::new(
            Self {
                cache: NonNull::new(ptr).ok_or(ENOMEM)?,
                _p: PhantomData,
            },
            GFP_KERNEL,
        )?)
    }

    /// Creates a pre-filled sheaf from this cache.
    ///
    /// Allocates a sheaf and pre-fills it with `size` objects. Once created,
    /// allocations from the sheaf via [`Sheaf::alloc`] are guaranteed to
    /// succeed until the sheaf is depleted.
    ///
    /// # Arguments
    ///
    /// - `size`: The number of objects to pre-allocate. Must not exceed the
    ///   cache's `sheaf_capacity`.
    /// - `gfp`: Allocation flags controlling how memory is obtained. Use
    ///   [`GFP_KERNEL`] for normal allocations that may sleep, or
    ///   [`GFP_NOWAIT`] for non-blocking allocations.
    ///
    /// # Errors
    ///
    /// Returns [`ENOMEM`] if the sheaf or its objects could not be allocated.
    ///
    /// # Warnings
    ///
    /// The kernel will warn if `size` exceeds `sheaf_capacity`.
    pub fn sheaf(
        self: ArcBorrow<'_, Self>,
        size: usize,
        gfp: kernel::alloc::Flags,
    ) -> Result<Sheaf<T>> {
        // SAFETY: `self.as_raw()` returns a valid cache pointer, and `size`
        // has been validated to fit in a `c_uint`.
        let ptr = unsafe {
            bindings::kmem_cache_prefill_sheaf(self.as_raw(), gfp.as_raw(), size.try_into()?)
        };

        // INVARIANT: `ptr` was returned by `kmem_cache_prefill_sheaf` and is
        // non-null (checked below). `cache` is the cache from which this sheaf
        // was created. `dropped` is false since the sheaf has not been returned.
        Ok(Sheaf {
            sheaf: NonNull::new(ptr).ok_or(ENOMEM)?,
            cache: self.into(),
            dropped: false,
        })
    }

    fn as_raw(&self) -> *mut bindings::kmem_cache {
        self.cache.as_ptr()
    }
}

impl<T: KMemCacheInit<T>> Drop for KMemCache<T> {
    fn drop(&mut self) {
        // SAFETY: `self.as_raw()` returns a valid cache pointer that was
        // created by `__kmem_cache_create_args`. As all objects allocated from
        // this hold a reference on `self`, they must have been dropped for this
        // `drop` method to execute.
        unsafe { bindings::kmem_cache_destroy(self.as_raw()) };
    }
}

/// Trait for types that can be initialized in a slab cache.
///
/// This trait provides the initialization logic for objects allocated from a
/// [`KMemCache`]. When the slab allocator creates new objects, it invokes the
/// constructor to ensure objects are in a valid initial state.
///
/// # Implementation
///
/// Implementors must provide [`init`](KMemCacheInit::init), which returns
/// a in-place initializer for the type.
///
/// # Example
///
/// ```
/// use kernel::mm::sheaf::KMemCacheInit;
/// use kernel::prelude::*;
///
/// struct MyData {
///     counter: u32,
///     name: [u8; 16],
/// }
///
/// impl KMemCacheInit<MyData> for MyData {
///     fn init() -> impl Init<MyData> {
///         init!(MyData {
///             counter: 0,
///             name: [0; 16],
///         })
///     }
/// }
/// ```
pub trait KMemCacheInit<T> {
    /// Returns an initializer for creating new objects of type `T`.
    ///
    /// This method is called by the allocator's constructor to initialize newly
    /// allocated objects. The initializer should set all fields to their
    /// default or initial values.
    fn init() -> impl Init<T, Infallible>;
}

/// A pre-filled container of slab objects.
///
/// A sheaf holds a set of pre-allocated objects from a [`KMemCache`].
/// Allocations from a sheaf are guaranteed to succeed until the sheaf is
/// depleted, making sheaves useful in contexts where allocation failure is
/// not acceptable.
///
/// Sheaves provide faster allocation than direct allocation because they use
/// local locks with preemption disabled rather than atomic operations.
///
/// # Lifecycle
///
/// Sheaves are created via [`KMemCache::sheaf`] and should be returned to the
/// allocator when no longer needed via [`Sheaf::return_refill`]. If a sheaf is
/// simply dropped, it is returned with `GFP_NOWAIT` flags, which may result in
/// the sheaf being flushed and freed rather than being cached for reuse.
///
/// # Invariants
///
/// - `sheaf` is a valid pointer to a `slab_sheaf` obtained from
///   `kmem_cache_prefill_sheaf`.
/// - `cache` is the cache from which this sheaf was created.
/// - `dropped` tracks whether the sheaf has been explicitly returned.
pub struct Sheaf<T: KMemCacheInit<T>> {
    sheaf: NonNull<bindings::slab_sheaf>,
    cache: Arc<KMemCache<T>>,
    dropped: bool,
}

impl<T: KMemCacheInit<T>> Sheaf<T> {
    fn as_raw(&self) -> *mut bindings::slab_sheaf {
        self.sheaf.as_ptr()
    }

    /// Return the sheaf and try to refill using `flags`.
    ///
    /// If the sheaf cannot simply become the percpu spare sheaf, but there's
    /// space for a full sheaf in the barn, we try to refill the sheaf back to
    /// the cache's sheaf_capacity to avoid handling partially full sheaves.
    ///
    /// If the refill fails because gfp is e.g. GFP_NOWAIT, or the barn is full,
    /// the sheaf is instead flushed and freed.
    pub fn return_refill(mut self, flags: kernel::alloc::Flags) {
        self.dropped = true;
        // SAFETY: `self.cache.as_raw()` and `self.as_raw()` return valid
        // pointers to the cache and sheaf respectively.
        unsafe {
            bindings::kmem_cache_return_sheaf(self.cache.as_raw(), flags.as_raw(), self.as_raw())
        };
        drop(self);
    }

    /// Allocates an object from the sheaf.
    ///
    /// Returns a new [`SBox`] containing an initialized object, or [`None`]
    /// if the sheaf is depleted. Allocations are guaranteed to succeed as
    /// long as the sheaf contains pre-allocated objects.
    ///
    /// The `gfp` flags passed to `kmem_cache_alloc_from_sheaf` are set to zero,
    /// meaning no additional flags like `__GFP_ZERO` or `__GFP_ACCOUNT` are
    /// applied.
    ///
    /// The returned `T` is initialized as part of this function.
    pub fn alloc(&mut self) -> Option<SBox<T>> {
        // SAFETY: `self.cache.as_raw()` and `self.as_raw()` return valid
        // pointers. The function returns NULL when the sheaf is empty.
        let ptr = unsafe {
            bindings::kmem_cache_alloc_from_sheaf_noprof(self.cache.as_raw(), 0, self.as_raw())
        };

        // SAFETY:
        // - `ptr` is a valid pointer as it was just returned by the cache.
        // - The initializer is infallible, so an error is never returned.
        unsafe { T::init().__init(ptr.cast()) }.expect("Initializer is infallible");

        let ptr = NonNull::new(ptr.cast::<T>())?;

        // INVARIANT: `ptr` was returned by `kmem_cache_alloc_from_sheaf_noprof`
        // and initialized above. `cache` is the cache from which this object
        // was allocated. The object remains valid until freed in `Drop`.
        Some(SBox {
            ptr,
            cache: self.cache.clone(),
        })
    }
}

impl<T: KMemCacheInit<T>> Drop for Sheaf<T> {
    fn drop(&mut self) {
        if !self.dropped {
            // SAFETY: `self.cache.as_raw()` and `self.as_raw()` return valid
            // pointers. Using `GFP_NOWAIT` because the drop may occur in a
            // context where sleeping is not permitted.
            unsafe {
                bindings::kmem_cache_return_sheaf(
                    self.cache.as_raw(),
                    GFP_NOWAIT.as_raw(),
                    self.as_raw(),
                )
            };
        }
    }
}

/// An owned allocation from a cache sheaf.
///
/// `SBox` is similar to `Box` but is backed by a slab cache allocation obtained
/// through a [`Sheaf`]. It provides owned access to an initialized object and
/// ensures the object is properly freed back to the cache when dropped.
///
/// The contained `T` is initialized when the `SBox` is returned from alloc and
/// dropped when the `SBox` is dropped.
///
/// # Invariants
///
/// - `ptr` points to a valid, initialized object of type `T`.
/// - `cache` is the cache from which this object was allocated.
/// - The object remains valid for the lifetime of the `SBox`.
pub struct SBox<T: KMemCacheInit<T>> {
    ptr: NonNull<T>,
    cache: Arc<KMemCache<T>>,
}

impl<T: KMemCacheInit<T>> Deref for SBox<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `ptr` is valid and properly aligned per the type invariants.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: KMemCacheInit<T>> DerefMut for SBox<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `ptr` is valid and properly aligned per the type invariants,
        // and we have exclusive access via `&mut self`.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: KMemCacheInit<T>> Drop for SBox<T> {
    fn drop(&mut self) {
        // SAFETY: By type invariant, `ptr` points to a valid and initialized
        // object. We do not touch `ptr` after returning it to the cache.
        unsafe { core::ptr::drop_in_place(self.ptr.as_ptr()) };

        // SAFETY: `self.ptr` was allocated from `self.cache` via
        // `kmem_cache_alloc_from_sheaf_noprof` and is valid.
        unsafe {
            bindings::kmem_cache_free(self.cache.as_raw(), self.ptr.as_ptr().cast());
        }
    }
}
