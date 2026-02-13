// SPDX-License-Identifier: GPL-2.0

use kernel::{
    block::{
        SECTOR_MASK,
        SECTOR_SHIFT, //
    },
    page::{
        SafePage,
        PAGE_SIZE, //
    },
    prelude::*,
    types::Owned,
    uapi::PAGE_SECTORS, //
};

const _CHEKC_STATUS_WIDTH: () = build_assert!((PAGE_SIZE >> SECTOR_SHIFT) <= 64);

pub(crate) struct NullBlockPage {
    page: Owned<SafePage>,
    status: u64,
    block_size: usize,
}

impl NullBlockPage {
    pub(crate) fn new(block_size: usize) -> Result<KBox<Self>> {
        Ok(KBox::new(
            Self {
                page: SafePage::alloc_page(GFP_NOIO | __GFP_ZERO)?,
                status: 0,
                block_size,
            },
            GFP_NOIO,
        )?)
    }

    pub(crate) fn set_occupied(&mut self, sector: u64) {
        let idx = sector & u64::from(SECTOR_MASK);
        self.status |= 1 << idx;
    }

    pub(crate) fn set_free(&mut self, sector: u64) {
        let idx = sector & u64::from(SECTOR_MASK);
        self.status &= !(1 << idx);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.status == 0
    }

    pub(crate) fn reset(&mut self) {
        self.status = 0;
    }

    pub(crate) fn is_full(&self) -> bool {
        let blocks_per_page = PAGE_SIZE >> self.block_size.trailing_zeros();
        let shift = PAGE_SECTORS as usize / blocks_per_page;

        for i in 0..blocks_per_page {
            if self.status & (1 << (i * shift)) == 0 {
                return false;
            }
        }

        true
    }

    pub(crate) fn page_mut(&mut self) -> &mut Owned<SafePage> {
        &mut self.page
    }

    pub(crate) fn page(&self) -> &Owned<SafePage> {
        &self.page
    }
}
