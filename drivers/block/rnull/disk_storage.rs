use super::HwQueueContext;
use core::pin::Pin;
use kernel::{
    block, new_spinlock,
    page::PAGE_SIZE,
    prelude::*,
    sync::{
        atomic::{ordering, Atomic},
        SpinLock, SpinLockGuard,
    },
    uapi::PAGE_SECTORS,
    xarray::{self, XArray},
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
    block_size: usize,
}

impl DiskStorage {
    pub(crate) fn new(cache_size: u64, block_size: usize) -> impl PinInit<Self, Error> {
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

    pub(crate) fn access<'a, 'b>(
        &'a self,
        tree_guard: &'a mut SpinLockGuard<'b, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'a mut SpinLockGuard<'b, HwQueueContext>,
    ) -> DiskStorageAccess<'a, 'b> {
        DiskStorageAccess::new(self, tree_guard, hw_data_guard)
    }

    pub(crate) fn lock(&self) -> SpinLockGuard<'_, Pin<KBox<TreeContainer>>> {
        self.trees.lock()
    }
}

pub(crate) struct DiskStorageAccess<'a, 'b> {
    cache_guard: xarray::Guard<'a, TreeNode>,
    disk_guard: xarray::Guard<'a, TreeNode>,
    hw_data_guard: &'a mut SpinLockGuard<'b, HwQueueContext>,
    disk_storage: &'a DiskStorage,
}

impl<'a, 'b> DiskStorageAccess<'a, 'b> {
    fn new(
        disk_storage: &'a DiskStorage,
        tree_guard: &'a mut SpinLockGuard<'b, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'a mut SpinLockGuard<'b, HwQueueContext>,
    ) -> Self {
        Self {
            cache_guard: tree_guard.cache_tree.lock(),
            disk_guard: tree_guard.disk_tree.lock(),
            hw_data_guard,
            disk_storage,
        }
    }
    fn to_index(sector: u64) -> usize {
        (sector >> block::PAGE_SECTORS_SHIFT) as usize
    }

    fn to_sector(index: usize) -> u64 {
        (index << block::PAGE_SECTORS_SHIFT) as u64
    }

    fn extract_cache_page(&mut self) -> Result<KBox<NullBlockPage>> {
        let cache_entry = self
            .cache_guard
            .find_next_entry_circular(
                self.disk_storage.next_flush_sector.load(ordering::Relaxed) as usize
            )
            .expect("Expected to find a page in the cache");

        let index = cache_entry.index();

        self.disk_storage
            .next_flush_sector
            .store(Self::to_sector(index).wrapping_add(1), ordering::Relaxed);

        self.disk_storage.cache_size_used.store(
            self.disk_storage.cache_size_used.load(ordering::Relaxed) - PAGE_SIZE as u64,
            ordering::Relaxed,
        );

        let page = match self.disk_guard.get_entry(index) {
            xarray::Entry::Vacant(disk_entry) => {
                disk_entry.insert(cache_entry.remove(), Some(&mut self.hw_data_guard.preload))?;
                self.hw_data_guard.page.take().ok_or(ENOSPC)?
            }
            xarray::Entry::Occupied(mut disk_entry) => {
                let mut page = if cache_entry.is_full() {
                    disk_entry.insert(cache_entry.remove())
                } else {
                    let mut src = cache_entry;
                    let mut offset = 0;
                    for _ in 0..PAGE_SECTORS {
                        src.page_mut().get_pin_mut().copy_to_page(
                            disk_entry.page_mut().get_pin_mut(),
                            offset,
                            block::SECTOR_SIZE as usize,
                        )?;
                        offset += block::SECTOR_SIZE as usize;
                    }
                    src.remove()
                };
                page.reset();
                page
            }
        };

        Ok(page)
    }

    fn get_cache_page(&mut self, sector: u64) -> Result<&mut NullBlockPage> {
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
                self.extract_cache_page()?
            };
            Ok(self
                .cache_guard
                .insert_entry(index, page, Some(&mut self.hw_data_guard.preload))
                .expect("Should be able to insert")
                .into_mut())
        }
    }

    fn get_disk_page(&mut self, sector: u64) -> Result<&mut NullBlockPage> {
        let index = Self::to_index(sector);

        let page = match self.disk_guard.get_entry(index) {
            xarray::Entry::Vacant(e) => e.insert(
                self.hw_data_guard
                    .page
                    .take()
                    .expect("Expected page to be available"),
                Some(&mut self.hw_data_guard.preload),
            )?,
            xarray::Entry::Occupied(e) => e.into_mut(),
        };

        Ok(page)
    }

    pub(crate) fn get_write_page(&mut self, sector: u64) -> Result<&mut NullBlockPage> {
        let page = if self.disk_storage.cache_size > 0 {
            self.get_cache_page(sector)?
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
            disk_tree <- XArray::new(xarray::AllocKind::Alloc),
            cache_tree <- XArray::new(xarray::AllocKind::Alloc),
        })
    }
}
