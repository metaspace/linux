// SPDX-License-Identifier: GPL-2.0

//! Types for working with the block layer.

pub mod badblocks;
pub mod bio;
pub mod mq;

/// Bit mask for masking out [`SECTOR_SIZE`].
pub const SECTOR_MASK: u32 = bindings::SECTOR_MASK;

/// Sectors are size `1 << SECTOR_SHIFT`.
pub const SECTOR_SHIFT: u32 = bindings::SECTOR_SHIFT;

/// Size of a sector.
pub const SECTOR_SIZE: u32 = bindings::SECTOR_SIZE;

/// The difference between the size of a page and the size of a sector,
/// expressed as a power of two.
pub const PAGE_SECTORS_SHIFT: u32 = bindings::PAGE_SECTORS_SHIFT;

pub mod error {
    use core::num::NonZeroU8;

    pub mod code {
        macro_rules! declare_err {
            ($err:tt $(,)? $($doc:expr),+) => {
                $(
                    #[doc = $doc]
                )*
                    pub const $err: super::BlkError =
                    match super::BlkError::try_from_blk_status(crate::bindings::$err as u8) {
                        Some(err) => err,
                        None => panic!("Invalid errno in `declare_err!`"),
                    };
            };
        }

        declare_err!(BLK_STS_NOTSUPP, "Operation not supported.");
        declare_err!(BLK_STS_IOERR, "Generic IO error.");
        declare_err!(BLK_STS_DEV_RESOURCE, "Device resource busy. Retry later.");
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct BlkError(NonZeroU8);

    impl BlkError {
        pub fn from_blk_status(status: bindings::blk_status_t) -> Self {
            if let Some(error) = Self::try_from_blk_status(status) {
                error
            } else {
                kernel::pr_warn!("Attempted to create `BlkError` from invalid value");
                code::BLK_STS_IOERR
            }
        }

        pub fn to_blk_status(self) -> bindings::blk_status_t {
            self.0.into()
        }

        const fn try_from_blk_status(errno: bindings::blk_status_t) -> Option<Self> {
            if errno == 0 {
                return None;
            } else {
                return Some(BlkError(unsafe { NonZeroU8::new_unchecked(errno) }));
            }
        }
    }

    impl From<kernel::error::Error> for BlkError {
        fn from(_value: kernel::error::Error) -> Self {
            code::BLK_STS_IOERR
        }
    }

    pub type BlkResult<T = ()> = Result<T, BlkError>;

    pub fn to_result(status: bindings::blk_status_t) -> BlkResult {
        if status == bindings::BLK_STS_OK as u8 {
            Ok(())
        } else {
            Err(BlkError::from_blk_status(status))
        }
    }
}
