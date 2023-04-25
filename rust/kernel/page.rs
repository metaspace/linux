// SPDX-License-Identifier: GPL-2.0

//! Kernel page allocation and management.

use crate::{
    alloc::{
        AllocError,
        Flags, //
    },
    bindings,
    error::{
        code::*,
        Result, //
    },
    types::{
        Opaque,
        Ownable,
        Owned, //
    },
    uaccess::UserSliceReader, //
};
use core::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::{
        self,
        NonNull, //
    }, //
};

/// A bitwise shift for the page size.
pub const PAGE_SHIFT: usize = bindings::PAGE_SHIFT as usize;

/// The number of bytes in a page.
pub const PAGE_SIZE: usize = bindings::PAGE_SIZE;

/// A bitmask that gives the page containing a given address.
pub const PAGE_MASK: usize = !(PAGE_SIZE - 1);

/// Round up the given number to the next multiple of [`PAGE_SIZE`].
///
/// It is incorrect to pass an address where the next multiple of [`PAGE_SIZE`] doesn't fit in a
/// [`usize`].
pub const fn page_align(addr: usize) -> usize {
    // Parentheses around `PAGE_SIZE - 1` to avoid triggering overflow sanitizers in the wrong
    // cases.
    (addr + (PAGE_SIZE - 1)) & PAGE_MASK
}

/// Representation of a non-owning reference to a [`Page`].
///
/// This type provides a borrowed version of a [`Page`] that is owned by some other entity, e.g. a
/// [`Vmalloc`] allocation such as [`VBox`].
///
/// # Example
///
/// ```
/// # use kernel::{bindings, prelude::*};
/// use kernel::page::{BorrowedPage, Page, PAGE_SIZE};
/// # use core::{mem::MaybeUninit, ptr, ptr::NonNull };
///
/// fn borrow_page<'a>(vbox: &'a mut VBox<MaybeUninit<[u8; PAGE_SIZE]>>) -> BorrowedPage<'a> {
///     let ptr = ptr::from_ref(&**vbox);
///
///     // SAFETY: `ptr` is a valid pointer to `Vmalloc` memory.
///     let page = unsafe { bindings::vmalloc_to_page(ptr.cast()) };
///
///     // SAFETY: `vmalloc_to_page` returns a valid pointer to a `struct page` for a valid
///     // pointer to `Vmalloc` memory.
///     let page = unsafe { NonNull::new_unchecked(page) };
///
///     // SAFETY:
///     // - `self.0` is a valid pointer to a `struct page`.
///     // - `self.0` is valid for the entire lifetime of `self`.
///     unsafe { BorrowedPage::from_raw(page) }
/// }
///
/// let mut vbox = VBox::<[u8; PAGE_SIZE]>::new_uninit(GFP_KERNEL)?;
/// let page = borrow_page(&mut vbox);
///
/// // SAFETY: There is no concurrent read or write to this page.
/// unsafe { page.fill_zero_raw(0, PAGE_SIZE)? };
/// # Ok::<(), Error>(())
/// ```
///
/// # Invariants
///
/// The borrowed underlying pointer to a `struct page` is valid for the entire lifetime `'a`.
///
/// [`VBox`]: kernel::alloc::VBox
/// [`Vmalloc`]: kernel::alloc::allocator::Vmalloc
pub struct BorrowedPage<'a>(ManuallyDrop<Owned<Page>>, PhantomData<&'a Owned<Page>>);

impl<'a> BorrowedPage<'a> {
    /// Constructs a [`BorrowedPage`] from a raw pointer to a `struct page`.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to a valid `bindings::page`.
    /// - `ptr` must remain valid for the entire lifetime `'a`.
    pub unsafe fn from_raw(ptr: NonNull<bindings::page>) -> Self {
        let page: Owned<Page> =
            // SAFETY: By function safety requirements `ptr` is non null and valid for 'a.
            unsafe { Owned::from_raw(NonNull::new_unchecked(ptr.as_ptr().cast())) };

        // INVARIANT: The safety requirements guarantee that `ptr` is valid for the entire lifetime
        // `'a`.
        Self(ManuallyDrop::new(page), PhantomData)
    }
}

impl<'a> Deref for BorrowedPage<'a> {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Trait to be implemented by types which provide an [`Iterator`] implementation of
/// [`BorrowedPage`] items, such as [`VmallocPageIter`](kernel::alloc::allocator::VmallocPageIter).
pub trait AsPageIter {
    /// The [`Iterator`] type, e.g. [`VmallocPageIter`](kernel::alloc::allocator::VmallocPageIter).
    type Iter<'a>: Iterator<Item = BorrowedPage<'a>>
    where
        Self: 'a;

    /// Returns an [`Iterator`] of [`BorrowedPage`] items over all pages owned by `self`.
    fn page_iter(&mut self) -> Self::Iter<'_>;
}

/// A pointer to a page that owns the page allocation.
///
/// # Invariants
///
/// The pointer is valid, and has ownership over the page.
#[repr(transparent)]
pub struct Page {
    page: Opaque<bindings::page>,
}

// SAFETY: Pages have no logic that relies on them staying on a given thread, so moving them across
// threads is safe.
unsafe impl Send for Page {}

// SAFETY: Pages have no logic that relies on them not being accessed concurrently, so accessing
// them concurrently is safe.
unsafe impl Sync for Page {}

impl Page {
    fn alloc_page_raw(flags: Flags) -> Result<NonNull<bindings::page>, AllocError> {
        // SAFETY: Depending on the value of `gfp_flags`, this call may sleep. Other than that, it
        // is always safe to call this method.
        let page = unsafe { bindings::alloc_pages(flags.as_raw(), 0) };
        NonNull::new(page).ok_or(AllocError)
    }

    /// Allocates a new page.
    ///
    /// # Examples
    ///
    /// Allocate memory for a page.
    ///
    /// ```
    /// use kernel::page::Page;
    ///
    /// let page = Page::alloc_page(GFP_KERNEL)?;
    /// # Ok::<(), kernel::alloc::AllocError>(())
    /// ```
    ///
    /// Allocate memory for a page and zero its contents.
    ///
    /// ```
    /// use kernel::page::Page;
    ///
    /// let page = Page::alloc_page(GFP_KERNEL | __GFP_ZERO)?;
    /// # Ok::<(), kernel::alloc::AllocError>(())
    /// ```
    #[inline]
    pub fn alloc_page(flags: Flags) -> Result<Owned<Self>, AllocError> {
        let page = Self::alloc_page_raw(flags)?;
        // SAFETY: We just successfully allocated a page, so we now have ownership of the newly
        // allocated page. We transfer that ownership to the new `Owned<Page>` object.
        // Since `Page` is transparent, we can cast the pointer directly.
        Ok(unsafe { Owned::from_raw(page.cast()) })
    }

    /// Returns a raw pointer to the page.
    pub fn as_ptr(&self) -> *mut bindings::page {
        Opaque::cast_into(&self.page)
    }

    /// Get the node id containing this page.
    pub fn nid(&self) -> i32 {
        // SAFETY: Always safe to call with a valid page.
        unsafe { bindings::page_to_nid(self.as_ptr()) }
    }

    /// Create a `&Page` from a raw `struct page` pointer
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for use as a reference for the duration of `'a`.
    pub unsafe fn from_raw<'a>(ptr: *const bindings::page) -> &'a Self {
        // SAFETY: By function safety requirements, ptr is not null and is
        // valid for use as a reference.
        unsafe { &*Opaque::cast_from(ptr).cast::<Self>() }
    }

    /// Runs a piece of code with this page mapped to an address.
    ///
    /// The page is unmapped when this call returns.
    ///
    /// # Using the raw pointer
    ///
    /// It is up to the caller to use the provided raw pointer correctly. The pointer is valid for
    /// `PAGE_SIZE` bytes and for the duration in which the closure is called. The pointer might
    /// only be mapped on the current thread, and when that is the case, dereferencing it on other
    /// threads is UB. Other than that, the usual rules for dereferencing a raw pointer apply: don't
    /// cause data races, the memory may be uninitialized, and so on.
    ///
    /// If multiple threads map the same page at the same time, then they may reference with
    /// different addresses. However, even if the addresses are different, the underlying memory is
    /// still the same for these purposes (e.g., it's still a data race if they both write to the
    /// same underlying byte at the same time).
    fn with_page_mapped<T>(&self, f: impl FnOnce(*mut u8) -> T) -> T {
        // SAFETY: `page` is valid due to the type invariants on `Page`.
        let mapped_addr = unsafe { bindings::kmap_local_page(self.as_ptr()) };

        let res = f(mapped_addr.cast());

        // This unmaps the page mapped above.
        //
        // SAFETY: Since this API takes the user code as a closure, it can only be used in a manner
        // where the pages are unmapped in reverse order. This is as required by `kunmap_local`.
        //
        // In other words, if this call to `kunmap_local` happens when a different page should be
        // unmapped first, then there must necessarily be a call to `kmap_local_page` other than the
        // call just above in `with_page_mapped` that made that possible. In this case, it is the
        // unsafe block that wraps that other call that is incorrect.
        unsafe { bindings::kunmap_local(mapped_addr) };

        res
    }

    /// Runs a piece of code with a raw pointer to a slice of this page, with bounds checking.
    ///
    /// If `f` is called, then it will be called with a pointer that points at `off` bytes into the
    /// page, and the pointer will be valid for at least `len` bytes. The pointer is only valid on
    /// this task, as this method uses a local mapping.
    ///
    /// If `off` and `len` refers to a region outside of this page, then this method returns
    /// [`EINVAL`] and does not call `f`.
    ///
    /// # Using the raw pointer
    ///
    /// It is up to the caller to use the provided raw pointer correctly. The pointer is valid for
    /// `len` bytes and for the duration in which the closure is called. The pointer might only be
    /// mapped on the current thread, and when that is the case, dereferencing it on other threads
    /// is UB. Other than that, the usual rules for dereferencing a raw pointer apply: don't cause
    /// data races, the memory may be uninitialized, and so on.
    ///
    /// If multiple threads map the same page at the same time, then they may reference with
    /// different addresses. However, even if the addresses are different, the underlying memory is
    /// still the same for these purposes (e.g., it's still a data race if they both write to the
    /// same underlying byte at the same time).
    pub(crate) fn with_pointer_into_page<T>(
        &self,
        off: usize,
        len: usize,
        f: impl FnOnce(*mut u8) -> Result<T>,
    ) -> Result<T> {
        let bounds_ok = off <= PAGE_SIZE && len <= PAGE_SIZE && (off + len) <= PAGE_SIZE;

        if bounds_ok {
            self.with_page_mapped(move |page_addr| {
                // SAFETY: The `off` integer is at most `PAGE_SIZE`, so this pointer offset will
                // result in a pointer that is in bounds or one off the end of the page.
                f(unsafe { page_addr.add(off) })
            })
        } else {
            Err(EINVAL)
        }
    }

    /// Maps the page and reads from it into the given buffer.
    ///
    /// This method will perform bounds checks on the page offset. If `offset .. offset+len` goes
    /// outside of the page, then this call returns [`EINVAL`].
    ///
    /// # Safety
    ///
    /// * Callers must ensure that `dst` is valid for writing `len` bytes.
    /// * Callers must ensure that there are no other concurrent reads or writes to/from the
    ///   destination memory region.
    /// * Callers must ensure that this call does not race with a write to the same page that
    ///   overlaps with this read.
    pub unsafe fn read_raw(&self, dst: *mut u8, offset: usize, len: usize) -> Result {
        self.with_pointer_into_page(offset, len, move |src| {
            // SAFETY: If `with_pointer_into_page` calls into this closure, then
            // it has performed a bounds check and guarantees that `src` is
            // valid for `len` bytes.
            //
            // There caller guarantees that there is no data race.
            unsafe { ptr::copy_nonoverlapping(src, dst, len) };
            Ok(())
        })
    }

    /// Maps the page and reads from it into the given memory region using byte-wise atomic memory
    /// operations.
    ///
    /// This method will perform bounds checks on the page offset. If `offset .. offset+len` goes
    /// outside of the page, then this call returns [`EINVAL`].
    ///
    /// # Safety
    ///
    /// Callers must ensure that:
    ///
    /// - `dst` is valid for writes for `len` bytes for the duration of the call.
    /// - For the duration of the call, other accesses to the area described by `dst` and `len`,
    ///   must not cause data races (defined by [`LKMM`]) against atomic operations executed by this
    ///   function. Note that if all other accesses are atomic, then this safety requirement is
    ///   trivially fulfilled.
    /// - Callers must ensure that this call does not race with a write to the source page that
    ///   overlaps with this read.
    ///
    /// [`LKMM`]: srctree/tools/memory-model
    pub unsafe fn read_bytewise_atomic(&self, dst: *mut u8, offset: usize, len: usize) -> Result {
        self.with_pointer_into_page(offset, len, move |src| {
            // SAFETY:
            // - If `with_pointer_into_page` calls into this closure, then it has performed a
            //   bounds check and guarantees that `src` is valid for `len` bytes.
            // - By function safety requirements `dst` is valid for writes for `len` bytes.
            // - By function safety requirements there are no other writes to `src` during this
            //   call.
            // - By function safety requirements all other access to `dst` during this call are
            //   atomic.
            unsafe { kernel::sync::atomic::atomic_per_byte_memcpy(src, dst, len) };
            Ok(())
        })
    }

    /// Maps the page and writes into it from the given buffer.
    ///
    /// This method will perform bounds checks on the page offset. If `offset .. offset+len` goes
    /// outside of the page, then this call returns [`EINVAL`].
    ///
    /// # Safety
    ///
    /// * Callers must ensure that `src` is valid for reading `len` bytes.
    /// * Callers must ensure that there are no concurrent writes to the source memory region.
    /// * Callers must ensure that this call does not race with a read or write to the same page
    ///   that overlaps with this write.
    pub unsafe fn write_raw(&self, src: *const u8, offset: usize, len: usize) -> Result {
        self.with_pointer_into_page(offset, len, move |dst| {
            // SAFETY: If `with_pointer_into_page` calls into this closure, then it has performed a
            // bounds check and guarantees that `dst` is valid for `len` bytes.
            //
            // There caller guarantees that there is no data race.
            unsafe { ptr::copy_nonoverlapping(src, dst, len) };
            Ok(())
        })
    }

    /// Maps the page and writes into it from the given memory region using byte-wise atomic memory
    /// operations.
    ///
    /// This method will perform bounds checks on the page offset. If `offset .. offset+len` goes
    /// outside of the page, then this call returns [`EINVAL`].
    ///
    /// # Safety
    ///
    /// Callers must ensure that:
    ///
    /// - `src` is valid for reads for `len` bytes for the duration of the call.
    /// - For the duration of the call, other accesses to the areas described by `src` and `len`,
    ///   must not cause data races (defined by [`LKMM`]) against atomic operations executed by this
    ///   function. Note that if all other accesses are atomic, then this safety requirement is
    ///   trivially fulfilled.
    /// - Callers must ensure that this call does not race with a read or write to the destination
    ///   page that overlaps with this write.
    ///
    /// [`LKMM`]: srctree/tools/memory-model
    pub unsafe fn write_bytewise_atomic(
        &self,
        src: *const u8,
        offset: usize,
        len: usize,
    ) -> Result {
        self.with_pointer_into_page(offset, len, move |dst| {
            // SAFETY:
            // - By function safety requirements `src` is valid for writes for `len` bytes.
            // - If `with_pointer_into_page` calls into this closure, then it has performed a
            //   bounds check and guarantees that `dst` is valid for `len` bytes.
            // - By function safety requirements there are no other writes to `dst` during this
            //   call.
            // - By function safety requirements all other access to `src` during this call are
            //   atomic.
            unsafe { kernel::sync::atomic::atomic_per_byte_memcpy(src, dst, len) };
            Ok(())
        })
    }

    /// Maps the page and zeroes the given slice.
    ///
    /// This method will perform bounds checks on the page offset. If `offset .. offset+len` goes
    /// outside of the page, then this call returns [`EINVAL`].
    ///
    /// # Safety
    ///
    /// Callers must ensure that this call does not race with a read or write to the same page that
    /// overlaps with this write.
    pub unsafe fn fill_zero_raw(&self, offset: usize, len: usize) -> Result {
        self.with_pointer_into_page(offset, len, move |dst| {
            // SAFETY: If `with_pointer_into_page` calls into this closure, then it has performed a
            // bounds check and guarantees that `dst` is valid for `len` bytes.
            //
            // There caller guarantees that there is no data race.
            unsafe { ptr::write_bytes(dst, 0u8, len) };
            Ok(())
        })
    }

    /// Copies data from userspace into this page.
    ///
    /// This method will perform bounds checks on the page offset. If `offset .. offset+len` goes
    /// outside of the page, then this call returns [`EINVAL`].
    ///
    /// Like the other `UserSliceReader` methods, data races are allowed on the userspace address.
    /// However, they are not allowed on the page you are copying into.
    ///
    /// # Safety
    ///
    /// Callers must ensure that this call does not race with a read or write to the same page that
    /// overlaps with this write.
    pub unsafe fn copy_from_user_slice_raw(
        &self,
        reader: &mut UserSliceReader,
        offset: usize,
        len: usize,
    ) -> Result {
        self.with_pointer_into_page(offset, len, move |dst| {
            // SAFETY: If `with_pointer_into_page` calls into this closure, then it has performed a
            // bounds check and guarantees that `dst` is valid for `len` bytes. Furthermore, we have
            // exclusive access to the slice since the caller guarantees that there are no races.
            reader.read_raw(unsafe { core::slice::from_raw_parts_mut(dst.cast(), len) })
        })
    }
}

// SAFETY: `Owned<Page>` objects returned by Page::alloc_page() follow the requirements of
// the Ownable abstraction.
unsafe impl Ownable for Page {
    #[inline]
    unsafe fn release(this: NonNull<Self>) {
        // SAFETY: By the type invariants, we have ownership of the page and can free it.
        // Since Page is transparent, we can cast the raw pointer directly.
        unsafe { bindings::__free_pages(this.cast().as_ptr(), 0) };
    }
}

/// A page whose data area does not incur data races.
///
/// [`SafePage`] has the same usage constraints as other Rust types. Thus, it cannot be mapped to
/// user space or shared with devices. This makes it safe to reference the contents of the page
/// while the page is mapped in kernel space.
///
/// # Invariants
///
/// There are no data races for the contents of this page.
#[repr(transparent)]
pub struct SafePage(Page);

impl SafePage {
    /// Allocate a new `SafePage`.
    pub fn alloc_page(flags: Flags) -> Result<Owned<Self>, AllocError> {
        let page = Page::alloc_page_raw(flags)?;

        // SAFETY: We just successfully allocated a page, so we now have ownership of the newly
        // allocated page. We transfer that ownership to the new `Owned<Page>` object.
        // Since `Page` and `SafePage` is transparent, we can cast the pointer directly.
        Ok(unsafe { Owned::from_raw(page.cast()) })
    }

    /// Copies data from this page to another page at the specified offset.
    ///
    /// # Arguments
    ///
    /// - `dst` - The destination page to copy data to.
    /// - `offset` - The byte offset within both pages where copying starts.
    /// - `len` - The number of bytes to copy.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::page::SafePage;
    /// # use kernel::alloc::flags::GFP_KERNEL;
    /// let mut src_page = SafePage::alloc_page(GFP_KERNEL)?;
    /// let mut dst_page = SafePage::alloc_page(GFP_KERNEL)?;
    /// src_page.copy_to_page(dst_page.get_pin_mut(), 0, 1024)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn copy_to_page(&self, dst: Pin<&mut Self>, offset: usize, len: usize) -> Result {
        // INVARIANT: The following code makes sure to not cause data races.
        self.with_pointer_into_page(offset, len, |src| {
            // SAFETY:
            // - If `with_pointer_into_page` calls into this closure, then it has performed a
            //   bounds check and guarantees that `src` is valid for `len` bytes.
            // - By type invariant and existence of shared reference, there are no other writes to
            //   `src` during this call.
            // - By exclusive ownership of `dst`, there are no other writes to `dst` during this
            //   call.
            unsafe { dst.write_raw(src, offset, len) }
        })
    }
}

// SAFETY: `Owned<SafePage>` objects returned by SafePage::alloc_page() follow the requirements of
// the Ownable abstraction.
unsafe impl Ownable for SafePage {
    #[inline]
    unsafe fn release(this: NonNull<Self>) {
        // SAFETY: By the type invariants, we have ownership of the page and can free it. Since
        // `SafePage` and `Page` are transparent, we can cast the raw pointer directly.
        unsafe { bindings::__free_pages(this.cast().as_ptr(), 0) };
    }
}

impl Deref for SafePage {
    type Target = Page;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SafePage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
