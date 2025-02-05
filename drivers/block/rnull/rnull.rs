// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.
//!
//! Supported features:
//!
//! - blk-mq interface
//! - direct completion
//! - block size 4k
//!
//! The driver is not configurable.

use kernel::{
    alloc::flags,
    block::mq::{
        self,
        gen_disk::{self, GenDisk},
        Operations, TagSet,
    },
    error::Result,
    new_mutex, pr_info,
    prelude::*,
    sync::{Arc, Mutex},
    types::Owned,
};

module! {
    type: NullBlkModule,
    name: "rnull_mod",
    authors: ["Andreas Hindborg"],
    description: "Rust implementation of the C null block driver",
    license: "GPL v2",
}

#[pin_data]
struct NullBlkModule {
    #[pin]
    _disk: Mutex<GenDisk<NullBlkDevice>>,
}

impl kernel::InPlaceModule for NullBlkModule {
    fn init(_module: &'static ThisModule) -> impl PinInit<Self, Error> {
        pr_info!("Rust null_blk loaded\n");

        // Use a immediately-called closure as a stable `try` block
        let disk = /* try */ (|| {
            let tagset = Arc::pin_init(TagSet::new(1, 256, 1), flags::GFP_KERNEL)?;

            gen_disk::GenDiskBuilder::new()
                .capacity_sectors(4096 << 11)
                .logical_block_size(4096)?
                .physical_block_size(4096)?
                .rotational(false)
                .build(format_args!("rnullb{}", 0), tagset)
        })();

        try_pin_init!(Self {
            _disk <- new_mutex!(disk?, "nullb:disk"),
        })
    }
}

struct NullBlkDevice;

#[vtable]
impl Operations for NullBlkDevice {
    #[inline(always)]
    fn queue_rq(rq: Owned<mq::Request<Self>>, _is_last: bool) -> Result {
        rq.end_ok();
        Ok(())
    }

    fn commit_rqs() {}
}
