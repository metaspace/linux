// SPDX-License-Identifier: GPL-2.0

use super::{
    DiskStorage,
    NullBlkDevice,
    THIS_MODULE, //
};
use core::fmt::Write;
use kernel::{
    bindings,
    block::{
        badblocks::BadBlocks,
        mq::gen_disk::{
            GenDisk,
            GenDiskBuilder, //
        }, //
    },
    c_str,
    configfs::{
        self,
        AttributeOperations, //
    },
    configfs_attrs,
    fmt,
    new_mutex,
    page::PAGE_SIZE,
    prelude::*,
    str::{
        kstrtobool_bytes,
        CString, //
    },
    sync::{
        Arc,
        Mutex, //
    },
    time, //
};
use macros::{
    configfs_attribute,
    configfs_simple_bool_field,
    configfs_simple_field,
    show_field,
    store_with_power_check, //
};
use pin_init::PinInit;

mod macros;

#[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
use kernel::fault_injection::FaultConfig;

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
             submit_queues,use_per_node_hctx,discard,blocking,shared_tags,\
             zoned,zone_size,zone_capacity,poll_queues\n",
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
                blocking: 17,
                shared_tags: 18,
                hw_queue_depth: 19,
                zoned: 20,
                zone_size: 21,
                zone_capacity: 22,
                zone_nr_conv: 23,
                zone_max_open: 24,
                zone_max_active: 25,
                zone_append_max_sectors: 26,
                poll_queues: 27,
                fua: 28,
            ],
        };

        use kernel::configfs::CDefaultGroup;

        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        let mut default_groups: KVec<Arc<dyn CDefaultGroup>> = KVec::new();

        #[cfg(not(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION))]
        let default_groups: KVec<Arc<dyn CDefaultGroup>> = KVec::new();

        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        let timeout_inject = Arc::pin_init(
            kernel::fault_injection::FaultConfig::new(c"timeout_inject"),
            GFP_KERNEL,
        )?;

        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        let requeue_inject = Arc::pin_init(
            kernel::fault_injection::FaultConfig::new(c"requeue_inject"),
            GFP_KERNEL,
        )?;

        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        let init_hctx_inject = Arc::pin_init(
            kernel::fault_injection::FaultConfig::new(c"init_hctx_fault_inject"),
            GFP_KERNEL,
        )?;

        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        {
            default_groups.push(timeout_inject.clone(), GFP_KERNEL)?;
            default_groups.push(requeue_inject.clone(), GFP_KERNEL)?;
            default_groups.push(init_hctx_inject.clone(), GFP_KERNEL)?;
        }

        let block_size = 4096;
        Ok(configfs::Group::new(
            name.try_into()?,
            item_type,
            // default_groups,
            // TODO: cannot coerce new_mutex!() to impl PinInit<_, Error>, so put mutex inside
            try_pin_init!(DeviceConfig {
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
                    queue_config: Arc::pin_init(
                        new_mutex!(QueueConfig {
                            submit_queues: 1,
                            poll_queues: 0
                        }),
                        GFP_KERNEL
                    )?,
                    home_node: bindings::NUMA_NO_NODE,
                    discard: false,
                    no_sched: false,
                    bad_blocks: Arc::pin_init(BadBlocks::new(false), GFP_KERNEL)?,
                    bad_blocks_once: false,
                    bad_blocks_partial_io: false,
                    disk_storage: Arc::pin_init(DiskStorage::new(0, block_size), GFP_KERNEL)?,
                    cache_size_mib: 0,
                    mbps: 0,
                    blocking: false,
                    shared_tags: false,
                    hw_queue_depth: 64,
                    zoned: false,
                    zone_size_mib: 256,
                    zone_capacity_mib: 0,
                    zone_nr_conv: 0,
                    zone_max_open: 0,
                    zone_max_active: 0,
                    zone_append_max_sectors: u32::MAX,
                    fua: true,
                    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                    timeout_inject,
                    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                    requeue_inject,
                    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                    init_hctx_inject,
                }),
            }),
            default_groups,
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

impl fmt::Display for IRQMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    queue_config: Arc<Mutex<QueueConfig>>,
    home_node: i32,
    discard: bool,
    no_sched: bool,
    bad_blocks: Arc<BadBlocks>,
    bad_blocks_once: bool,
    bad_blocks_partial_io: bool,
    cache_size_mib: u64,
    disk_storage: Arc<DiskStorage>,
    mbps: u32,
    blocking: bool,
    shared_tags: bool,
    hw_queue_depth: u32,
    zoned: bool,
    zone_size_mib: u32,
    zone_capacity_mib: u32,
    zone_nr_conv: u32,
    zone_max_open: u32,
    zone_max_active: u32,
    zone_append_max_sectors: u32,
    fua: bool,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    timeout_inject: Arc<FaultConfig>,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    requeue_inject: Arc<FaultConfig>,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    init_hctx_inject: Arc<FaultConfig>,
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
            guard.disk = Some(NullBlkDevice::new(crate::NullBlkOptions {
                name: &guard.name,
                block_size_bytes: guard.block_size,
                rotational: guard.rotational,
                device_capacity_mib: guard.capacity_mib,
                irq_mode: guard.irq_mode,
                completion_time: guard.completion_time,
                memory_backed: guard.memory_backed,
                queue_config: guard.queue_config.clone(),
                home_node: guard.home_node,
                discard: guard.discard,
                no_sched: guard.no_sched,
                bad_blocks: guard.bad_blocks.clone(),
                bad_blocks_once: guard.bad_blocks_once,
                bad_blocks_partial_io: guard.bad_blocks_partial_io,
                storage: guard.disk_storage.clone(),
                bandwidth_limit: u64::from(guard.mbps) * 2u64.pow(20),
                blocking: guard.blocking,
                shared_tags: guard.shared_tags,
                hw_queue_depth: guard.hw_queue_depth,
                zoned: guard.zoned,
                zone_size_mib: guard.zone_size_mib,
                zone_capacity_mib: guard.zone_capacity_mib,
                zone_nr_conv: guard.zone_nr_conv,
                zone_max_open: guard.zone_max_open,
                zone_max_active: guard.zone_max_active,
                zone_append_max_sectors: guard.zone_append_max_sectors,
                forced_unit_access: guard.fua,
                #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                requeue_inject: guard.requeue_inject.clone(),
                #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                init_hctx_inject: guard.init_hctx_inject.clone(),
                #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                timeout_inject: guard.timeout_inject.clone(),
            })?);
            guard.powered = true;
        } else if guard.powered && !power_op {
            drop(guard.disk.take());
            guard.powered = false;
        }

        Ok(())
    }
}

pub(crate) struct QueueConfig {
    pub(crate) submit_queues: u32,
    pub(crate) poll_queues: u32,
}

configfs_simple_field!(
    DeviceConfig,
    1,
    block_size,
    u32,
    check GenDiskBuilder::<NullBlkDevice>::validate_block_size
);
configfs_simple_bool_field!(DeviceConfig, 2, rotational);
configfs_simple_field!(DeviceConfig, 3, capacity_mib, u64);
configfs_simple_field!(DeviceConfig, 4, irq_mode, IRQMode);
configfs_simple_field!(DeviceConfig, 5, completion_time, i64, into time::Delta::from_nanos);

impl core::str::FromStr for IRQMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let value: u8 = s.parse().map_err(|_| EINVAL)?;
        value.try_into()
    }
}

#[vtable]
impl configfs::AttributeOperations<6> for DeviceConfig {
    type Data = DeviceConfig;

    fn show(this: &DeviceConfig, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
        let mut writer = kernel::str::Formatter::new(page);

        if this.data.lock().memory_backed {
            writer.write_fmt(fmt!("1\n"))?;
        } else {
            writer.write_fmt(fmt!("0\n"))?;
        }

        Ok(writer.bytes_written())
    }

    fn store(this: &DeviceConfig, page: &[u8]) -> Result {
        if this.data.lock().powered {
            return Err(EBUSY);
        }

        this.data.lock().memory_backed = core::str::from_utf8(page)?
            .trim()
            .parse::<u8>()
            .map_err(|_| kernel::error::code::EINVAL)?
            != 0;

        Ok(())
    }
}

configfs_attribute! {
    DeviceConfig,
    7,
    show: |this, page| show_field(this.data.lock().queue_config.lock().submit_queues, page),
    store: |this,page| {
        let config_guard = this.data.lock();
        let mut queue_config = config_guard.queue_config.lock();

        let text = core::str::from_utf8(page)?.trim();
        let value = text.parse().map_err(|_| EINVAL)?;
        if value > kernel::num_possible_cpus() {
            return Err(kernel::error::code::EINVAL)
        }

        let old_submit_queues = queue_config.submit_queues;
        queue_config.submit_queues = value;
        let total_queue_count = queue_config.submit_queues + queue_config.poll_queues;

        let disk = config_guard.disk.clone();

        drop(queue_config);
        drop(config_guard);

        if let Some(disk) = &disk {
            if let Err(e) = disk.tag_set().update_hw_queue_count(total_queue_count) {
                this.data.lock().queue_config.lock().submit_queues = old_submit_queues;
                return Err(e);
            }
        }

        Ok(())
    },
}

configfs_attribute!(DeviceConfig, 8,
            show: |this, page| show_field(
                this.data.lock().queue_config.lock().submit_queues == kernel::num_online_nodes(),
                page
            ),
            store: |this, page| store_with_power_check(this, page, |this, page| {
                let value = core::str::from_utf8(page)?
                    .trim()
                    .parse::<u8>()
                    .map_err(|_| kernel::error::code::EINVAL)?
                    != 0;

                if value {
        this.data.lock().queue_config.lock().submit_queues *= kernel::num_online_nodes();
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

#[vtable]
impl configfs::AttributeOperations<11> for DeviceConfig {
    type Data = DeviceConfig;

    fn show(this: &DeviceConfig, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
        let mut writer = kernel::str::Formatter::new(page);

        if this.data.lock().no_sched {
            writer.write_str("1\n")?;
        } else {
            writer.write_str("0\n")?;
        }

        Ok(writer.bytes_written())
    }

    fn store(this: &DeviceConfig, page: &[u8]) -> Result {
        if this.data.lock().powered {
            return Err(EBUSY);
        }

        this.data.lock().no_sched = kstrtobool_bytes(page)?;

        Ok(())
    }
}

#[vtable]
impl configfs::AttributeOperations<12> for DeviceConfig {
    type Data = DeviceConfig;

    fn show(this: &DeviceConfig, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
        let ret = this.data.lock().bad_blocks.show(page, false);
        if ret < 0 {
            Err(Error::from_errno(ret as c_int))
        } else {
            Ok(ret as usize)
        }
    }

    fn store(this: &DeviceConfig, page: &[u8]) -> Result {
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
                            this.data.lock().bad_blocks.set_bad(start..=end, true)?;
                        } else {
                            this.data.lock().bad_blocks.set_good(start..=end)?;
                        }
                    }
                }
                _ => return Err(EINVAL),
            }
        }

        Ok(())
    }
}

configfs_simple_bool_field!(DeviceConfig, 13, bad_blocks_once);
configfs_simple_bool_field!(DeviceConfig, 14, bad_blocks_partial_io);
configfs_attribute!(DeviceConfig, 15,
    show: |this, page| show_field(this.data.lock().cache_size_mib, page),
    store: |this, page| store_with_power_check(this, page, |this, page| {
        let text = core::str::from_utf8(page)?.trim();
        let value = text.parse::<u64>().map_err(|_| EINVAL)?;
        let mut guard = this.data.lock();
        guard.disk_storage = Arc::pin_init(DiskStorage::new(value, guard.block_size), GFP_KERNEL)?;
        guard.cache_size_mib = value;
        Ok(())
    })
);

configfs_simple_field!(DeviceConfig, 16, mbps, u32);
configfs_simple_bool_field!(DeviceConfig, 17, blocking);
configfs_simple_bool_field!(DeviceConfig, 18, shared_tags);
configfs_simple_field!(DeviceConfig, 19, hw_queue_depth, u32);
configfs_simple_bool_field!(DeviceConfig, 20, zoned);
configfs_simple_field!(DeviceConfig, 21, zone_size_mib, u32);
configfs_simple_field!(DeviceConfig, 22, zone_capacity_mib, u32);
configfs_simple_field!(DeviceConfig, 23, zone_nr_conv, u32);
configfs_simple_field!(DeviceConfig, 24, zone_max_open, u32);
configfs_simple_field!(DeviceConfig, 25, zone_max_active, u32);
configfs_simple_field!(DeviceConfig, 26, zone_append_max_sectors, u32);
configfs_attribute! {
    DeviceConfig,
    27,
    show: |this, page| show_field(this.data.lock().queue_config.lock().poll_queues, page),
    store: |this,page| {
        let config_guard = this.data.lock();
        let mut queue_config = config_guard.queue_config.lock();

        let text = core::str::from_utf8(page)?.trim();
        let value = text.parse().map_err(|_| EINVAL)?;
        if value > kernel::num_possible_cpus() {
            return Err(kernel::error::code::EINVAL)
        }

        let old_poll_queues = queue_config.poll_queues;
        queue_config.poll_queues = value;
        let total_queue_count = queue_config.submit_queues + queue_config.poll_queues;

        let disk = config_guard.disk.clone();

        drop(queue_config);
        drop(config_guard);

        if let Some(disk) = &disk {
            if let Err(e) = disk.tag_set().update_hw_queue_count(total_queue_count) {
                this.data.lock().queue_config.lock().poll_queues = old_poll_queues;
                return Err(e);
            }
        }

        Ok(())
    },
}
configfs_simple_bool_field!(DeviceConfig, 28, fua);
