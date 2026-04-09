// SPDX-License-Identifier: GPL-2.0

// TODO: Move this to another module.
// TODO: Implement this properly for be archs. (At the moment conversion is no-op.)
#[derive(Default, Clone, Copy)]
#[repr(transparent)]
#[allow(non_camel_case_types)]
pub(crate) struct le<T>(T);

impl<T> le<T> {
    pub(crate) fn into(self) -> T {
        self.0
    }
}

impl<T> From<T> for le<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

#[allow(non_camel_case_types)]
pub(crate) enum NvmeAdminOpcode {
    delete_sq = 0x00,
    create_sq = 0x01,
    get_log_page = 0x02,
    delete_cq = 0x04,
    create_cq = 0x05,
    identify = 0x06,
    abort_cmd = 0x08,
    set_features = 0x09,
    get_features = 0x0a,
    async_event = 0x0c,
    activate_fw = 0x10,
    download_fw = 0x11,
    dbbuf = 0x7c,
    format_nvm = 0x80,
    security_send = 0x81,
    security_recv = 0x82,
}

#[allow(non_camel_case_types)]
pub(crate) enum NvmeOpcode {
    flush = 0x00,
    write = 0x01,
    read = 0x02,
    write_uncor = 0x04,
    compare = 0x05,
    dsm = 0x09,
}

pub(crate) const NVME_QUEUE_PHYS_CONTIG: u16 = 1 << 0;
pub(crate) const NVME_CQ_IRQ_ENABLED: u16 = 1 << 1;
pub(crate) const NVME_SQ_PRIO_URGENT: u16 = 0 << 1;
pub(crate) const NVME_SQ_PRIO_HIGH: u16 = 1 << 1;
pub(crate) const NVME_SQ_PRIO_MEDIUM: u16 = 2 << 1;
pub(crate) const NVME_SQ_PRIO_LOW: u16 = 3 << 1;

pub(crate) const NVME_FEAT_ARBITRATION: u32 = 0x01;
pub(crate) const NVME_FEAT_POWER_MGMT: u32 = 0x02;
pub(crate) const NVME_FEAT_LBA_RANGE: u32 = 0x03;
pub(crate) const NVME_FEAT_TEMP_THRESH: u32 = 0x04;
pub(crate) const NVME_FEAT_ERR_RECOVERY: u32 = 0x05;
pub(crate) const NVME_FEAT_VOLATILE_WC: u32 = 0x06;
pub(crate) const NVME_FEAT_NUM_QUEUES: u32 = 0x07;
pub(crate) const NVME_FEAT_IRQ_COALESCE: u32 = 0x08;
pub(crate) const NVME_FEAT_IRQ_CONFIG: u32 = 0x09;
pub(crate) const NVME_FEAT_WRITE_ATOMIC: u32 = 0x0a;
pub(crate) const NVME_FEAT_ASYNC_EVENT: u32 = 0x0b;
pub(crate) const NVME_FEAT_SW_PROGRESS: u32 = 0x0c;

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeCompletion {
    pub(crate) result: le<u32>,
    reserved: u32,
    pub(crate) sq_head: le<u16>,
    pub(crate) sq_id: le<u16>,
    pub(crate) command_id: u16,
    pub(crate) status: le<u16>,
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeCreateCq {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) command_id: u16,
    pub(crate) rsvd1: [u32; 5],
    pub(crate) prp1: le<u64>,
    pub(crate) rsvd8: u64,
    pub(crate) cqid: le<u16>,
    pub(crate) qsize: le<u16>,
    pub(crate) cq_flags: le<u16>,
    pub(crate) irq_vector: le<u16>,
    pub(crate) rsvd12: [u32; 4],
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeCreateSq {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) command_id: u16,
    pub(crate) rsvd1: [u32; 5],
    pub(crate) prp1: le<u64>,
    pub(crate) rsvd8: u64,
    pub(crate) sqid: le<u16>,
    pub(crate) qsize: le<u16>,
    pub(crate) sq_flags: le<u16>,
    pub(crate) cqid: le<u16>,
    pub(crate) rsvd12: [u32; 4],
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeIdentify {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) command_id: u16,
    pub(crate) nsid: le<u32>,
    pub(crate) reserved1: [u64; 2],
    pub(crate) prp1: le<u64>,
    pub(crate) prp2: le<u64>,
    pub(crate) cns: le<u32>,
    pub(crate) reserved2: [u32; 5],
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeFeatures {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) command_id: u16,
    pub(crate) nsid: le<u32>,
    pub(crate) rsvd2: [u64; 2],
    pub(crate) prp1: le<u64>,
    pub(crate) prp2: le<u64>,
    pub(crate) fid: le<u32>,
    pub(crate) dword11: le<u32>,
    pub(crate) rsvd12: [u32; 4],
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeLbaRangeType {
    pub(crate) type_: u8,
    pub(crate) attributes: u8,
    rsvd2: [u8; 14],
    pub(crate) slba: le<u64>,
    pub(crate) nlb: le<u64>,
    pub(crate) guid: [u8; 16],
    rsvd48: [u8; 16],
}
pub(crate) const NVME_LBART_ATTRIB_TEMP: u8 = 1 << 0;
pub(crate) const NVME_LBART_ATTRIB_HIDE: u8 = 1 << 1;

#[repr(C, packed)]
pub(crate) struct NvmeIdPowerState {
    max_power: le<u16>, /* centiwatts */
    rsvd2: u16,
    entry_lat: le<u32>, /* microseconds */
    exit_lat: le<u32>,  /* microseconds */
    read_tput: u8,
    read_lat: u8,
    write_tput: u8,
    write_lat: u8,
    rsvd16: [u8; 16],
}

#[repr(C, packed)]
pub(crate) struct NvmeIdCtrl {
    pub(crate) vid: le<u16>,
    pub(crate) ssvid: le<u16>,
    pub(crate) sn: [u8; 20],
    pub(crate) mn: [u8; 40],
    pub(crate) fr: [u8; 8],
    pub(crate) rab: u8,
    pub(crate) ieee: [u8; 3],
    pub(crate) mic: u8,
    pub(crate) mdts: u8,
    rsvd78: [u8; 178],
    pub(crate) oacs: le<u16>,
    pub(crate) acl: u8,
    pub(crate) aerl: u8,
    pub(crate) frmw: u8,
    pub(crate) lpa: u8,
    pub(crate) elpe: u8,
    pub(crate) npss: u8,
    rsvd264: [u8; 248],
    pub(crate) sqes: u8,
    pub(crate) cqes: u8,
    pub(crate) rsvd514: [u8; 2],
    pub(crate) nn: le<u32>,
    pub(crate) oncs: le<u16>,
    pub(crate) fuses: le<u16>,
    pub(crate) fna: u8,
    pub(crate) vwc: u8,
    pub(crate) awun: le<u16>,
    pub(crate) awupf: le<u16>,
    rsvd530: [u8; 1518],
    pub(crate) psd: [NvmeIdPowerState; 32],
    pub(crate) vs: [u8; 1024],
}

#[repr(C, packed)]
pub(crate) struct NvmeLbaf {
    pub(crate) ms: le<u16>,
    pub(crate) ds: u8,
    pub(crate) rp: u8,
}

#[repr(C, packed)]
pub(crate) struct NvmeIdNs {
    pub(crate) nsze: le<u64>,
    pub(crate) ncap: le<u64>,
    pub(crate) nuse: le<u64>,
    pub(crate) nsfeat: u8,
    pub(crate) nlbaf: u8,
    pub(crate) flbas: u8,
    pub(crate) mc: u8,
    pub(crate) dpc: u8,
    pub(crate) dps: u8,
    rsvd30: [u8; 98],
    pub(crate) lbaf: [NvmeLbaf; 16],
    rsvd192: [u8; 192],
    pub(crate) vs: [u8; 3712],
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeRw {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) command_id: u16,
    pub(crate) nsid: le<u32>,
    pub(crate) rsvd2: u64,
    pub(crate) metadata: le<u64>,
    pub(crate) prp1: le<u64>,
    pub(crate) prp2: le<u64>,
    pub(crate) slba: le<u64>,
    pub(crate) length: le<u16>,
    pub(crate) control: le<u16>,
    pub(crate) dsmgmt: le<u32>,
    pub(crate) reftag: le<u32>,
    pub(crate) apptag: le<u16>,
    pub(crate) appmask: le<u16>,
}

#[derive(Default, Clone, Copy)]
#[repr(C, packed)]
pub(crate) struct NvmeCommon {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) command_id: u16,
    pub(crate) nsid: le<u32>,
    pub(crate) cdw2: [u32; 2],
    pub(crate) metadta: le<u64>,
    pub(crate) prp1: le<u64>,
    pub(crate) prp2: le<u64>,
    pub(crate) cdw10: [u32; 6],
}

#[derive(Clone, Copy)]
#[repr(C, packed)]
pub(crate) union NvmeCommand {
    pub(crate) common: NvmeCommon,
    pub(crate) rw: NvmeRw,
    pub(crate) identify: NvmeIdentify,
    pub(crate) features: NvmeFeatures,
    pub(crate) create_cq: NvmeCreateCq,
    pub(crate) create_sq: NvmeCreateSq,
}

impl NvmeCommand {
    pub(crate) fn new_flush(nsid: u32) -> Self {
        Self {
            common: NvmeCommon {
                opcode: NvmeOpcode::flush as _,
                nsid: nsid.into(),
                ..NvmeCommon::default()
            },
        }
    }
}

impl Default for NvmeCommand {
    fn default() -> Self {
        Self {
            common: NvmeCommon::default(),
        }
    }
}

pub(crate) const NVME_CC_ENABLE: u32 = 1 << 0;
pub(crate) const NVME_CC_CSS_NVM: u32 = 0 << 4;
pub(crate) const NVME_CC_MPS_SHIFT: u32 = 7;
pub(crate) const NVME_CC_ARB_RR: u32 = 0 << 11;
pub(crate) const NVME_CC_ARB_WRRU: u32 = 1 << 11;
pub(crate) const NVME_CC_ARB_VS: u32 = 7 << 11;
pub(crate) const NVME_CC_SHN_NONE: u32 = 0 << 14;
pub(crate) const NVME_CC_SHN_NORMAL: u32 = 1 << 14;
pub(crate) const NVME_CC_SHN_ABRUPT: u32 = 2 << 14;
pub(crate) const NVME_CC_IOSQES: u32 = 6 << 16;
pub(crate) const NVME_CC_IOCQES: u32 = 4 << 20;
pub(crate) const NVME_CSTS_RDY: u32 = 1 << 0;
pub(crate) const NVME_CSTS_CFS: u32 = 1 << 1;
pub(crate) const NVME_CSTS_SHST_NORMAL: u32 = 0 << 2;
pub(crate) const NVME_CSTS_SHST_OCCUR: u32 = 1 << 2;
pub(crate) const NVME_CSTS_SHST_CMPLT: u32 = 2 << 2;

// TODO Prefix constants with something.
pub(crate) const OFFSET_CAP: usize = 0x00;
pub(crate) const OFFSET_CC: usize = 0x14;
pub(crate) const OFFSET_CSTS: usize = 0x1c;
pub(crate) const OFFSET_AQA: usize = 0x24;
pub(crate) const OFFSET_ASQ: usize = 0x28;
pub(crate) const OFFSET_ACQ: usize = 0x30;
