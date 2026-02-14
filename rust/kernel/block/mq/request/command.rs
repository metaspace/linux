// SPDX-License-Identifier: GPL-2.0

/// Block I/O operation codes.
///
/// This is the Rust abstraction for the C [`enum req_op`].
///
/// Operations common to the bio and request structures. The kernel uses 8 bits
/// for encoding the operation, and the remaining 24 bits for flags.
///
/// The least significant bit of the operation number indicates the data
/// transfer direction:
///
/// - If the least significant bit is set, transfers are TO the device.
/// - If the least significant bit is not set, transfers are FROM the device.
///
/// If an operation does not transfer data, the least significant bit has no
/// meaning.
///
/// [`enum req_op`]: srctree/include/linux/blk_types.h
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Command {
    /// Read sectors from the device.
    Read = bindings::req_op_REQ_OP_READ,
    /// Write sectors to the device.
    Write = bindings::req_op_REQ_OP_WRITE,
    /// Flush the volatile write cache.
    Flush = bindings::req_op_REQ_OP_FLUSH,
    /// Discard sectors.
    Discard = bindings::req_op_REQ_OP_DISCARD,
    /// Securely erase sectors.
    SecureErase = bindings::req_op_REQ_OP_SECURE_ERASE,
    /// Write data at the current zone write pointer.
    ZoneAppend = bindings::req_op_REQ_OP_ZONE_APPEND,
    /// Write zeroes. This allows to implement zeroing for devices that don't use either discard
    /// with a predictable zero pattern or WRITE SAME of zeroes.
    WriteZeroes = bindings::req_op_REQ_OP_WRITE_ZEROES,
    /// Open a zone.
    ZoneOpen = bindings::req_op_REQ_OP_ZONE_OPEN,
    /// Close a zone.
    ZoneClose = bindings::req_op_REQ_OP_ZONE_CLOSE,
    /// Transition a zone to full.
    ZoneFinish = bindings::req_op_REQ_OP_ZONE_FINISH,
    /// Reset a zone write pointer.
    ZoneReset = bindings::req_op_REQ_OP_ZONE_RESET,
    /// Reset all the zones present on the device.
    ZoneResetAll = bindings::req_op_REQ_OP_ZONE_RESET_ALL,
    /// Driver private request for data transfer to the driver.
    DriverIn = bindings::req_op_REQ_OP_DRV_IN,
    /// Driver private request for data transfer from the driver.
    DriverOut = bindings::req_op_REQ_OP_DRV_OUT,
}

impl Command {
    /// Creates a [`Command`] from a raw `u32` value.
    ///
    /// # Safety
    ///
    /// The value must be a valid `req_op` operation code.
    pub unsafe fn from_raw(value: u32) -> Self {
        // SAFETY: The caller guarantees that the value is a valid operation
        // code.
        unsafe { core::mem::transmute(value) }
    }
}
