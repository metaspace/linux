// SPDX-License-Identifier: GPL-2.0

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Command {
    Read = bindings::req_op_REQ_OP_READ,
    Write = bindings::req_op_REQ_OP_WRITE,
    Flush = bindings::req_op_REQ_OP_FLUSH,
    Discard = bindings::req_op_REQ_OP_DISCARD,
    SecureErase = bindings::req_op_REQ_OP_SECURE_ERASE,
    ZoneAppend = bindings::req_op_REQ_OP_ZONE_APPEND,
    WriteZeroes = bindings::req_op_REQ_OP_WRITE_ZEROES,
    ZoneOpen = bindings::req_op_REQ_OP_ZONE_OPEN,
    ZoneClose = bindings::req_op_REQ_OP_ZONE_CLOSE,
    ZoneFinish = bindings::req_op_REQ_OP_ZONE_FINISH,
    ZoneReset = bindings::req_op_REQ_OP_ZONE_RESET,
    ZoneResetAll = bindings::req_op_REQ_OP_ZONE_RESET_ALL,
    DriverIn = bindings::req_op_REQ_OP_DRV_IN,
    DriverOut = bindings::req_op_REQ_OP_DRV_OUT,
}

impl Command {
    pub unsafe fn from_raw(value: u32) -> Self {
        unsafe { core::mem::transmute(value) }
    }
}
