// SPDX-License-Identifier: GPL-2.0

//! Types for working with the bio layer.
//!
//! C header: [`include/linux/blk_types.h`](../../include/linux/blk_types.h)

use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;

mod vec;

pub use vec::BioSegmentIterator;
pub use vec::Segment;

use crate::types::Opaque;

/// A block device IO descriptor (`struct bio`).
///
/// A `Bio` is the main unit of IO for the block layer. It describes an IO command and associated
/// data buffers.
///
/// The data buffers associated with a `Bio` are represented by a vector of [`Segment`]s. These
/// segments represent physically contiguous regions of memory. The memory is represented by
/// [`Page`] descriptors internally.
///
/// The vector of [`Segment`]s can be iterated by obtaining a [`SegmentIterator`].
///
/// # Invariants
///
/// Instances of this type is always reference counted. A call to
/// `bindings::bio_get()` ensures that the instance is valid for read at least
/// until a matching call to `bindings :bio_put()`.
#[repr(transparent)]
pub struct Bio(Opaque<bindings::bio>);

impl Bio {
    /// Returns an iterator over segments in this `Bio`. Does not consider
    /// segments of other bios in this bio chain.
    #[inline(always)]
    pub fn segment_iter(&mut self) -> BioSegmentIterator<'_> {
        BioSegmentIterator::new(self)
    }

    /// Get the number of io vectors in this bio.
    fn io_vec_count(&self) -> u16 {
        // SAFETY: By the type invariant of `Bio` and existence of `&self`,
        // `self.0` is valid for read.
        unsafe { (*self.0.get()).bi_vcnt }
    }

    /// Get slice referencing the `bio_vec` array of this bio
    #[inline(always)]
    fn io_vec(&self) -> NonNull<bindings::bio_vec> {
        let this = self.0.get();

        // SAFETY: By the type invariant of `Bio` and existence of `&self`,
        // `this` is valid for read.
        let vec_ptr = unsafe { (*this).bi_io_vec };

        // SAFETY: By C API contract, bi_io_vec is always set, even if bi_vcnt
        // is zero.
        unsafe { NonNull::new_unchecked(vec_ptr) }
    }

    /// Return a copy of the `bvec_iter` for this `Bio`. This iterator always
    /// indexes to a valid `bio_vec` entry.
    // TODO: Should not be pub
    #[inline(always)]
    pub fn raw_iter(&self) -> bindings::bvec_iter {
        // SAFETY: By the type invariant of `Bio` and existence of `&self`,
        // `self` is valid for read.
        unsafe { (*self.0.get()).bi_iter }
    }

    /// Create an instance of `Bio` from a raw pointer.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the `ptr` is valid for use as a reference to
    /// `Bio` for the duration of `'a`.
    #[inline(always)]
    pub(crate) unsafe fn from_raw<'a>(ptr: *mut bindings::bio) -> Option<&'a Self> {
        Some(
            // SAFETY: by the safety requirement of this funciton, `ptr` is
            // valid for read for the duration of the returned lifetime
            unsafe { &*NonNull::new(ptr)?.as_ptr().cast::<Bio>() },
        )
    }

    /// Create an instance of `Bio` from a raw pointer.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the `ptr` is valid for use as a unique reference
    /// to `Bio` for the duration of `'a`.
    #[inline(always)]
    pub(crate) unsafe fn from_raw_mut<'a>(ptr: *mut bindings::bio) -> Option<&'a mut Self> {
        Some(
            // SAFETY: by the safety requirement of this funciton, `ptr` is
            // valid for read for the duration of the returned lifetime
            unsafe { &mut *NonNull::new(ptr)?.as_ptr().cast::<Bio>() },
        )
    }
}

impl core::fmt::Display for Bio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Bio({:?}, vcnt: {}, idx: {}, size: 0x{:x}, completed: 0x{:x})",
            self.0.get(),
            self.io_vec_count(),
            self.raw_iter().bi_idx,
            self.raw_iter().bi_size,
            self.raw_iter().bi_bvec_done
        )
    }
}

/// An iterator over `Bio` in a bio chain, yielding `&mut Bio`.
///
/// # Invariants
///
/// `bio` must be either `None` or be valid for use as a `&mut Bio`.
pub struct BioIterator<'a> {
    pub(crate) bio: Option<NonNull<Bio>>,
    pub(crate) _p: PhantomData<&'a ()>,
}

impl<'a> core::iter::Iterator for BioIterator<'a> {
    type Item = &'a mut Bio;

    #[inline(always)]
    fn next(&mut self) -> Option<&'a mut Bio> {
        let mut current = self.bio.take()?;
        // SAFETY: By the type invariant of `Bio` and type invariant on `Self`,
        // `current` is valid for use as a unique reference.
        let next = unsafe { (*current.as_ref().0.get()).bi_next };
        self.bio = NonNull::new(next.cast());
        // SAFETY: By type invariant, `bio` is valid for use as a reference.
        Some(unsafe { current.as_mut() })
    }
}
