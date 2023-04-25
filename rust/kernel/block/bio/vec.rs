// SPDX-License-Identifier: GPL-2.0

//! Types for working with `struct bio_vec` IO vectors
//!
//! C header: [`include/linux/bvec.h`](../../include/linux/bvec.h)

use super::Bio;
use crate::error::{code, Result};
use crate::page::{Page, PAGE_SIZE};
use crate::prelude::*;
use core::fmt;
use core::mem::ManuallyDrop;

/// A segment of an IO request.
///
/// [`Segment`] represents a contiguous range of physical memory addresses of an
/// IO request. A segment has a offset and a length, representing the amount of
/// data that needs to be processed. Processing the data increases the offset
/// and reduces the length.
///
/// The data buffer of a [`Segment`] is mutably borrowed from the owning `Bio`.
///
/// A wrapper around a `strutct bio_vec`.
///
/// # Invariants
///
/// `bio_vec` must always be initialized and valid for read and write
pub struct Segment<'a> {
    bio_vec: bindings::bio_vec,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl Segment<'_> {
    /// Get he lenght of the segment in bytes.
    #[inline(always)]
    pub fn len(&self) -> u32 {
        self.bio_vec.bv_len
    }

    /// Returns true if the length of the segment is 0.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the offset field of the `bio_vec`.
    #[inline(always)]
    pub fn offset(&self) -> usize {
        self.bio_vec.bv_offset as usize
    }

    /// Advance the offset of the segment.
    ///
    /// If `count` is greater than the remaining size of the segment, an error
    /// is returned.
    pub fn advance(&mut self, count: u32) -> Result {
        if self.len() < count {
            return Err(code::EINVAL);
        }

        self.bio_vec.bv_offset += count;
        self.bio_vec.bv_len -= count;
        Ok(())
    }

    /// Copy data of this segment into `dst_page`.
    ///
    /// Copies data from the current offset to the next page boundary. That is `PAGE_SIZE -
    /// (self.offeset() % PAGE_SIZE)` bytes of data. Data is placed at offset `self.offset()` in the
    /// target page. This call will advance offset and reduce length of `self`.
    ///
    /// Returns the number of bytes copied.
    #[inline(always)]
    pub fn copy_to_page(&mut self, dst_page: Pin<&mut Page>, dst_offset: usize) -> usize {
        // SAFETY: We are not moving out of `dst_page`.
        let dst_page = unsafe { Pin::into_inner_unchecked(dst_page) };
        let src_offset = self.offset() % PAGE_SIZE;
        debug_assert!(dst_offset <= PAGE_SIZE);
        let length = (PAGE_SIZE - src_offset)
            .min(self.len() as usize)
            .min(PAGE_SIZE - dst_offset);
        let page_idx = self.offset() / PAGE_SIZE;

        // SAFETY: self.bio_vec is valid and thus bv_page must be a valid
        // pointer to a `struct page` array.
        let src_page = unsafe { Page::from_raw(self.bio_vec.bv_page.add(page_idx)) };

        src_page
            .with_pointer_into_page(src_offset, length, |src| {
                // SAFETY:
                // - If `with_pointer_into_page` calls this closure, it has performed bounds
                //   checking and guarantees that `src` is valid for `length` bytes.
                // - TODO: User space might write src_page at any time?
                // - We have exclusive ownership of `dst_page` and thus this write wil not race.
                unsafe { dst_page.write_raw(src, dst_offset, length) }
            })
            .expect("Assertion failure, bounds check failed.");

        self.advance(length as u32)
            .expect("Assertion failure, bounds check failed.");

        length
    }

    /// Copy data to the current page of this segment from `src_page`.
    ///
    /// Copies  `PAGE_SIZE - (self.offset() % PAGE_SIZE` bytes of data from `src_page` to this
    /// segment starting at `self.offset()` from offset `self.offset() % PAGE_SIZE`. This call
    /// will advance offset and reduce length of `self`.
    ///
    /// Returns the number of bytes copied.
    pub fn copy_from_page(&mut self, src_page: &Page, src_offset: usize) -> usize {
        let dst_offset = self.offset() % PAGE_SIZE;
        debug_assert!(src_offset <= PAGE_SIZE);
        let length = (PAGE_SIZE - dst_offset)
            .min(self.len() as usize)
            .min(PAGE_SIZE - src_offset);
        let page_idx = self.offset() / PAGE_SIZE;

        // SAFETY: self.bio_vec is valid and thus bv_page must be a valid
        // pointer to a `struct page`.
        let dst_page = unsafe { Page::from_raw(self.bio_vec.bv_page.add(page_idx)) };

        dst_page
            .with_pointer_into_page(dst_offset, length, |dst| {
                // SAFETY:
                // - If `with_pointer_into_page` calls this closure, then it has performed bounds
                //   checks and guarantees that `dst` is valid for `length` bytes.
                // - TODO: Nothing prevents user space from writing to dst_page?
                // - Since we have a shared reference to `src_page`, the read cannot race with any
                //   writes to `src_page`.
                unsafe { src_page.read_raw(dst, src_offset, length) }
            })
            .expect("Assertion failure, bounds check failed.");

        self.advance(length as u32)
            .expect("Assertion failure, bounds check failed.");

        length
    }

    /// Copy zeroes to the current page of this segment.
    ///
    /// Copies  `PAGE_SIZE - (self.offset() % PAGE_SIZE` bytes of data to this
    /// segment starting at `self.offset()`. This call will advance offset and reduce length of
    /// `self`.
    pub fn zero_page(&mut self) -> usize {
        let offset = self.offset() % PAGE_SIZE;
        let length = (PAGE_SIZE - offset).min(self.len() as usize);
        let page_idx = self.offset() / PAGE_SIZE;

        // SAFETY: self.bio_vec is valid and thus bv_page must be a valid
        // pointer to a `struct page`. We do not own the page, but we prevent
        // drop by wrapping the `Page` in `ManuallyDrop`.
        let dst_page =
            ManuallyDrop::new(unsafe { Page::from_raw(self.bio_vec.bv_page.add(page_idx)) });

        // SAFETY: TODO: This migt race with user space writes.
        unsafe { dst_page.fill_zero_raw(offset, length) }
            .expect("Assertion failure, bounds check failed.");

        self.advance(length as u32)
            .expect("Assertion failure, bounds check failed.");

        length
    }
}

impl core::fmt::Display for Segment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Segment {:?} len: {}, offset: {}",
            self.bio_vec.bv_page, self.bio_vec.bv_len, self.bio_vec.bv_offset
        )
    }
}

/// An iterator over `Segment`
///
/// # Invariants
///
/// If `iter.bi_size` > 0, `iter` must always index a valid `bio_vec` in `bio.io_vec()`.
pub struct BioSegmentIterator<'a> {
    bio: &'a Bio,
    iter: bindings::bvec_iter,
}

impl<'a> BioSegmentIterator<'a> {
    /// Creeate a new segemnt iterator for iterating the segments of `bio`. The
    /// iterator starts at the beginning of `bio`.
    #[inline(always)]
    pub(crate) fn new(bio: &'a Bio) -> BioSegmentIterator<'_> {
        // SAFETY: `bio.raw_iter()` returns an index that indexes into a valid
        // `bio_vec` in `bio.io_vec()`.
        Self {
            bio,
            iter: bio.raw_iter(),
        }
    }

    // The accessors in this implementation block are modelled after C side
    // macros and static functions `bvec_iter_*` and `mp_bvec_iter_*` from
    // bvec.h.

    /// Construct a `bio_vec` from the current iterator state.
    ///
    /// This will return a `bio_vec`of size <= PAGE_SIZE
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    unsafe fn io_vec(&self) -> bindings::bio_vec {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        unsafe {
            bindings::bio_vec {
                bv_page: self.page(),
                bv_len: self.len(),
                bv_offset: self.offset(),
            }
        }
    }

    /// Get the currently indexed `bio_vec` entry.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn bvec(&self) -> &bindings::bio_vec {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By the safety requirement of this function and the type
        // invariant of `Self`, `self.iter.bi_idx` indexes into a valid
        // `bio_vec`
        unsafe { self.bio.io_vec().offset(self.iter.bi_idx as isize).as_ref() }
    }

    /// Get the  as u32currently indexed page, indexing into pages of order >= 0.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn page(&self) -> *mut bindings::page {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By C API contract, the following offset cannot exceed pages
        // allocated to this bio.
        unsafe { self.mp_page().add(self.mp_page_idx()) }
    }

    /// Get the remaining bytes in the current page. Never more than PAGE_SIZE.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn len(&self) -> u32 {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        unsafe {
            self.mp_len()
                .min((bindings::PAGE_SIZE as u32) - self.offset())
        }
    }

    /// Get the offset from the last page boundary in the currently indexed
    /// `bio_vec` entry. Never more than PAGE_SIZE.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn offset(&self) -> u32 {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        unsafe { self.mp_offset() % (bindings::PAGE_SIZE as u32) }
    }

    /// Return the first page of the currently indexed `bio_vec` entry. This
    /// might be a multi-page entry, meaning that page might have order > 0.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn mp_page(&self) -> *mut bindings::page {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        unsafe { self.bvec().bv_page }
    }

    /// Get the offset in whole pages into the currently indexed `bio_vec`. This
    /// can be more than 0 is the page has order > 0.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn mp_page_idx(&self) -> usize {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        (unsafe { self.mp_offset() } / (bindings::PAGE_SIZE as u32)) as usize
    }

    /// Get the offset in the currently indexed `bio_vec` multi-page entry. This
    /// can be more than `PAGE_SIZE` if the page has order > 0.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn mp_offset(&self) -> u32 {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        unsafe { self.bvec().bv_offset + self.iter.bi_bvec_done }
    }

    /// Get the number of remaining bytes for the currently indexed `bio_vec`
    /// entry. Can be more than PAGE_SIZE for `bio_vec` entries with pages of
    /// order > 0.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `self.iter.bi_size` > 0 before calling this
    /// method.
    #[inline(always)]
    unsafe fn mp_len(&self) -> u32 {
        debug_assert!(self.iter.bi_size > 0);
        // SAFETY: By safety requirement of this function `self.iter.bi_size` is
        // greater than 0.
        self.iter
            .bi_size
            .min(unsafe { self.bvec().bv_len } - self.iter.bi_bvec_done)
    }
}

impl<'a> crate::types::BorrowIterator for BioSegmentIterator<'a> {
    type Item<'b> = Segment<'b> where Self: 'b;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item<'_>> {
        if self.iter.bi_size == 0 {
            return None;
        }

        // SAFETY: We checked that `self.iter.bi_size` > 0 above.
        let bio_vec_ret = unsafe { self.io_vec() };

        // SAFETY: By existence of reference `&bio`, `bio.0` contains a valid
        // `struct bio`. By type invariant of `BioSegmentItarator` `self.iter`
        // indexes into a valid `bio_vec` entry. By C API contracit, `bv_len`
        // does not exceed the size of the bio.
        unsafe {
            bindings::bio_advance_iter_single(
                self.bio.0.get(),
                &mut self.iter as *mut bindings::bvec_iter,
                bio_vec_ret.bv_len,
            )
        };

        Some(Segment {
            bio_vec: bio_vec_ret,
            _marker: core::marker::PhantomData,
        })
    }
}
