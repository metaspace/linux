// SPDX-License-Identifier: GPL-2.0
use kernel::prelude::*;

impl_flags! {
    #[derive(Debug, Clone, Default, Copy, PartialEq, Eq)]
    pub struct Features(u32);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Feature {
        /// Supports a volatile write cache.
        WriteCache = bindings::BLK_FEAT_WRITE_CACHE,

        /// Supports passing on the FUA bit.
        ForcedUnitAccess = bindings::BLK_FEAT_FUA,

        /// Rotational device (hard drive or floppy).
        Rotational = bindings::BLK_FEAT_ROTATIONAL,

        /// Contributes to the random number pool.
        AddRandom = bindings::BLK_FEAT_ADD_RANDOM,

        /// Do disk/partitions IO accounting.
        IoStat = bindings::BLK_FEAT_IO_STAT,

        /// Don't modify data until writeback is done.
        StableWrites = bindings::BLK_FEAT_STABLE_WRITES,

        /// Always completes in submit context.
        Synchronous = bindings::BLK_FEAT_SYNCHRONOUS,

        /// Supports REQ_NOWAIT.
        Nowait = bindings::BLK_FEAT_NOWAIT,

        /// supports DAX.
        Dax = bindings::BLK_FEAT_DAX,

        /// supports I/O polling.
        Poll = bindings::BLK_FEAT_POLL,

        /// Is a zoned device.
        Zoned = bindings::BLK_FEAT_ZONED,

        /// supports PCI(e) p2p requests. 
        PciP2Pdma = bindings::BLK_FEAT_PCI_P2PDMA,

        /// Skip this queue in blk_mq_(un)quiesce_tagset.
        SkipTagsetQuiesce = bindings::BLK_FEAT_SKIP_TAGSET_QUIESCE,

        /// undocumented magic for bcache
        RaidPartialStripesExpensive = bindings::BLK_FEAT_RAID_PARTIAL_STRIPES_EXPENSIVE,

        /// Atomic writes enabled.
        AtomicWrites = bindings::BLK_FEAT_ATOMIC_WRITES,
    }
}
