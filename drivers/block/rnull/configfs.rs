// SPDX-License-Identifier: GPL-2.0

use super::{
    NullBlkDevice,
    THIS_MODULE, //
};
use kernel::{
    block::mq::gen_disk::{
        GenDisk,
        GenDiskBuilder, //
    },
    c_str,
    configfs::{
        self,
        AttributeOperations, //
    },
    configfs_attrs,
    fmt::{
        self,
        Write as _, //
    },
    new_mutex,
    page::PAGE_SIZE,
    prelude::*,
    str::{
        kstrtobool_bytes,
        CString, //
    },
    sync::Mutex,
    time, //
};
use macros::{
    configfs_simple_bool_field,
    configfs_simple_field, //
};
use pin_init::PinInit;

mod macros;

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
        writer.write_str("blocksize,size,rotational,irqmode,completion_nsec\n")?;
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
            ],
        };

        Ok(configfs::Group::new(
            name.try_into()?,
            item_type,
            // TODO: cannot coerce new_mutex!() to impl PinInit<_, Error>, so put mutex inside
            try_pin_init!( DeviceConfig {
                data <- new_mutex!(DeviceConfigInner {
                    powered: false,
                    block_size: 4096,
                    rotational: false,
                    disk: None,
                    capacity_mib: 4096,
                    irq_mode: IRQMode::None,
                    completion_time: time::Delta::ZERO,
                    name: name.try_into()?,
                }),
            }),
            core::iter::empty(),
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
    disk: Option<GenDisk<NullBlkDevice>>,
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

impl core::str::FromStr for IRQMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let value: u8 = s.parse().map_err(|_| EINVAL)?;
        value.try_into()
    }
}
