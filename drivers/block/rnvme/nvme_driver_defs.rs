use kernel::bindings;

pub(crate) const NVME_MAX_KB_SZ: usize = 4096;

pub(crate) const NVME_MAX_SEGS: usize = 127;

pub(crate) const NVME_CTRL_PAGE_SIZE: usize = bindings::PAGE_SIZE;

pub(crate) const NVME_MAX_SEGMENTS: usize = NVME_CTRL_PAGE_SIZE / 16;
