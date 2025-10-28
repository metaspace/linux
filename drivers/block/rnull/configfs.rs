// SPDX-License-Identifier: GPL-2.0

use super::{DiskStorage, NullBlkDevice, THIS_MODULE};
use core::{
    fmt::{Display, Write},
    str::FromStr,
};
use kernel::{
    bindings,
    block::{
        badblocks::BadBlocks,
        mq::gen_disk::{GenDisk, GenDiskBuilder},
    },
    c_str,
    configfs::{self, AttributeOperations},
    configfs_attrs, new_mutex,
    page::PAGE_SIZE,
    prelude::*,
    str::{kstrtobool_bytes, CString},
    sync::{Arc, Mutex},
    time,
};
use pin_init::PinInit;

fn show_field<T: core::fmt::Display>(value: T, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
    let mut writer = kernel::str::Formatter::new(page);
    writer.write_fmt(fmt!("{}\n", value))?;
    Ok(writer.bytes_written())
}

fn store_with_power_check<F>(this: &DeviceConfig, page: &[u8], store_fn: F) -> Result
where
    F: FnOnce(&DeviceConfig, &[u8]) -> Result,
{
    if this.data.lock().powered {
        return Err(EBUSY);
    }
    store_fn(this, page)
}

fn store_number_with_power_check<F, T>(this: &DeviceConfig, page: &[u8], store_fn: F) -> Result
where
    F: FnOnce(&DeviceConfig, T) -> Result,
    T: FromStr,
{
    if this.data.lock().powered {
        return Err(EBUSY);
    }

    let text = core::str::from_utf8(page)?.trim();
    let value = text.parse::<T>().map_err(|_| EINVAL)?;

    store_fn(this, value)
}

macro_rules! configfs_attribute {
    (
        $type:ty,
        $id:literal,
        show: |$show_this:ident, $show_page:ident| $show_block:expr,
        store: |$store_this:ident, $store_page:ident| $store_block:expr
    ) => {
        #[vtable]
        impl configfs::AttributeOperations<$id> for $type {
            type Data = $type;

            fn show($show_this: &$type, $show_page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
                $show_block
            }

            fn store($store_this: &$type, $store_page: &[u8]) -> Result {
                $store_block
            }
        }
    };
}

// Specialized macro for simple boolean fields that just store kstrtobool_bytes result.
macro_rules! configfs_simple_bool_field {
    ($type:ty, $id:literal, $field:ident) => {
        configfs_attribute!($type, $id,
            show: |this, page| show_field(this.data.lock().$field, page),
            store: |this, page| store_with_power_check(this, page, |this, page| {
                this.data.lock().$field = kstrtobool_bytes(page)?;
                Ok(())
            })
        );
    };
}

// Specialized macro for simple numeric fields that just parse and assign
macro_rules! configfs_simple_field {
    // Simple direct assignment
    ($type:ty, $id:literal, $field:ident, $field_type:ty) => {
        configfs_attribute!($type, $id,
            show: |this, page| show_field(this.data.lock().$field, page),
            store: |this, page| store_number_with_power_check(this, page, |this, value: $field_type| {
                this.data.lock().$field = value;
                Ok(())
            })
        );
    };
    // With infallible conversion expression (direct value)
    ($type:ty, $id:literal, $field:ident, $field_type:ty, into $convert:expr) => {
        configfs_attribute!($type, $id,
                            show: |this, page| show_field(this.data.lock().$field, page),
                            store: |this, page| store_number_with_power_check(this, page, |this, value: $field_type| {
                                this.data.lock().$field = $convert(value);
                                Ok(())
                            })
        );
    };
    ($type:ty, $id:literal, $field:ident, $field_type:ty, check $check:expr) => {
        configfs_attribute!($type, $id,
            show: |this, page| show_field(this.data.lock().$field, page),
            store: |this, page| store_number_with_power_check(this, page, |this, value: $field_type| {
                $check(value)?;
                this.data.lock().$field = value;
                Ok(())
            })
        );
    };
}

pub(crate) fn subsystem() -> impl PinInit<kernel::configfs::Subsystem<Config>, Error> {
    let item_type = configfs_attrs! {
        container: configfs::Subsystem<Config>,
        data: Config,
        child: DeviceConfig,
        attributes: [
            features: 0,
        ],
    };

    kernel::configfs::Subsystem::new(c_str!("rnull"), item_type, try_pin_init!(Config {}))
}

#[pin_data]
pub(crate) struct Config {}

#[vtable]
impl AttributeOperations<0> for Config {
    type Data = Config;

    fn show(_this: &Config, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
        let mut writer = kernel::str::Formatter::new(page);
        writer.write_str(
            "blocksize,size,rotational,irqmode,completion_nsec,memory_backed\
             submit_queues,use_per_node_hctx,discard\n",
        )?;
        Ok(writer.bytes_written())
    }
}

#[vtable]
impl configfs::GroupOperations for Config {
    type Child = DeviceConfig;

    fn make_group(
        &self,
        name: &CStr,
    ) -> Result<impl PinInit<configfs::Group<DeviceConfig>, Error>> {
        let item_type = configfs_attrs! {
            container: configfs::Group<DeviceConfig>,
            data: DeviceConfig,
            attributes: [
                // Named for compatibility with C null_blk
                power: 0,
                blocksize: 1,
                rotational: 2,
                size: 3,
                irqmode: 4,
                completion_nsec: 5,
                memory_backed: 6,
                submit_queues: 7,
                use_per_node_hctx: 8,
                home_node: 9,
                discard: 10,
                no_sched:11,
                badblocks: 12,
                badblocks_once: 13,
                badblocks_partial_io: 14,
                cache_size_mib: 15,
                mbps: 16,
            ],
        };

        let block_size = 4096;
        Ok(configfs::Group::new(
            name.try_into()?,
            item_type,
            // TODO: cannot coerce new_mutex!() to impl PinInit<_, Error>, so put mutex inside
            try_pin_init!( DeviceConfig {
                data <- new_mutex!(DeviceConfigInner {
                    powered: false,
                    block_size,
                    rotational: false,
                    disk: None,
                    capacity_mib: 4096,
                    irq_mode: IRQMode::None,
                    completion_time: time::Delta::ZERO,
                    name: name.try_into()?,
                    memory_backed: false,
                    submit_queues: 1,
                    home_node: bindings::NUMA_NO_NODE,
                    discard: false,
                    no_sched: false,
                    bad_blocks: Arc::pin_init(BadBlocks::new(false), GFP_KERNEL)?,
                    bad_blocks_once: false,
                    bad_blocks_partial_io: false,
                    disk_storage: Arc::pin_init(DiskStorage::new(0, block_size as usize), GFP_KERNEL)?,
                    cache_size_mib: 0,
                    mbps: 0,
                }),
            }),
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum IRQMode {
    None,
    Soft,
    Timer,
}

impl TryFrom<u8> for IRQMode {
    type Error = kernel::error::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Soft),
            2 => Ok(Self::Timer),
            _ => Err(EINVAL),
        }
    }
}

impl core::str::FromStr for IRQMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let value: u8 = s.parse().map_err(|_| EINVAL)?;
        Ok(value.try_into()?)
    }
}

impl Display for IRQMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::None => f.write_str("0")?,
            Self::Soft => f.write_str("1")?,
            Self::Timer => f.write_str("2")?,
        }
        Ok(())
    }
}

#[pin_data]
pub(crate) struct DeviceConfig {
    #[pin]
    data: Mutex<DeviceConfigInner>,
}

#[pin_data]
struct DeviceConfigInner {
    powered: bool,
    name: CString,
    block_size: u32,
    rotational: bool,
    capacity_mib: u64,
    irq_mode: IRQMode,
    completion_time: time::Delta,
    disk: Option<Arc<GenDisk<NullBlkDevice>>>,
    memory_backed: bool,
    submit_queues: u32,
    home_node: i32,
    discard: bool,
    no_sched: bool,
    bad_blocks: Arc<BadBlocks>,
    bad_blocks_once: bool,
    bad_blocks_partial_io: bool,
    cache_size_mib: u64,
    disk_storage: Arc<DiskStorage>,
    mbps: u32,
}

#[vtable]
impl configfs::AttributeOperations<0> for DeviceConfig {
    type Data = DeviceConfig;

    fn show(this: &DeviceConfig, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
        let mut writer = kernel::str::Formatter::new(page);

        if this.data.lock().powered {
            writer.write_str("1\n")?;
        } else {
            writer.write_str("0\n")?;
        }

        Ok(writer.bytes_written())
    }

    fn store(this: &DeviceConfig, page: &[u8]) -> Result {
        let power_op = kstrtobool_bytes(page)?;
        let mut guard = this.data.lock();

        if !guard.powered && power_op {
            guard.disk = Some(NullBlkDevice::new(
                &guard.name,
                guard.block_size,
                guard.rotational,
                guard.capacity_mib,
                guard.irq_mode,
                guard.completion_time,
                guard.memory_backed,
                guard.submit_queues,
                guard.home_node,
                guard.discard,
                guard.no_sched,
                guard.bad_blocks.clone(),
                guard.bad_blocks_once,
                guard.bad_blocks_partial_io,
                guard.disk_storage.clone(),
                (guard.mbps as u64) * 2u64.pow(20),
            )?);
            guard.powered = true;
        } else if guard.powered && !power_op {
            drop(guard.disk.take());
            guard.powered = false;
        }

        Ok(())
    }
}

configfs_simple_field!(DeviceConfig, 1, block_size, u32, check GenDiskBuilder::validate_block_size);
configfs_simple_bool_field!(DeviceConfig, 2, rotational);
configfs_simple_field!(DeviceConfig, 3, capacity_mib, u64);
configfs_simple_field!(DeviceConfig, 4, irq_mode, IRQMode);
configfs_simple_field!(DeviceConfig, 5, completion_time, i64, into time::Delta::from_nanos);

configfs_attribute!(DeviceConfig, 6,
    show: |this, page| show_field(this.data.lock().memory_backed, page),
    store: |this, page| store_with_power_check(this, page, |this, page| {
        let value = kstrtobool_bytes(page)?;
        let mut guard = this.data.lock();
        guard.discard &= value;
        guard.memory_backed = value;
        Ok(())
    })
);

configfs_simple_field!(
    DeviceConfig,
    7,
    submit_queues,
    u32,
    check | value | {
        if value == 0 || value > kernel::num_possible_cpus() {
            Err(kernel::error::code::EINVAL)
        } else {
            Ok(())
        }
    }
);

configfs_attribute!(DeviceConfig, 8,
    show: |this, page| show_field(this.data.lock().submit_queues == kernel::num_online_nodes(), page),
    store: |this, page| store_with_power_check(this, page, |this, page| {
        let value = core::str::from_utf8(page)?
            .trim()
            .parse::<u8>()
            .map_err(|_| kernel::error::code::EINVAL)?
            != 0;

        if value {
            this.data.lock().submit_queues *= kernel::num_online_nodes();
        }
        Ok(())
    })
);

configfs_simple_field!(
    DeviceConfig,
    9,
    home_node,
    i32,
    check | value | {
        if value == 0 || value >= kernel::num_online_nodes().try_into()? {
            Err(kernel::error::code::EINVAL)
        } else {
            Ok(())
        }
    }
);

configfs_attribute!(DeviceConfig, 10,
    show: |this, page| show_field(this.data.lock().discard, page),
    store: |this, page| store_with_power_check(this, page, |this, page| {
        if !this.data.lock().memory_backed {
            return Err(EINVAL);
        }
        this.data.lock().discard = kstrtobool_bytes(page)?;
        Ok(())
    })
);

configfs_simple_bool_field!(DeviceConfig, 11, no_sched);

configfs_attribute!(DeviceConfig, 12,
    show: |this, page| {
        let ret = this.data.lock().bad_blocks.show(page, false);
        if ret < 0 {
            Err(Error::from_errno(ret as c_int))
        } else {
            Ok(ret as usize)
        }
    },
    store: |this, page| {
        // This attribute can be set while device is powered.
        for line in core::str::from_utf8(page)?.lines() {
            let mut chars = line.chars();
            match chars.next() {
                Some(sign @ '+' | sign @ '-') => {
                    if let Some((start, end)) = chars.as_str().split_once('-') {
                        let start: u64 = start.parse().map_err(|_| EINVAL)?;
                        let end: u64 = end.parse().map_err(|_| EINVAL)?;

                        if start > end {
                            return Err(EINVAL);
                        }

                        this.data.lock().bad_blocks.enable();

                        if sign == '+' {
                            this.data
                                .lock()
                                .bad_blocks
                                .set_bad(start..=end, true)?;
                        } else {
                            this.data
                                .lock()
                                .bad_blocks
                                .set_good(start..=end)?;
                        }
                    }
                }
                _ => return Err(EINVAL),
            }
        }
        Ok(())
    }
);

configfs_simple_bool_field!(DeviceConfig, 13, bad_blocks_once);
configfs_simple_bool_field!(DeviceConfig, 14, bad_blocks_partial_io);
configfs_attribute!(DeviceConfig, 15,
    show: |this, page| show_field(this.data.lock().cache_size_mib, page),
    store: |this, page| store_with_power_check(this, page, |this, page| {

        let text = core::str::from_utf8(page)?.trim();
        let value = text.parse::<u64>().map_err(|_| EINVAL)?;
        let mut guard = this.data.lock();
        guard.disk_storage = Arc::pin_init(DiskStorage::new(value, guard.block_size as usize), GFP_KERNEL)?;
        guard.cache_size_mib = value;
        Ok(())
    })
);

configfs_simple_field!(DeviceConfig, 16, mbps, u32);
