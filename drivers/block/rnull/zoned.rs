// SPDX-License-Identifier: GPL-2.0

use crate::{
    util::*,
    HwQueueContext, //
};
use kernel::{
    bindings,
    block::mq::{
        self,
        gen_disk::GenDiskRef, //
    },
    new_mutex,
    new_spinlock,
    prelude::*,
    sync::Mutex,
    sync::SpinLock,
    types::Owned, //
};

pub(crate) struct ZoneOptionsArgs {
    pub(crate) enable: bool,
    pub(crate) device_capacity_mib: u64,
    pub(crate) block_size_bytes: u32,
    pub(crate) zone_size_mib: u32,
    pub(crate) zone_capacity_mib: u32,
    pub(crate) zone_nr_conv: u32,
    pub(crate) zone_max_open: u32,
    pub(crate) zone_max_active: u32,
    pub(crate) zone_append_max_sectors: u32,
}

#[pin_data]
pub(crate) struct ZoneOptions {
    pub(crate) enabled: bool,
    zones: Pin<KBox<[Mutex<ZoneDescriptor>]>>,
    conventional_count: u32,
    pub(crate) size_sectors: u32,
    append_max_sectors: u32,
    max_open: u32,
    max_active: u32,
    #[pin]
    accounting: SpinLock<ZoneAccounting>,
}

impl ZoneOptions {
    pub(crate) fn new(args: ZoneOptionsArgs) -> Result<impl PinInit<Self, Error>> {
        let ZoneOptionsArgs {
            enable,
            device_capacity_mib,
            block_size_bytes,
            zone_size_mib,
            zone_capacity_mib,
            mut zone_nr_conv,
            mut zone_max_open,
            mut zone_max_active,
            zone_append_max_sectors,
        } = args;

        if !is_power_of_two(zone_size_mib) {
            return Err(EINVAL);
        }

        if zone_capacity_mib > zone_size_mib {
            return Err(EINVAL);
        }

        let zone_size_sectors = mib_to_sectors(zone_size_mib);
        let device_capacity_sectors = mib_to_sectors(device_capacity_mib);
        let zone_capacity_sectors = mib_to_sectors(zone_capacity_mib);
        let zone_count: u32 = (align_up(device_capacity_sectors, zone_size_sectors.into())
            >> zone_size_sectors.ilog2())
        .try_into()?;

        if zone_nr_conv >= zone_count {
            zone_nr_conv = zone_count - 1;
            pr_info!("changed the number of conventional zones to {zone_nr_conv}\n");
        }

        let zone_append_max_sectors =
            align_down(zone_append_max_sectors, bytes_to_sectors(block_size_bytes))
                .min(zone_capacity_sectors);

        let seq_zone_count = zone_count - zone_nr_conv;

        if zone_max_active >= seq_zone_count {
            zone_max_active = 0;
            pr_info!("zone_max_active limit disabled, limit >= zone count\n");
        }

        if zone_max_active != 0 && zone_max_open > zone_max_active {
            zone_max_open = zone_max_active;
            pr_info!("changed the maximum number of open zones to {zone_max_open}\n");
        } else if zone_max_open >= seq_zone_count {
            zone_max_open = 0;
            pr_info!("zone_max_open limit disabled, limit >= zone count\n");
        }

        Ok(try_pin_init!(Self {
            enabled: enable,
            zones: init_zone_descriptors(
                zone_size_sectors,
                zone_capacity_sectors,
                zone_count,
                zone_nr_conv,
            )?,
            size_sectors: zone_size_sectors,
            append_max_sectors: zone_append_max_sectors,
            max_open: zone_max_open,
            max_active: zone_max_active,
            accounting <- new_spinlock!(ZoneAccounting {
                implicit_open: 0,
                explicit_open: 0,
                closed: 0,
                start_zone: zone_nr_conv,
            }),
            conventional_count: zone_nr_conv,
        }))
    }
}

struct ZoneAccounting {
    implicit_open: u32,
    explicit_open: u32,
    closed: u32,
    start_zone: u32,
}

pub(crate) fn init_zone_descriptors(
    zone_size_sectors: u32,
    zone_capacity_sectors: u32,
    zone_count: u32,
    zone_nr_conv: u32,
) -> Result<Pin<KBox<[Mutex<ZoneDescriptor>]>>> {
    let zone_capacity_sectors = if zone_capacity_sectors == 0 {
        zone_size_sectors
    } else {
        zone_capacity_sectors
    };

    KBox::pin_slice(
        |i| {
            let sector = i as u64 * Into::<u64>::into(zone_size_sectors);
            new_mutex!(
                if i < zone_nr_conv.try_into().expect("Fewer than 2^32 zones") {
                    ZoneDescriptor {
                        start_sector: sector,
                        size_sectors: zone_size_sectors,
                        capacity_sectors: zone_size_sectors,
                        kind: ZoneType::Conventional,
                        write_pointer: sector + Into::<u64>::into(zone_size_sectors),
                        condition: ZoneCondition::NoWritePointer,
                    }
                } else {
                    ZoneDescriptor {
                        start_sector: sector,
                        size_sectors: zone_size_sectors,
                        capacity_sectors: zone_capacity_sectors,
                        kind: ZoneType::SequentialWriteRequired,
                        write_pointer: sector,
                        condition: ZoneCondition::Empty,
                    }
                }
            )
        },
        zone_count as usize,
        GFP_KERNEL,
    )
}

impl super::NullBlkDevice {
    pub(crate) fn handle_zoned_command(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        rq: &mut Owned<mq::Request<Self>>,
    ) -> Result {
        use mq::Command::*;
        match rq.command() {
            ZoneAppend | Write => self.zoned_write(hw_data, rq)?,
            ZoneReset | ZoneResetAll | ZoneOpen | ZoneClose | ZoneFinish => {
                self.zone_management(rq)?
            }
            _ => self.zoned_read(hw_data, rq)?,
        }

        Ok(())
    }

    fn zone_management(&self, rq: &mut Owned<mq::Request<Self>>) -> Result {
        if rq.command() == mq::Command::ZoneResetAll {
            for zone in self.zoned.zones_iter() {
                let mut zone = zone.lock();
                use ZoneCondition::*;
                match zone.condition {
                    Empty | ReadOnly | Offline => continue,
                    _ => self.zoned.reset_zone(&self.storage, &mut zone)?,
                }
            }

            return Ok(());
        }

        let zone = self.zoned.zone(rq.sector())?;
        let mut zone = zone.lock();

        if zone.condition == ZoneCondition::ReadOnly || zone.condition == ZoneCondition::Offline {
            return Err(EIO);
        }

        use mq::Command::*;
        match rq.command() {
            ZoneOpen => self.zoned.open_zone(&mut zone),
            ZoneClose => self.zoned.close_zone(&mut zone),
            ZoneReset => self.zoned.reset_zone(&self.storage, &mut zone),
            ZoneFinish => self.zoned.finish_zone(&mut zone),
            _ => Err(EIO),
        }
    }

    fn zoned_read(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        rq: &mut Owned<mq::Request<Self>>,
    ) -> Result {
        let zone = self.zoned.zone(rq.sector())?;
        let zone = zone.lock();
        if zone.condition == ZoneCondition::Offline {
            return Err(EINVAL);
        }

        zone.check_bounds_read(rq.sector(), rq.sectors())?;

        self.handle_regular_command(hw_data, rq)
    }

    fn zoned_write(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        rq: &mut Owned<mq::Request<Self>>,
    ) -> Result {
        let zone = self.zoned.zone(rq.sector())?;
        let mut zone = zone.lock();
        let append: bool = rq.command() == mq::Command::ZoneAppend;

        if zone.kind == ZoneType::Conventional {
            if append {
                return Err(EINVAL);
            }

            // NOTE: C driver does not check bounds on write.
            zone.check_bounds_write(rq.sector(), rq.sectors())?;

            let mut sectors = rq.sectors();
            self.handle_bad_blocks(rq, &mut sectors)?;
            return self.transfer(hw_data, rq, rq.command(), sectors);
        }

        // Check zoned write fits within zone
        if zone.write_pointer + Into::<u64>::into(rq.sectors())
            > zone.start_sector + Into::<u64>::into(zone.capacity_sectors)
        {
            return Err(EINVAL);
        }

        if append {
            if self.zoned.append_max_sectors == 0 {
                return Err(EINVAL);
            }
            rq.get_pin_mut().set_sector(zone.write_pointer);
        }

        // Check write pointer alignment
        if !append && rq.sector() != zone.write_pointer {
            return Err(EINVAL);
        }

        if zone.condition == ZoneCondition::Closed || zone.condition == ZoneCondition::Empty {
            if self.zoned.use_accounting() {
                let mut accounting = self.zoned.accounting.lock();
                self.zoned
                    .check_zone_resources(&mut accounting, &mut zone)?;

                if zone.condition == ZoneCondition::Closed {
                    accounting.closed -= 1;
                    accounting.implicit_open += 1;
                } else if zone.condition == ZoneCondition::Empty {
                    accounting.implicit_open += 1;
                }
            }

            zone.condition = ZoneCondition::ImplicitOpen;
        }

        let mut sectors = rq.sectors();
        self.handle_bad_blocks(rq, &mut sectors)?;

        if self.memory_backed {
            self.transfer(hw_data, rq, mq::Command::Write, sectors)?;
        }

        zone.write_pointer += Into::<u64>::into(sectors);
        if zone.write_pointer == zone.start_sector + Into::<u64>::into(zone.capacity_sectors) {
            if self.zoned.use_accounting() {
                let mut accounting = self.zoned.accounting.lock();

                if zone.condition == ZoneCondition::ExplicitOpen {
                    accounting.explicit_open -= 1;
                } else if zone.condition == ZoneCondition::ImplicitOpen {
                    accounting.implicit_open -= 1;
                }
            }

            zone.condition = ZoneCondition::Full;
        }

        Ok(())
    }

    pub(crate) fn report_zones_internal(
        disk: &GenDiskRef<Self>,
        sector: u64,
        nr_zones: u32,
        callback: impl Fn(&bindings::blk_zone, u32) -> Result,
    ) -> Result<u32> {
        let device = disk.queue_data();
        let first_zone = sector >> device.zoned.size_sectors.ilog2();

        let mut count = 0;

        for (i, zone) in device
            .zoned
            .zones
            .split_at(first_zone as usize)
            .1
            .iter()
            .take(nr_zones as usize)
            .enumerate()
        {
            let zone = zone.lock();
            let descriptor = bindings::blk_zone {
                start: zone.start_sector,
                len: zone.size_sectors.into(),
                wp: zone.write_pointer,
                capacity: zone.capacity_sectors.into(),
                type_: zone.kind as u8,
                cond: zone.condition as u8,
                ..bindings::blk_zone::zeroed()
            };
            drop(zone);
            callback(&descriptor, i as u32)?;

            count += 1;
        }

        Ok(count)
    }
}

impl ZoneOptions {
    fn zone_no(&self, sector: u64) -> usize {
        (sector >> self.size_sectors.ilog2()) as usize
    }

    pub(crate) fn zone(&self, sector: u64) -> Result<&Mutex<ZoneDescriptor>> {
        self.zones.get(self.zone_no(sector)).ok_or(EINVAL)
    }

    fn zones_iter(&self) -> impl Iterator<Item = &Mutex<ZoneDescriptor>> {
        self.zones.iter()
    }

    fn use_accounting(&self) -> bool {
        self.max_active != 0 || self.max_open != 0
    }

    fn try_close_implicit_open_zone(&self, accounting: &mut ZoneAccounting, sector: u64) -> Result {
        let skip = self.zone_no(sector) as u32;

        let it = Iterator::chain(
            self.zones[(accounting.start_zone as usize)..]
                .iter()
                .enumerate()
                .map(|(i, z)| (i + accounting.start_zone as usize, z)),
            self.zones[(self.conventional_count as usize)..(accounting.start_zone as usize)]
                .iter()
                .enumerate()
                .map(|(i, z)| (i + self.conventional_count as usize, z)),
        )
        .filter(|(i, _)| *i != skip as usize);

        for (index, zone) in it {
            let mut zone = zone.lock();
            if zone.condition == ZoneCondition::ImplicitOpen {
                accounting.implicit_open -= 1;

                let index_u32: u32 = index.try_into()?;
                let next_zone: u32 = index_u32 + 1;
                accounting.start_zone = if next_zone == self.zones.len().try_into()? {
                    self.conventional_count
                } else {
                    next_zone
                };

                if zone.write_pointer == zone.start_sector {
                    zone.condition = ZoneCondition::Empty;
                } else {
                    zone.condition = ZoneCondition::Closed;
                    accounting.closed += 1;
                }
                return Ok(());
            }
        }

        Err(EINVAL)
    }

    fn open_zone(&self, zone: &mut ZoneDescriptor) -> Result {
        if zone.kind == ZoneType::Conventional {
            return Err(EINVAL);
        }

        use ZoneCondition::*;
        match zone.condition {
            ExplicitOpen => return Ok(()),
            Empty | ImplicitOpen | Closed => (),
            _ => return Err(EIO),
        }

        if self.use_accounting() {
            let mut accounting = self.accounting.lock();
            match zone.condition {
                Empty => {
                    self.check_zone_resources(&mut accounting, zone)?;
                }
                ImplicitOpen => {
                    accounting.implicit_open -= 1;
                }
                Closed => {
                    self.check_zone_resources(&mut accounting, zone)?;
                    accounting.closed -= 1;
                }
                _ => (),
            }

            accounting.explicit_open += 1;
        }

        zone.condition = ExplicitOpen;
        Ok(())
    }

    fn check_zone_resources(
        &self,
        accounting: &mut ZoneAccounting,
        zone: &mut ZoneDescriptor,
    ) -> Result {
        match zone.condition {
            ZoneCondition::Empty => {
                self.check_active_zones(accounting)?;
                self.check_open_zones(accounting, zone.start_sector)
            }
            ZoneCondition::Closed => self.check_open_zones(accounting, zone.start_sector),
            _ => Err(EIO),
        }
    }

    fn check_open_zones(&self, accounting: &mut ZoneAccounting, sector: u64) -> Result {
        if self.max_open == 0 {
            return Ok(());
        }

        if self.max_open > accounting.explicit_open + accounting.implicit_open {
            return Ok(());
        }

        if accounting.implicit_open > 0 {
            self.check_active_zones(accounting)?;
            return self.try_close_implicit_open_zone(accounting, sector);
        }

        Err(EBUSY)
    }

    fn check_active_zones(&self, accounting: &mut ZoneAccounting) -> Result {
        if self.max_active == 0 {
            return Ok(());
        }

        if self.max_active > accounting.implicit_open + accounting.explicit_open + accounting.closed
        {
            return Ok(());
        }

        Err(EBUSY)
    }

    fn close_zone(&self, zone: &mut ZoneDescriptor) -> Result {
        if zone.kind == ZoneType::Conventional {
            return Err(EINVAL);
        }

        use ZoneCondition::*;
        match zone.condition {
            Closed => return Ok(()),
            ImplicitOpen | ExplicitOpen => (),
            _ => return Err(EIO),
        }

        if self.use_accounting() {
            let mut accounting = self.accounting.lock();
            match zone.condition {
                ImplicitOpen => accounting.implicit_open -= 1,
                ExplicitOpen => accounting.explicit_open -= 1,
                _ => (),
            }

            if zone.write_pointer > zone.start_sector {
                accounting.closed += 1;
            }
        }

        if zone.write_pointer == zone.start_sector {
            zone.condition = Empty;
        } else {
            zone.condition = Closed;
        }

        Ok(())
    }

    fn finish_zone(&self, zone: &mut ZoneDescriptor) -> Result {
        if zone.kind == ZoneType::Conventional {
            return Err(EINVAL);
        }

        if self.use_accounting() {
            let mut accounting = self.accounting.lock();

            use ZoneCondition::*;
            match zone.condition {
                Full => return Ok(()),
                Empty => {
                    self.check_zone_resources(&mut accounting, zone)?;
                }
                ImplicitOpen => accounting.implicit_open -= 1,
                ExplicitOpen => accounting.explicit_open -= 1,
                Closed => {
                    self.check_zone_resources(&mut accounting, zone)?;
                    accounting.closed -= 1;
                }
                _ => return Err(EIO),
            }
        }

        zone.condition = ZoneCondition::Full;
        zone.write_pointer = zone.start_sector + Into::<u64>::into(zone.size_sectors);

        Ok(())
    }

    fn reset_zone(
        &self,
        storage: &crate::disk_storage::DiskStorage,
        zone: &mut ZoneDescriptor,
    ) -> Result {
        if zone.kind == ZoneType::Conventional {
            return Err(EINVAL);
        }

        if self.use_accounting() {
            let mut accounting = self.accounting.lock();

            use ZoneCondition::*;
            match zone.condition {
                ImplicitOpen => accounting.implicit_open -= 1,
                ExplicitOpen => accounting.explicit_open -= 1,
                Closed => accounting.closed -= 1,
                Empty | Full => (),
                _ => return Err(EIO),
            }
        }

        zone.condition = ZoneCondition::Empty;
        zone.write_pointer = zone.start_sector;

        storage.discard(zone.start_sector, zone.size_sectors);

        Ok(())
    }

    fn set_zone_condition(
        &self,
        storage: &crate::disk_storage::DiskStorage,
        zone: &mut ZoneDescriptor,
        condition: ZoneCondition,
    ) -> Result {
        if zone.condition == condition {
            zone.condition = ZoneCondition::Empty;
            zone.write_pointer = zone.start_sector;
            storage.discard(zone.start_sector, zone.size_sectors);
        } else {
            if matches!(
                zone.condition,
                ZoneCondition::ReadOnly | ZoneCondition::Offline
            ) {
                self.finish_zone(zone)?;
            }

            zone.condition = ZoneCondition::Offline;
            zone.write_pointer = u64::MAX;
        }
        Ok(())
    }
    pub(crate) fn offline_zone(
        &self,
        storage: &crate::disk_storage::DiskStorage,
        zone: &mut ZoneDescriptor,
    ) -> Result {
        self.set_zone_condition(storage, zone, ZoneCondition::Offline)
    }

    pub(crate) fn read_only_zone(
        &self,
        storage: &crate::disk_storage::DiskStorage,
        zone: &mut ZoneDescriptor,
    ) -> Result {
        self.set_zone_condition(storage, zone, ZoneCondition::ReadOnly)
    }
}

pub(crate) struct ZoneDescriptor {
    start_sector: u64,
    size_sectors: u32,
    pub(crate) kind: ZoneType,
    capacity_sectors: u32,
    write_pointer: u64,
    condition: ZoneCondition,
}

impl ZoneDescriptor {
    fn check_bounds_write(&self, sector: u64, sectors: u32) -> Result {
        if sector + Into::<u64>::into(sectors)
            > self.start_sector + Into::<u64>::into(self.capacity_sectors)
        {
            Err(EIO)
        } else {
            Ok(())
        }
    }

    fn check_bounds_read(&self, sector: u64, sectors: u32) -> Result {
        if sector + Into::<u64>::into(sectors) > self.write_pointer {
            Err(EIO)
        } else {
            Ok(())
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum ZoneType {
    Conventional = bindings::blk_zone_type_BLK_ZONE_TYPE_CONVENTIONAL,
    SequentialWriteRequired = bindings::blk_zone_type_BLK_ZONE_TYPE_SEQWRITE_REQ,
    #[expect(dead_code)]
    SequentialWritePreferred = bindings::blk_zone_type_BLK_ZONE_TYPE_SEQWRITE_PREF,
}

impl ZoneType {
    #[expect(dead_code)]
    fn as_raw(self) -> u32 {
        self as u32
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
enum ZoneCondition {
    NoWritePointer = bindings::blk_zone_cond_BLK_ZONE_COND_NOT_WP,
    Empty = bindings::blk_zone_cond_BLK_ZONE_COND_EMPTY,
    ImplicitOpen = bindings::blk_zone_cond_BLK_ZONE_COND_IMP_OPEN,
    ExplicitOpen = bindings::blk_zone_cond_BLK_ZONE_COND_EXP_OPEN,
    Closed = bindings::blk_zone_cond_BLK_ZONE_COND_CLOSED,
    Full = bindings::blk_zone_cond_BLK_ZONE_COND_FULL,
    ReadOnly = bindings::blk_zone_cond_BLK_ZONE_COND_READONLY,
    Offline = bindings::blk_zone_cond_BLK_ZONE_COND_OFFLINE,
}

impl ZoneCondition {
    #[expect(dead_code)]
    fn as_raw(self) -> u32 {
        self as u32
    }
}
