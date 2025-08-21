// SPDX-License-Identifier: GPL-2.0

//! Bad blocks tracking for block devices.
//!
//! This module provides a safe Rust wrapper around the badblocks
//! infrastructure, which is used to track and manage bad sectors on block
//! devices. Bad blocks are sectors that cannot reliably store data and should
//! be avoided during I/O operations.

use core::ops::{Range, RangeBounds};

use crate::{
    error::to_result,
    page::PAGE_SIZE,
    prelude::*,
    sync::atomic::{ordering, Atomic},
    types::Opaque,
};
use pin_init::{pin_data, PinInit};

/// A bad blocks tracker for managing defective sectors on a block device.
///
/// `BadBlocks` provides functionality to mark sectors as bad and check if
/// ranges contain bad blocks. This is useful for some classes of drivers to
/// maintain data integrity by avoiding known bad sectors.
///
/// # Storage Format
///
/// Bad blocks are stored in a compact format where each 64-bit entry contains:
/// - **Sector offset** (54 bits): Starting sector of the bad range
/// - **Length** (9 bits): Number of sectors (1-512) in the bad range
/// - **Acknowledged flag** (1 bit): Whether the bad blocks have been acknowledged
///
/// The bad blocks tracker uses exactly one page ([`PAGE_SIZE`]) of memory to store
/// bad block entries. This allows tracking up to `PAGE_SIZE/8` bad block ranges
/// (typically 512 ranges on systems with 4KB pages).
///
/// # Locking
///
/// Operations on the structure is internally synchronized by a seqlock.
///
/// # Examples
///
/// Basic usage:
///
/// ```rust
/// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
/// # use kernel::prelude::*;
///
/// // Create a new bad blocks tracker
/// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
///
/// // Mark sectors 100-109 as bad (unacknowledged)
/// bad_blocks.set_bad(100..110, false)?;
///
/// // Check if sector range 95-104 contains bad blocks
/// match bad_blocks.check(95..105) {
///     BlockStatus::None => pr_info!("No bad blocks found"),
///     BlockStatus::Acknowledged(range) => pr_warn!("Acknowledged bad blocks: {:?}", range),
///     BlockStatus::Unacknowledged(range) => pr_err!("Unacknowledged bad blocks: {:?}", range),
/// }
/// # Ok::<(), kernel::error::Error>(())
/// ```
/// # Invariants
///
/// - `self.blocks` is a valid `bindings::badblocks` struct.
#[pin_data(PinnedDrop)]
pub struct BadBlocks {
    #[pin]
    blocks: Opaque<bindings::badblocks>,
}

impl BadBlocks {
    /// Creates a new bad blocks tracker.
    ///
    /// Initializes an empty bad blocks tracker that can manage defective sectors
    /// on a block device. The tracker starts with no bad blocks recorded and
    /// allocates a single page for storing bad block entries.
    ///
    /// # Returns
    ///
    /// Returns a [`PinInit`] that can be used to initialize a [`BadBlocks`] instance.
    /// Initialization may fail with `ENOMEM` if memory allocation fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// // Create and initialize a bad blocks tracker
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // The tracker is ready to use with no bad blocks initially
    /// match bad_blocks.check(0..100) {
    ///     BlockStatus::None => pr_info!("No bad blocks found initially"),
    ///     _ => unreachable!(),
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn new(enable: bool) -> impl PinInit<Self, Error> {
        // INVARIANT: We initialize `self.blocks` below. If initialization fails, an error is returned.
        try_pin_init!(Self {
            blocks <- Opaque::try_ffi_init(|slot| {
                // SAFETY: `slot` is a valid pointer to uninitialized memory
                // allocated by the Opaque type. `badblocks_init` is safe to
                // call with uninitialized memory.
                to_result(unsafe {bindings::badblocks_init(slot, enable.then_some(1).unwrap_or(0))})
            }),
        })
    }

    fn shift_ref(&self) -> &Atomic<c_int> {
        let ptr = unsafe { &raw const (*self.blocks.get()).shift };
        unsafe { Atomic::from_ptr(ptr.cast_mut().cast()) }
    }

    /// Enables the bad blocks tracker if it was previously disabled.
    ///
    /// Attempts to enable bad block tracking by transitioning the tracker from
    /// a disabled state to an enabled state.
    ///
    /// # Behavior
    ///
    /// - If the tracker is disabled, it will be enabled.
    /// - If the tracker is already enabled, this operation has no effect.
    /// - The operation is atomic and thread-safe.
    ///
    /// # Usage
    ///
    /// Bad blocks trackers can be created in a disabled state and enabled later
    /// when needed. This is useful for conditional bad block tracking or for
    /// deferring activation until the device is fully initialized.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use kernel::block::badblocks::BadBlocks;
    /// # use kernel::prelude::*;
    ///
    /// // Create a disabled bad blocks tracker
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(false), GFP_KERNEL)?;
    /// assert!(!bad_blocks.enabled());
    ///
    /// // Enable it when needed
    /// bad_blocks.enable();
    /// assert!(bad_blocks.enabled());
    ///
    /// // Subsequent enable calls have no effect
    /// bad_blocks.enable();
    /// assert!(bad_blocks.enabled());
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn enable(&self) {
        let _ = self.shift_ref().cmpxchg(-1, 0, ordering::Relaxed);
    }

    /// Checks whether the bad blocks tracker is currently enabled.
    ///
    /// Returns `true` if bad block tracking is active, `false` if it is disabled.
    /// When disabled, the tracker will not perform bad block checks or operations.
    ///
    /// # Returns
    ///
    /// - `true` - Bad block tracking is enabled and operational
    /// - `false` - Bad block tracking is disabled
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and uses atomic operations to check the
    /// tracker's state without requiring external synchronization.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use kernel::block::badblocks::BadBlocks;
    /// # use kernel::prelude::*;
    ///
    /// // Create an enabled tracker
    /// let enabled_tracker = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    /// assert!(enabled_tracker.enabled());
    ///
    /// // Create a disabled tracker
    /// let disabled_tracker = KBox::pin_init(BadBlocks::new(false), GFP_KERNEL)?;
    /// assert!(!disabled_tracker.enabled());
    ///
    /// // Enable and verify
    /// disabled_tracker.enable();
    /// assert!(disabled_tracker.enabled());
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn enabled(&self) -> bool {
        self.shift_ref().load(ordering::Relaxed) >= 0
    }

    /// Marks a range of sectors as bad.
    ///
    /// Records a contiguous range of sectors as defective in the bad blocks tracker.
    /// Bad sectors should be avoided during I/O operations to prevent data corruption.
    /// The implementation may merge, split, or extend existing ranges as needed.
    ///
    /// # Parameters
    ///
    /// - `range` - The range of sectors to mark as bad. Each individual range is limited to 512 sectors maximum by the underlying implementation.
    /// - `acknowledged` - Whether the bad blocks have been acknowledged to be bad. Acknowledged bad blocks may be handled differently by some subsystems.
    ///
    /// # Acknowledgment Semantics
    ///
    /// - **Unacknowledged** (`acknowledged = false`): Newly discovered bad blocks that
    ///   need attention. These are often treated as errors by upper layers.
    /// - **Acknowledged** (`acknowledged = true`): Blocks that have been confirmed bad. These may be should be handled by remapping.
    ///
    /// # Range Management
    ///
    /// The implementation automatically:
    /// - **Merges** adjacent or overlapping ranges with the same acknowledgment status
    /// - **Splits** ranges when acknowledgment status differs
    /// - **Extends** existing ranges when new bad blocks are adjacent
    /// - **Limits** individual ranges to 512 sectors maximum (BB_MAX_LEN)
    ///
    /// Please see [C documentation] for details.
    ///
    /// # Performance
    ///
    /// Executes in O(n) time where n is number of entries in the bad block table.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Bad blocks were successfully recorded
    /// * `Err(ENOMEM)` - Insufficient space in bad blocks table (table full)
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Mark sectors 1000-1009 as bad (unacknowledged)
    /// bad_blocks.set_bad(1000..1010, false)?;
    ///
    /// // Mark a single sector as bad and acknowledged
    /// bad_blocks.set_bad(2000..2001, true)?;
    ///
    /// // Verify the bad blocks are recorded
    /// assert!(matches!(bad_blocks.check(1000..1010), BlockStatus::Unacknowledged(_)));
    /// assert!(matches!(bad_blocks.check(2000..2001), BlockStatus::Acknowledged(_)));
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// Range merging behavior:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Add adjacent ranges with same acknowledgment status
    /// bad_blocks.set_bad(100..105, false)?;  // Sectors 100-104
    /// bad_blocks.set_bad(105..108, false)?;  // Sectors 105-107
    ///
    /// // These will be merged into a single range 100-107
    /// match bad_blocks.check(100..108) {
    ///     BlockStatus::Unacknowledged(range) => {
    ///         assert_eq!(range.start, 100);
    ///         assert_eq!(range.end, 108);
    ///     },
    ///     _ => panic!("Expected unacknowledged bad blocks"),
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// Handling acknowledgment conflicts:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Mark range as unacknowledged
    /// bad_blocks.set_bad(200..210, false)?;
    ///
    /// // Acknowledge part of the range (will split)
    /// bad_blocks.set_bad(205..208, true)?;
    ///
    /// // Now we have: unack[200-204], ack[205-207], unack[208-209]
    /// assert!(matches!(bad_blocks.check(200..205), BlockStatus::Unacknowledged(_)));
    /// assert!(matches!(bad_blocks.check(205..208), BlockStatus::Acknowledged(_)));
    /// assert!(matches!(bad_blocks.check(208..210), BlockStatus::Unacknowledged(_)));
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// [C documentation]: srctree/block/badblocks.c
    pub fn set_bad(&self, range: impl RangeBounds<u64>, acknowledged: bool) -> Result {
        let range = Self::range(range);

        // SAFETY: By type invariant `self.blocks` is valid. The C function
        // `badblocks_set` handles synchronization internally.
        unsafe {
            bindings::badblocks_set(
                self.blocks.get(),
                range.start,
                range.end - range.start,
                acknowledged.then_some(1).unwrap_or(0),
            )
        }
        .then_some(())
        .ok_or(ENOMEM)
    }

    /// Marks a range of sectors as good.
    ///
    /// Removes a contiguous range of sectors from the bad blocks tracker,
    /// indicating that these sectors are now reliable for I/O operations.
    /// This is typically used after bad sectors have been repaired, remapped,
    /// or determined to be false positives.
    ///
    /// # Parameters
    ///
    /// - `range` - The range of sectors to mark as good.
    ///
    /// # Behavior
    ///
    /// The implementation handles various scenarios automatically:
    /// - **Complete removal**: If the range exactly matches a bad block range, it's removed entirely
    /// - **Partial removal**: If the range partially overlaps, the bad block range is split or trimmed
    /// - **No effect**: If the range doesn't overlap any bad blocks, the operation succeeds without changes
    /// - **Range splitting**: If the cleared range is in the middle of a bad block range, it may split the range in two
    ///
    /// # Performance
    ///
    /// Executes in O(n) time where n is the number of entries in the bad blocks table.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Sectors were successfully marked as good (or were already good)
    /// * `Err(EINVAL)` - Operation failed (typically due to table constraints)
    ///
    /// # Examples
    ///
    /// Basic usage after repair:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Mark some sectors as bad initially
    /// bad_blocks.set_bad(100..110, false)?;
    /// assert!(matches!(bad_blocks.check(100..110), BlockStatus::Unacknowledged(_)));
    ///
    /// // After successful repair, mark them as good
    /// bad_blocks.set_good(100..110)?;
    /// assert!(matches!(bad_blocks.check(100..110), BlockStatus::None));
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// Partial clearing:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Mark a large range as bad
    /// bad_blocks.set_bad(200..220, false)?;
    ///
    /// // Clear only the middle portion
    /// bad_blocks.set_good(205..215)?; // Clear sectors 205-214
    ///
    /// // Now we have bad blocks at the edges: 200-204 and 215-219
    /// assert!(matches!(bad_blocks.check(200..205), BlockStatus::Unacknowledged(_)));
    /// assert!(matches!(bad_blocks.check(205..215), BlockStatus::None));
    /// assert!(matches!(bad_blocks.check(215..220), BlockStatus::Unacknowledged(_)));
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// Safe clearing of potentially good sectors:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // It's safe to clear sectors that were never marked as bad
    /// bad_blocks.set_good(1000..1100)?; // No-op, but succeeds
    /// assert!(matches!(bad_blocks.check(1000..1100), BlockStatus::None));
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn set_good(&self, range: impl RangeBounds<u64>) -> Result {
        let range = Self::range(range);
        // SAFETY: By type invariant `self.blocks` is valid. The C function
        // `badblocks_clear` handles synchronization internally.
        unsafe {
            bindings::badblocks_clear(self.blocks.get(), range.start, range.end - range.start)
        }
        .then_some(())
        .ok_or(EINVAL)
    }

    // Transform a `RangeBounds` to start included end excluded range.
    fn range(range: impl RangeBounds<u64>) -> Range<u64> {
        let start = match range.start_bound() {
            core::ops::Bound::Included(start) => *start,
            core::ops::Bound::Excluded(start) => start + 1,
            core::ops::Bound::Unbounded => u64::MIN,
        };

        let end = match range.end_bound() {
            core::ops::Bound::Included(end) => end + 1,
            core::ops::Bound::Excluded(end) => *end,
            core::ops::Bound::Unbounded => u64::MAX,
        };

        start..end
    }

    /// Checks if a range of sectors contains any bad blocks.
    ///
    /// Examines the specified sector range to determine if it contains any sectors
    /// that have been marked as bad. This is typically called before performing I/O
    /// operations to avoid accessing defective sectors. The check uses seqlocks to
    /// ensure consistent reads even under concurrent modifications.
    ///
    /// # Parameters
    ///
    /// - `range` - The range of sectors to check (supports any type implementing `RangeBounds<u64>`).
    ///
    /// # Returns
    ///
    /// Returns a [`BlockStatus`] indicating the state of the checked range:
    ///
    /// - `BlockStatus::None` - No bad blocks found in the specified range.
    /// - `BlockStatus::Acknowledged(range)` - Contains acknowledged bad blocks.
    /// - `BlockStatus::Unacknowledged(range)` - Contains unacknowledged bad blocks.
    ///
    /// The returned range indicates the **first bad block range** encountered that
    /// overlaps with the checked area. If multiple separate bad ranges exist, only
    /// the first is reported.
    ///
    /// # Performance
    ///
    /// The check operation uses binary search on the sorted bad blocks table,
    /// providing O(log n) lookup time where n is the number of bad block ranges.
    ///
    /// # Examples
    ///
    /// Basic checking:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Initially no bad blocks
    /// assert!(matches!(bad_blocks.check(0..1000), BlockStatus::None));
    ///
    /// // Mark some sectors as bad
    /// bad_blocks.set_bad(100..110, false)?;
    ///
    /// // Check various ranges
    /// match bad_blocks.check(90..120) {
    ///     BlockStatus::Unacknowledged(range) => {
    ///         assert_eq!(range.start, 100);
    ///         assert_eq!(range.end, 110);
    ///         pr_warn!("Found unacknowledged bad blocks: {}-{}", range.start, range.end - 1);
    ///     },
    ///     _ => panic!("Expected bad blocks"),
    /// }
    ///
    /// // Check range that doesn't overlap
    /// assert!(matches!(bad_blocks.check(0..50), BlockStatus::None));
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// Handling different acknowledgment states:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    ///
    /// // Add both acknowledged and unacknowledged bad blocks
    /// bad_blocks.set_bad(100..105, true)?;   // Acknowledged
    /// bad_blocks.set_bad(200..205, false)?;  // Unacknowledged
    ///
    /// match bad_blocks.check(95..105) {
    ///     BlockStatus::Acknowledged(range) => {
    ///         pr_info!("Acknowledged bad blocks found, can potentially remap: {:?}", range);
    ///         // Continue with remapping logic
    ///     },
    ///     BlockStatus::Unacknowledged(range) => {
    ///         pr_err!("Unacknowledged bad blocks found, requires attention: {:?}", range);
    ///         // Handle as error condition
    ///     },
    ///     BlockStatus::None => {
    ///         // Safe to proceed with I/O
    ///     },
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    ///
    /// Safe I/O operation pattern:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    /// # use core::ops::RangeBounds;
    /// # fn perform_sector_read(range: impl RangeBounds<u64>) -> Result<()> { Ok(()) }
    ///
    /// fn safe_read_sectors(
    ///     bad_blocks: &BadBlocks,
    ///     range: impl RangeBounds<u64> + Clone
    /// ) -> Result<()> {
    ///     // Check for bad blocks before attempting I/O
    ///     match bad_blocks.check(range.clone()) {
    ///         BlockStatus::None => {
    ///             // Safe to proceed with I/O operation - convert range to start/count for legacy function
    ///             perform_sector_read(range)
    ///         },
    ///         BlockStatus::Acknowledged(range) => {
    ///             pr_warn!("I/O intersects acknowledged bad blocks: {:?}", range);
    ///             // Potentially remap or skip bad sectors
    ///             Err(EIO)
    ///         },
    ///         BlockStatus::Unacknowledged(range) => {
    ///             pr_err!("I/O intersects unacknowledged bad blocks: {:?}", range);
    ///             // Treat as serious error
    ///             Err(EIO)
    ///         },
    ///     }
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn check(&self, range: impl RangeBounds<u64>) -> BlockStatus {
        let mut first_bad = 0;
        let mut bad_count = 0;
        let range = Self::range(range);

        // SAFETY: By type invariant `self.blocks` is valid. `first_bad` and
        // `bad_count` are valid mutable references The C function
        // `badblocks_check` handles synchronization internally.
        let ret = unsafe {
            bindings::badblocks_check(
                self.blocks.get(),
                range.start,
                range.end - range.start,
                &mut first_bad,
                &mut bad_count,
            )
        };

        match ret {
            0 => BlockStatus::None,
            1 => BlockStatus::Acknowledged(first_bad..first_bad + bad_count),
            -1 => BlockStatus::Unacknowledged(first_bad..first_bad + bad_count),
            _ => {
                debug_assert!(false, "Illegal return value from `badblocks_check`");
                BlockStatus::None
            }
        }
    }

    /// Formats bad blocks information into a human-readable string.
    ///
    /// Exports the current bad blocks table to a text representation suitable
    /// for display via sysfs. The output format shows each bad block range
    /// with sector numbers and acknowledgment status.
    ///
    /// # Parameters
    ///
    /// - `page` - A page-sized buffer to write the formatted output into.
    /// - `show_unacknowledged` - Whether to include unacknowledged bad blocks in output.
    ///   - `true`: Shows both acknowledged and unacknowledged bad blocks
    ///   - `false`: Shows only acknowledged bad blocks
    ///
    /// # Output Format
    ///
    /// The output consists of space-separated entries, each representing a bad block range:
    /// - Format: `start_sector length [acknowledgment_status]`
    /// - Acknowledged blocks: Just sector and length (e.g., "100 10")  
    /// - Unacknowledged blocks: Sector, length, and "u" suffix (e.g., "200 5 u")
    ///
    /// # Returns
    ///
    /// Returns the number of bytes written to the buffer, or a negative value on error.
    /// The returned length can be used to extract the valid portion of the buffer.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```rust
    /// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
    /// # use kernel::prelude::*;
    /// # use kernel::page::PAGE_SIZE;
    ///
    /// let bad_blocks = KBox::pin_init(BadBlocks::new(true), GFP_KERNEL)?;
    /// let mut page = [0u8; PAGE_SIZE];
    ///
    /// // Add some bad blocks
    /// bad_blocks.set_bad(100..110, true)?;   // Acknowledged
    /// bad_blocks.set_bad(200..205, false)?;   // Unacknowledged
    ///
    /// // Show all bad blocks (including unacknowledged)
    /// let len = bad_blocks.show(&mut page, true);
    /// if len > 0 {
    ///     let output = core::str::from_utf8(&page[..len as usize]).unwrap_or("<invalid utf8>");
    ///     pr_info!("Bad blocks: {}", output);
    ///     // Output might be: "100 10 200 5 u"
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn show(&self, page: &mut [u8; PAGE_SIZE], show_unacknowledged: bool) -> isize {
        // SAFETY: By type invariant `self.blocks` is valid. The C function
        // `badblocks_show` handles synchronization internally.
        // `page.as_mut_ptr()` returns a valid pointer to a PAGE_SIZE buffer.
        // The C function will not write beyond the provided buffer size.
        unsafe {
            bindings::badblocks_show(
                self.blocks.get(),
                page.as_mut_ptr(),
                show_unacknowledged.then_some(1).unwrap_or(0),
            )
        }
    }
}

#[pinned_drop]
impl PinnedDrop for BadBlocks {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: We do not move out of `self` before it is dropped.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: By type invariant `this.blocks` is valid. `badblocks_exit` is
        // safe to call during destruction and will properly clean up allocated
        // resources.
        unsafe { bindings::badblocks_exit(this.blocks.get()) };
    }
}

// SAFETY: `BadBlocks` can be safely dropped from other threads.
unsafe impl Send for BadBlocks {}

// SAFETY: All `BadBlocks` methods use internal synchronization.
unsafe impl Sync for BadBlocks {}

/// Status of a sector range after checking for bad blocks.
///
/// This enum represents the result of checking a sector range against the bad blocks
/// table. It distinguishes between ranges with no bad blocks, ranges with acknowledged
/// bad blocks, and ranges with unacknowledged bad blocks.
///
/// # Examples
///
/// ```rust
/// # use kernel::block::badblocks::{BadBlocks, BlockStatus};
/// # use kernel::prelude::*;
/// # use core::ops::{Range, RangeBounds};
/// # fn perform_io(range: impl RangeBounds<u64>) -> Result<()> { Ok(()) }
/// # fn remap_and_retry(io_range: impl RangeBounds<u64>, bad_range: Range<u64>) -> Result<()> { Ok(()) }
///
/// fn handle_io_request(bad_blocks: &BadBlocks, range: impl RangeBounds<u64> + Clone) -> Result<()> {
///     match bad_blocks.check(range.clone()) {
///         BlockStatus::None => {
///             // Safe to proceed with I/O - convert range to start/count for legacy function
///             perform_io(range)
///         },
///         BlockStatus::Acknowledged(bad_range) => {
///             pr_warn!("I/O overlaps acknowledged bad blocks: {:?}", bad_range);
///             // Attempt remapping or alternative strategy
///             remap_and_retry(range, bad_range)
///         },
///         BlockStatus::Unacknowledged(bad_range) => {
///             pr_err!("I/O overlaps unacknowledged bad blocks: {:?}", bad_range);
///             // Treat as serious error
///             Err(EIO)
///         },
///     }
/// }
/// # Ok::<(), kernel::error::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockStatus {
    /// No bad blocks found in the checked range.
    None,
    /// The range contains acknowledged bad blocks.
    ///
    /// The contained range represents the first bad block
    /// range encountered.
    Acknowledged(Range<u64>),
    /// The range contains unacknowledged bad blocks that need attention.
    ///
    /// The contained range represents the boundaries of the first bad block
    /// range encountered.
    Unacknowledged(Range<u64>),
}
