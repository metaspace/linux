// SPDX-License-Identifier: GPL-2.0

use super::DeviceConfig;
use core::{fmt::Write, str::FromStr};
use kernel::{page::PAGE_SIZE, prelude::*};

pub(crate) fn show_field<T: fmt::Display>(value: T, page: &mut [u8; PAGE_SIZE]) -> Result<usize> {
    let mut writer = kernel::str::Formatter::new(page);
    writer.write_fmt(fmt!("{}\n", value))?;
    Ok(writer.bytes_written())
}

pub(crate) fn store_with_power_check<F>(this: &DeviceConfig, page: &[u8], store_fn: F) -> Result
where
    F: FnOnce(&DeviceConfig, &[u8]) -> Result,
{
    if this.data.lock().powered {
        return Err(EBUSY);
    }
    store_fn(this, page)
}

pub(crate) fn store_number_with_power_check<F, T>(
    this: &DeviceConfig,
    page: &[u8],
    store_fn: F,
) -> Result
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
        $(,)?
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
pub(crate) use configfs_attribute;

// Specialized macro for simple boolean fields that just store kstrtobool_bytes result.
macro_rules! configfs_simple_bool_field {
    ($type:ty, $id:literal, $field:ident) => {
        crate::configfs::macros::configfs_attribute!($type, $id,
            show: |this, page| crate::configfs::macros::show_field(this.data.lock().$field, page),
            store: |this, page|
              crate::configfs::macros::store_with_power_check(this, page, |this, page| {
                this.data.lock().$field = kstrtobool_bytes(page)?;
                Ok(())
            })
        );
    };
}
pub(crate) use configfs_simple_bool_field;

// Specialized macro for simple numeric fields that just parse and assign
macro_rules! configfs_simple_field {
    // Simple direct assignment
    ($type:ty, $id:literal, $field:ident, $field_type:ty) => {
        crate::configfs::macros::configfs_attribute!($type, $id,
            show: |this, page| crate::configfs::macros::show_field(this.data.lock().$field, page),
            store: |this, page| crate::configfs::macros::store_number_with_power_check(
                this,
                page,
                |this, value: $field_type| {
                    this.data.lock().$field = value;
                    Ok(())
                }
            )
        );
    };
    // With infallible conversion expression (direct value)
    ($type:ty, $id:literal, $field:ident, $field_type:ty, into $convert:expr) => {
        crate::configfs::macros::configfs_attribute!($type, $id,
            show: |this, page|
                crate::configfs::macros::show_field(this.data.lock().$field, page),
            store: |this, page| crate::configfs::macros::store_number_with_power_check(
                this,
                page,
                |this, value: $field_type| {
                    this.data.lock().$field = $convert(value);
                    Ok(())
                }
            )
        );
    };
    // With check, no conversion
    ($type:ty, $id:literal, $field:ident, $field_type:ty, check $check:expr) => {
        crate::configfs::macros::configfs_attribute!($type, $id,
            show: |this, page| crate::configfs::macros::show_field(this.data.lock().$field, page),
            store: |this, page| crate::configfs::macros::store_number_with_power_check(
                this,
                page,
                |this, value: $field_type| {
                    $check(value)?;
                    this.data.lock().$field = value;
                    Ok(())
                }
            )
        );
    };
}
pub(crate) use configfs_simple_field;
