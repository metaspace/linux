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
    //! Block layer errors.

    use core::num::NonZeroU8;

    pub mod code {
        //! C compatible error codes for the block subsystem.
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

    /// A wrapper around a 1 byte block layer error code.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct BlkError(NonZeroU8);

    impl BlkError {
        /// Create a [`BlkError`] from a `blk_status_t`.
        ///
        /// If the code is not know, this function will warn and return [`code::BLK_STS_IOERR`].
        pub fn from_blk_status(status: bindings::blk_status_t) -> Self {
            if let Some(error) = Self::try_from_blk_status(status) {
                error
            } else {
                kernel::pr_warn!("Attempted to create `BlkError` from invalid value");
                code::BLK_STS_IOERR
            }
        }

        /// Convert `Self` to the underlying type.
        pub fn to_blk_status(self) -> bindings::blk_status_t {
            self.0.into()
        }

        /// Try to create a `Self` form a `blk_status_t`.
        ///
        /// Returns `None` if the conversion fails.
        const fn try_from_blk_status(errno: bindings::blk_status_t) -> Option<Self> {
            if errno == 0 {
                None
            } else {
                Some(BlkError(
                    // SAFETY: We just checked that `errno`is nonzero.
                    unsafe { NonZeroU8::new_unchecked(errno) },
                ))
            }
        }
    }

    impl From<BlkError> for u8 {
        fn from(value: BlkError) -> Self {
            value.0.into()
        }
    }

    impl From<BlkError> for u32 {
        fn from(value: BlkError) -> Self {
            let value: u8 = value.0.into();
            value.into()
        }
    }

    impl From<kernel::error::Error> for BlkError {
        fn from(_value: kernel::error::Error) -> Self {
            code::BLK_STS_IOERR
        }
    }

    /// A result with a [`BlkError`] error type.
    pub type BlkResult<T = ()> = Result<T, BlkError>;

    /// Convert a `blk_status_t` to a `BlkResult`.
    pub fn to_result(status: bindings::blk_status_t) -> BlkResult {
        if status == bindings::BLK_STS_OK {
            Ok(())
        } else {
            Err(BlkError::from_blk_status(status))
        }
    }
}
