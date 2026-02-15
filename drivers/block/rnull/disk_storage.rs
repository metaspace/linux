// SPDX-License-Identifier: GPL-2.0

use super::HwQueueContext;
use crate::util::*;
use core::pin::Pin;
use kernel::{
    block,
    new_spinlock,
    new_xarray,
    page::PAGE_SIZE,
    prelude::*,
    sync::{
        atomic::{
            ordering,
            Atomic, //
        },
        SpinLock,
        SpinLockGuard, //
    },
    uapi::PAGE_SECTORS,
    xarray::{
        self,
        XArray,
        XArraySheaf, //
    }, //
};
pub(crate) use page::NullBlockPage;

mod page;

#[pin_data]
pub(crate) struct DiskStorage {
    // TODO: Get rid of this pointer indirection.
    #[pin]
    trees: SpinLock<Pin<KBox<TreeContainer>>>,
    cache_size: u64,
    cache_size_used: Atomic<u64>,
    next_flush_sector: Atomic<u64>,
    block_size: u32,
}

impl DiskStorage {
    pub(crate) fn new(cache_size: u64, block_size: u32) -> impl PinInit<Self, Error> {
        try_pin_init!( Self {
            // TODO: Get rid of the box
            // https://git.kernel.org/pub/scm/linux/kernel/git/boqun/linux.git/commit/?h=locking&id=a5d84cafb3e253a11d2e078902c5b090be2f4227
            trees <- new_spinlock!(KBox::pin_init(TreeContainer::new(), GFP_KERNEL)?),
            cache_size,
            cache_size_used: Atomic::new(0),
            next_flush_sector: Atomic::new(0),
            block_size
        })
    }

    pub(crate) fn access<'a, 'b, 'c>(
        &'a self,
        tree_guard: &'a mut SpinLockGuard<'b, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'a mut SpinLockGuard<'b, HwQueueContext>,
        sheaf: Option<XArraySheaf<'c>>,
    ) -> DiskStorageAccess<'a, 'b, 'c> {
        DiskStorageAccess::new(self, tree_guard, hw_data_guard, sheaf)
    }

    pub(crate) fn lock(&self) -> SpinLockGuard<'_, Pin<KBox<TreeContainer>>> {
        self.trees.lock()
    }

    pub(crate) fn discard(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        mut sector: u64,
        sectors: u32,
    ) {
        let mut tree_guard = self.lock();
        let mut hw_data_guard = hw_data.lock();

        let mut access = self.access(&mut tree_guard, &mut hw_data_guard, None);

        let mut remaining_bytes = sectors_to_bytes(sectors);

        while remaining_bytes > 0 {
            access.free_sector(sector);
            let processed = remaining_bytes.min(self.block_size);
            sector += Into::<u64>::into(bytes_to_sectors(processed));
            remaining_bytes -= processed;
        }
    }

    pub(crate) fn flush(&self, hw_data: &Pin<&SpinLock<HwQueueContext>>) {
        let mut tree_guard = self.lock();
        let mut hw_data_guard = hw_data.lock();
        let mut access = self.access(&mut tree_guard, &mut hw_data_guard, None);
        access.flush()
    }

    pub(crate) fn cache_enabled(&self) -> bool {
        self.cache_size > 0
    }
}

pub(crate) struct DiskStorageAccess<'a, 'b, 'c> {
    cache_guard: xarray::Guard<'a, TreeNode>,
    disk_guard: xarray::Guard<'a, TreeNode>,
    hw_data_guard: &'a mut SpinLockGuard<'b, HwQueueContext>,
    disk_storage: &'a DiskStorage,
    pub(crate) sheaf: Option<XArraySheaf<'c>>,
}

impl<'a, 'b, 'c> DiskStorageAccess<'a, 'b, 'c> {
    fn new(
        disk_storage: &'a DiskStorage,
        tree_guard: &'a mut SpinLockGuard<'b, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'a mut SpinLockGuard<'b, HwQueueContext>,
        sheaf: Option<XArraySheaf<'c>>,
    ) -> Self {
        Self {
            cache_guard: tree_guard.cache_tree.lock(),
            disk_guard: tree_guard.disk_tree.lock(),
            hw_data_guard,
            disk_storage,
            sheaf,
        }
    }
    fn to_index(sector: u64) -> usize {
        (sector >> block::PAGE_SECTORS_SHIFT) as usize
    }

    fn to_sector(index: usize) -> u64 {
        (index << block::PAGE_SECTORS_SHIFT) as u64
    }

    fn extract_cache_page(&mut self) -> Option<KBox<NullBlockPage>> {
        let cache_entry = self.cache_guard.find_next_entry_circular(
            self.disk_storage.next_flush_sector.load(ordering::Relaxed) as usize,
        )?;

        let index = cache_entry.index();

        self.disk_storage
            .next_flush_sector
            .store(Self::to_sector(index).wrapping_add(1), ordering::Relaxed);

        self.disk_storage.cache_size_used.store(
            self.disk_storage.cache_size_used.load(ordering::Relaxed) - PAGE_SIZE as u64,
            ordering::Relaxed,
        );

        let page = match self.disk_guard.entry(index) {
            xarray::Entry::Vacant(disk_entry) => {
                disk_entry
                    .insert(cache_entry.remove(), self.sheaf.as_mut())
                    .expect("Preload is set up to allow insert without failure");
                self.hw_data_guard
                    .page
                    .take()
                    .expect("Preload has allocated for us")
            }
            xarray::Entry::Occupied(mut disk_entry) => {
                let mut page = if cache_entry.is_full() {
                    disk_entry.insert(cache_entry.remove())
                } else {
                    let mut src = cache_entry;
                    let mut offset = 0;
                    for _ in 0..PAGE_SECTORS {
                        src.page_mut()
                            .get_pin_mut()
                            .copy_to_page(
                                disk_entry.page_mut().get_pin_mut(),
                                offset,
                                block::SECTOR_SIZE as usize,
                            )
                            .expect("Write to succeed");
                        offset += block::SECTOR_SIZE as usize;
                    }
                    src.remove()
                };
                page.reset();
                page
            }
        };

        Some(page)
    }

    fn flush(&mut self) {
        if self.disk_storage.cache_size > 0 {
            while let Some(page) = self.extract_cache_page() {
                drop(page);
            }
        }
    }

    fn get_or_alloc_cache_page(&mut self, sector: u64) -> Result<&mut NullBlockPage> {
        let index = Self::to_index(sector);

        if self.cache_guard.contains_index(index) {
            Ok(self.cache_guard.get_mut(index).expect("Index is present"))
        } else {
            let page = if self.disk_storage.cache_size_used.load(ordering::Relaxed)
                < self.disk_storage.cache_size
            {
                self.hw_data_guard
                    .page
                    .take()
                    .expect("Expected to have a page available")
            } else {
                self.extract_cache_page()
                    .expect("Expected to find a page in the cache")
            };
            Ok(self
                .cache_guard
                .insert_entry(index, page, self.sheaf.as_mut())
                .expect("Should be able to insert")
                .into_mut())
        }
    }

    pub(crate) fn get_cache_page(&mut self, sector: u64) -> Option<&mut NullBlockPage> {
        let index = Self::to_index(sector);

        self.cache_guard.get_mut(index)
    }

    fn get_disk_page(&mut self, sector: u64) -> Result<&mut NullBlockPage> {
        let index = Self::to_index(sector);

        let page = match self.disk_guard.entry(index) {
            xarray::Entry::Vacant(e) => e.insert(
                self.hw_data_guard
                    .page
                    .take()
                    .expect("Expected page to be available"),
                self.sheaf.as_mut(),
            )?,
            xarray::Entry::Occupied(e) => e.into_mut(),
        };

        Ok(page)
    }

    pub(crate) fn get_write_page(
        &mut self,
        sector: u64,
        bypass_cache: bool,
    ) -> Result<&mut NullBlockPage> {
        let page = if self.disk_storage.cache_size > 0 && !bypass_cache {
            self.get_or_alloc_cache_page(sector)?
        } else {
            self.get_disk_page(sector)?
        };

        Ok(page)
    }

    pub(crate) fn get_read_page(&self, sector: u64) -> Option<&NullBlockPage> {
        let index = Self::to_index(sector);
        if self.disk_storage.cache_size > 0 {
            self.cache_guard
                .get(index)
                .or_else(|| self.disk_guard.get(index))
        } else {
            self.disk_guard.get(index)
        }
    }

    fn free_sector_tree(tree_access: &mut xarray::Guard<'_, TreeNode>, sector: u64) {
        let index = Self::to_index(sector);
        if let Some(page) = tree_access.get_mut(index) {
            page.set_free(sector);

            if page.is_empty() {
                tree_access.remove(index);
            }
        }
    }

    pub(crate) fn free_sector(&mut self, sector: u64) {
        if self.disk_storage.cache_size > 0 {
            Self::free_sector_tree(&mut self.cache_guard, sector);
        }

        Self::free_sector_tree(&mut self.disk_guard, sector);
    }
}

type Tree = XArray<TreeNode>;
type TreeNode = KBox<NullBlockPage>;

#[pin_data]
pub(crate) struct TreeContainer {
    #[pin]
    disk_tree: Tree,
    #[pin]
    cache_tree: Tree,
}

impl TreeContainer {
    fn new() -> impl PinInit<Self> {
        pin_init!(TreeContainer {
            disk_tree <- new_xarray!(xarray::AllocKind::Alloc),
            cache_tree <- new_xarray!(xarray::AllocKind::Alloc),
        })
    }
}
