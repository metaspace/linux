// SPDX-License-Identifier: GPL-2.0

// Return true if `value` is a power of two.
pub(crate) fn is_power_of_two<T>(value: T) -> bool
where
    T: core::ops::Sub<T, Output = T>,
    T: core::ops::BitAnd<Output = T>,
    T: core::cmp::PartialOrd<T>,
    T: Copy,
    T: From<u8>,
{
    (value > 0u8.into()) && (value & (value - 1u8.into())) == 0u8.into()
}

// Round `value` down to the next multiple of `to`, which must be a power of
// two.
pub(crate) fn align_down<T>(value: T, to: T) -> T
where
    T: core::ops::Sub<T, Output = T>,
    T: core::ops::Not<Output = T>,
    T: core::ops::BitAnd<Output = T>,
    T: core::cmp::PartialOrd<T>,
    T: Copy,
    T: From<u8>,
{
    debug_assert!(is_power_of_two(to));
    value & !(to - 1u8.into())
}

// Round `value` up to the next multiple of `to`, which must be a power of two.
#[cfg(CONFIG_BLK_DEV_ZONED)]
pub(crate) fn align_up<T>(value: T, to: T) -> T
where
    T: core::ops::Sub<T, Output = T>,
    T: core::ops::Add<T, Output = T>,
    T: core::ops::BitAnd<Output = T>,
    T: core::ops::BitOr<Output = T>,
    T: core::cmp::PartialOrd<T>,
    T: Copy,
    T: From<u8>,
{
    debug_assert!(is_power_of_two(to));
    ((value - 1u8.into()) | (to - 1u8.into())) + 1u8.into()
}

pub(crate) fn mib_to_sectors<T>(mib: T) -> T
where
    T: core::ops::Shl<u32, Output = T>,
{
    mib << (20 - kernel::block::SECTOR_SHIFT)
}

pub(crate) fn sectors_to_bytes<T>(sectors: T) -> T
where
    T: core::ops::Shl<u32, Output = T>,
{
    sectors << kernel::block::SECTOR_SHIFT
}

pub(crate) fn bytes_to_sectors<T>(bytes: T) -> T
where
    T: core::ops::Shl<u32, Output = T>,
{
    bytes << kernel::block::SECTOR_SHIFT
}
