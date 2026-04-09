// SPDX-License-Identifier: GPL-2.0

//! Driver for NVMe devices.
//!
//! Based on the C driver written by Matthew Wilcox <willy@linux.intel.com>.

use core::{
    cell::SyncUnsafeCell,
    convert::TryInto,
    format_args,
    ops::Deref,
    sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering},
};
use kernel::{
    alloc::{flags, mempool::MemPool, KBox},
    bindings,
    block::mq::{self, dma_map_iter::{DmaMapIterMapped, DmaMapMempool}, gen_disk::GenDisk, Command},
    c_str,
    device::{self, Bound},
    devres::Devres,
    dma::{self, Device, DmaMask},
    error::code::*,
    new_spinlock,
    pci::{self, Bar, IrqVectorRegistration},
    prelude::*,
    sync::{Arc, SpinLock},
    types::{ARef, Owned},
};

#[allow(dead_code)]
mod nvme_defs;
mod nvme_driver_defs;
mod nvme_mq;
mod nvme_queue;

use nvme_defs::*;
use nvme_driver_defs::*;

#[pin_data]
struct NvmeDevice {
    data: Arc<NvmeData>,
}

#[pin_data]
struct NvmeData {
    db_stride: usize,
    pci_dev: ARef<pci::Device>,
    instance: u32,
    shadow: Option<NvmeShadow>,
    #[pin]
    queues: SpinLock<NvmeQueues>,
    dma_pool: Arc<dma::Pool<le<u64>>>,
    poll_queue_count: u32,
    irq_queue_count: u32,
    bar: Pin<KBox<Devres<Bar<8192>>>>,
    #[pin]
    irq_vector_registration: SpinLock<Option<IrqVectorRegistration>>,
    dma_vec_mempool: DmaMapMempool<NVME_MAX_SEGMENTS>,
}

// TODO: This is not OK
unsafe impl Send for NvmeData {}
unsafe impl Sync for NvmeData {}

struct NvmeQueues {
    admin: Option<Arc<nvme_queue::NvmeQueue<nvme_mq::AdminQueueOperations>>>,
    io: KVec<Arc<nvme_queue::NvmeQueue<nvme_mq::IoQueueOperations>>>,
}

struct NvmeShadow {
    dbs: dma::CoherentAllocation<u32, dma::CoherentAllocator>,
    eis: dma::CoherentAllocation<u32, dma::CoherentAllocator>,
}

#[pin_data]
struct NvmeRequest<T: mq::Operations> {
    dma_pool: Arc<dma::Pool<le<u64>>>,
    result: AtomicU32,
    status: AtomicU16,
    direction: AtomicI32,
    len: AtomicU32,
    dev: ARef<device::Device>,
    cmd: SyncUnsafeCell<NvmeCommand>,
    sg_count: AtomicU32,
    page_count: AtomicU32,
    first_dma: AtomicU64,
    #[pin]
    mapping_data: SpinLock<Option<MappingData<T>>>,
}

struct NvmeNamespace {
    id: u32,
    lba_shift: u32,
}

const fn div_round_up(a: usize, b: usize) -> usize {
    (a + (b - 1)) / b
}

const fn npages_prp() -> usize {
    let nprps = div_round_up(
        nvme_driver_defs::NVME_MAX_KB_SZ * 1024 + nvme_driver_defs::NVME_CTRL_PAGE_SIZE,
        nvme_driver_defs::NVME_CTRL_PAGE_SIZE,
    );
    div_round_up(8 * nprps, nvme_driver_defs::NVME_CTRL_PAGE_SIZE - 8)
}


#[allow(dead_code)]
struct MappingData<T: mq::Operations> {
    io_data: DmaMapIterMapped<NVME_MAX_SEGMENTS, T>,
    prp_data: [usize; npages_prp()],
}

fn calculate_max_blocks(cap: u64, mdts: u8) -> Option<u32> {
    if mdts == 0 {
        return None;
    }

    let mps_min = ((cap >> 48) & 0xf) as u32;
    let ps_in_blocks = 1u32.checked_shl(mps_min.checked_add(3)?)?;
    ps_in_blocks.checked_mul(1u32.checked_shl(mdts.into())?)
}

impl NvmeDevice {
    fn alloc_ns(
        max_sectors: u32,
        instance: u32,
        nsid: u32,
        id: &NvmeIdNs,
        tagset: Arc<mq::TagSet<nvme_mq::IoQueueOperations>>,
        rt: &NvmeLbaRangeType,
    ) -> Result<Arc<GenDisk<nvme_mq::IoQueueOperations>>> {
        if rt.attributes & NVME_LBART_ATTRIB_HIDE != 0 {
            return Err(ENODEV);
        }

        let lbaf = (id.flbas & 0xf) as usize;
        let lba_shift = id.lbaf[lbaf].ds as u32;
        let ns = KBox::new(
            NvmeNamespace {
                id: nsid,
                lba_shift,
            },
            flags::GFP_KERNEL,
        )?;
        let disk = kernel::block::mq::gen_disk::GenDiskBuilder::new()
            .logical_block_size(1 << lba_shift)?
            .capacity_sectors(id.nsze.into() << (lba_shift - kernel::block::SECTOR_SHIFT))
            .virt_boundary_mask(nvme_driver_defs::NVME_CTRL_PAGE_SIZE - 1)
            .max_sectors(max_sectors)
            .max_segments(nvme_driver_defs::NVME_MAX_SEGS as _)
            .build(format_args!("nvme{}n{}", instance, nsid), tagset, ns)?;

        Ok(disk)
    }

    fn setup_io_queues(
        dev: &Arc<NvmeData>,
        pci_dev: &pci::Device<Bound>,
        admin_queue: &Arc<nvme_queue::NvmeQueue<nvme_mq::AdminQueueOperations>>,
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
    ) -> Result<Arc<mq::TagSet<nvme_mq::IoQueueOperations>>> {
        pr_info!("Setting up io queues\n");
        let nr_io_queues = dev.poll_queue_count + dev.irq_queue_count;
        let result = Self::set_queue_count(nr_io_queues, mq)?;
        if result < nr_io_queues {
            //nr_io_queues = result;
            todo!("Not implemented");
        }

        admin_queue.unregister_irq();
        // TODO: We might have to add a way to ensure the devres thing in the registration is
        // actually dropped?
        let registration = dev.irq_vector_registration.lock().take();
        drop(registration);
        let (irq_vector_registration, vectors) = pci_dev.alloc_irq_vectors_affinity(
            1,
            dev.irq_queue_count + 1,
            pci::IrqTypes::all(),
            1,
            0,
        )?;
        *dev.irq_vector_registration.lock() = Some(irq_vector_registration);
        admin_queue.register_irq(
            &pci_dev,
            vectors
                .nth(0)
                .expect("Allocated a vector for the admiqueue above"),
        )?;

        // TODO: Check what else needs to happen from C side.

        // Initialise the queue depth.
        let max_depth =
            (u64::from_le(dev.bar.try_access().unwrap().read64(OFFSET_CAP)) & 0xffff) + 1;
        let q_depth: u32 = core::cmp::min(max_depth, 128).try_into()?;

        // Advertise `q_depth - 1` tags to the block layer. The SQ has `q_depth`
        // slots but NVMe uses head == tail to mean "empty", so one slot must be
        // reserved as a sentinel to distinguish full from empty.
        let tagset = Arc::pin_init(
            mq::TagSet::new(
                nr_io_queues,
                dev.clone(),
                q_depth - 1,
                3,
                bindings::NUMA_NO_NODE,
                mq::tag_set::Flags::empty(),
            ),
            flags::GFP_KERNEL,
        )?; //TODO: 1 or 3 on demand, depending on polling enabled

        for i in 1..=nr_io_queues {
            let qid: u32 = i.try_into()?;

            let polled: bool = i > dev.irq_queue_count;

            let vector_index = if !polled { qid % vectors.len() } else { 0 };
            let vector = vectors
                .nth(vector_index)
                .expect("Allocated correct amount of vectors earlier");

            pr_info!(
                "Setting up queue {}, vector: {}, polled: {}\n",
                qid,
                vector_index,
                polled
            );

            let io_queue = nvme_queue::NvmeQueue::try_new(
                dev.clone(),
                &pci_dev,
                qid.try_into()?,
                q_depth.try_into()?,
                tagset.clone(),
                polled,
            )?;

            // Create completion queue.
            Self::alloc_completion_queue(mq, &io_queue, vector.index().try_into()?)?;

            // Create submission queue.
            Self::alloc_submission_queue(mq, &io_queue)?;

            if !polled {
                io_queue.register_irq(&pci_dev, vector)?;
            }

            dev.queues
                .lock()
                .io
                .push_within_capacity(io_queue.clone())?;
        }

        Ok(tagset)
    }

    fn dev_add(
        cap: u64,
        dev: &Arc<NvmeData>,
        pci_dev: &pci::Device<Bound>,
        admin_queue: &Arc<nvme_queue::NvmeQueue<nvme_mq::AdminQueueOperations>>,
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
    ) -> Result {
        let tagset = Self::setup_io_queues(dev, pci_dev, admin_queue, mq)?;
        pr_info!("setup_io_queues done\n");

        let id = dma::CoherentAllocator::alloc_coherent::<u8>(
            pci_dev.as_ref().into(),
            4096,
            flags::GFP_KERNEL,
        )?;
        let rt = dma::CoherentAllocator::alloc_coherent::<u8>(
            pci_dev.as_ref().into(),
            4096,
            flags::GFP_KERNEL,
        )?;

        // Identify the device.
        Self::identify(mq, 0, 1, id.dma_handle())?;

        let number_of_namespaces;
        let mdts;
        {
            let ctrl_id = unsafe { &*(id.start_ptr() as *const NvmeIdCtrl) };
            number_of_namespaces = ctrl_id.nn.into();
            mdts = ctrl_id.mdts;
        }

        let max_sectors = if let Some(blocks) = calculate_max_blocks(cap, mdts) {
            core::cmp::min((nvme_driver_defs::NVME_MAX_KB_SZ << 1) as u32, blocks)
        } else {
            (nvme_driver_defs::NVME_MAX_KB_SZ << 1) as u32
        };

        let zero_rt = NvmeLbaRangeType::default();
        for i in 1..=number_of_namespaces {
            if Self::identify(mq, i, 0, id.dma_handle()).is_err() {
                continue;
            }
            let id_ns = unsafe { &*(id.start_ptr() as *const NvmeIdNs) };
            if id_ns.ncap.into() == 0 {
                continue;
            }

            let res = Self::get_features(mq, NVME_FEAT_LBA_RANGE, i, rt.dma_handle());
            let rt = if res.is_err() {
                &zero_rt
            } else {
                unsafe { &*(rt.start_ptr() as *const NvmeLbaRangeType) }
            };

            pr_info!("about to add disk\n");
            let disk = Self::alloc_ns(max_sectors, dev.instance, i, id_ns, tagset.clone(), rt)?;
            // TODO: Add disk to list.
            pr_info!("disk added\n");

            // TODO: DONT LEAK
            core::mem::forget(disk);
        }

        Ok(())
    }

    fn wait_ready(dev: &Arc<NvmeData>) {
        pr_info!("Waiting for controller ready\n");
        {
            let bar = dev.bar.try_access().unwrap();
            while u32::from_le(bar.read32(OFFSET_CSTS)) & NVME_CSTS_RDY == 0 {
                unsafe { bindings::mdelay(100) };
                // TODO: Add check for fatal signal pending.
                // TODO: Set timeout.
            }
        }
        pr_info!("Controller ready\n");
    }

    fn wait_idle(dev: &Arc<NvmeData>) {
        pr_info!("Waiting for controller idle\n");
        {
            let bar = dev.bar.try_access().unwrap();
            while u32::from_le(bar.read32(OFFSET_CSTS)) & NVME_CSTS_RDY != 0 {
                unsafe { bindings::mdelay(100) };
                // TODO: Add check for fatal signal pending.
                // TODO: Set timeout.
            }
        }
        pr_info!("Controller ready\n");
    }

    fn configure_admin_queue(
        dev: &Arc<NvmeData>,
        pci_dev: &pci::Device<Bound>,
    ) -> Result<(
        Arc<nvme_queue::NvmeQueue<nvme_mq::AdminQueueOperations>>,
        Owned<mq::RequestQueue<nvme_mq::AdminQueueOperations>>,
    )> {
        // pr_info!("Reset subsystem\n");
        // let support_ssr = (u32::from_le(dev.resources().unwrap.bar.read32(OFFSET_CAP)) >> 36) & 1;
        // if support_ssr == 1 {
        //     dev.resources().unwrap().bar.write32(0x4E564D65);
        // } else {
        //     pr_info!("Controller does not support subsystem reset\n");
        // }

        let (registration, vectors) = pci_dev.alloc_irq_vectors(1, 1, pci::IrqTypes::all())?;

        pr_info!("Disable (reset) controller\n");
        {
            dev.bar.try_access().unwrap().write32(0, OFFSET_CC);
        }
        Self::wait_idle(dev);

        // TODO: Depth?
        let queue_depth = 64;
        let admin_tagset: Arc<mq::TagSet<nvme_mq::AdminQueueOperations>> = Arc::pin_init(
            mq::TagSet::new(
                1,
                dev.clone(),
                queue_depth,
                1,
                bindings::NUMA_NO_NODE,
                mq::tag_set::Flags::empty(),
            ),
            flags::GFP_KERNEL,
        )?;

        let admin_queue: Arc<nvme_queue::NvmeQueue<nvme_mq::AdminQueueOperations>> =
            nvme_queue::NvmeQueue::try_new(
                dev.clone(),
                pci_dev,
                0,
                queue_depth.try_into()?,
                admin_tagset.clone(),
                false,
            )?;
        dev.queues.lock().admin = Some(admin_queue.clone());
        let ns = KBox::new(
            NvmeNamespace {
                id: 0,
                lba_shift: 9,
            },
            flags::GFP_KERNEL,
        )?;
        let admin_mq = mq::RequestQueue::new(admin_tagset, ns)?;

        let mut aqa = (queue_depth - 1) as u32;
        aqa |= aqa << 16;

        let mut ctrl_config = NVME_CC_ENABLE | NVME_CC_CSS_NVM;
        ctrl_config |= (kernel::bindings::PAGE_SHIFT - 12) << NVME_CC_MPS_SHIFT;
        ctrl_config |= NVME_CC_ARB_RR | NVME_CC_SHN_NONE;
        ctrl_config |= NVME_CC_IOSQES | NVME_CC_IOCQES;

        pr_info!("About to wait for nvme readiness\n");
        {
            let bar = dev.bar.try_access().unwrap();

            // TODO: All writes should support endian conversion
            bar.write32(aqa, OFFSET_AQA);
            bar.write64(admin_queue.sq.dma_handle(), OFFSET_ASQ);
            bar.write64(admin_queue.cq.dma_handle(), OFFSET_ACQ);
            bar.write32(ctrl_config, OFFSET_CC);
        }
        Self::wait_ready(dev);

        pr_info!("Registering admin queue irq\n");
        *dev.irq_vector_registration.lock() = Some(registration);
        admin_queue.register_irq(
            pci_dev,
            vectors.nth(0).expect("Allocated at least one vector above"),
        )?;
        pr_info!("Done registering admin queue irq\n");

        Ok((admin_queue, admin_mq))
    }

    fn submit_sync_command(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        mut cmd: NvmeCommand,
    ) -> Result<u32> {
        let op = if unsafe { cmd.common.opcode } & 1 != 0 {
            Command::DriverOut
        } else {
            Command::DriverIn
        };
        let mut rq = mq.alloc_sync_request(op)?;
        cmd.common.command_id = rq.tag() as u16;
        unsafe { rq.data_ref().cmd.get().write(cmd) };

        rq.get_pin_mut().execute(false)?;

        let pdu = rq.data_ref();
        if pdu.status.load(Ordering::Relaxed) != 0 {
            Err(EIO)
        } else {
            Ok(pdu.result.load(Ordering::Relaxed))
        }
    }

    fn set_queue_count(
        count: u32,
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
    ) -> Result<u32> {
        let q_count = (count - 1) | ((count - 1) << 16);
        let res = Self::set_features(mq, NVME_FEAT_NUM_QUEUES, q_count, 0)?;
        Ok(core::cmp::min(res & 0xffff, res >> 16) + 1)
    }

    fn alloc_completion_queue<T: mq::Operations<RequestData = NvmeRequest<T>>>(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        queue: &nvme_queue::NvmeQueue<T>,
        vector: u16,
    ) -> Result<u32> {
        let mut flags = NVME_QUEUE_PHYS_CONTIG;
        if !queue.polled {
            flags |= NVME_CQ_IRQ_ENABLED;
        }

        Self::submit_sync_command(
            mq,
            NvmeCommand {
                create_cq: NvmeCreateCq {
                    opcode: NvmeAdminOpcode::create_cq as _,
                    prp1: queue.cq.dma_handle().into(),
                    cqid: queue.qid.into(),
                    qsize: (queue.q_depth - 1).into(),
                    cq_flags: flags.into(),
                    irq_vector: vector.into(),
                    ..NvmeCreateCq::default()
                },
            },
        )
    }

    fn alloc_submission_queue<T: mq::Operations<RequestData = NvmeRequest<T>>>(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        queue: &nvme_queue::NvmeQueue<T>,
    ) -> Result<u32> {
        Self::submit_sync_command(
            mq,
            NvmeCommand {
                create_sq: NvmeCreateSq {
                    opcode: NvmeAdminOpcode::create_sq as _,
                    prp1: queue.sq.dma_handle().into(),
                    sqid: queue.qid.into(),
                    qsize: (queue.q_depth - 1).into(),
                    sq_flags: (NVME_QUEUE_PHYS_CONTIG | NVME_SQ_PRIO_MEDIUM).into(),
                    cqid: queue.qid.into(),
                    ..NvmeCreateSq::default()
                },
            },
        )
    }

    fn identify(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        nsid: u32,
        cns: u32,
        dma_addr: u64,
    ) -> Result<u32> {
        Self::submit_sync_command(
            mq,
            NvmeCommand {
                identify: NvmeIdentify {
                    opcode: NvmeAdminOpcode::identify as _,
                    nsid: nsid.into(),
                    prp1: dma_addr.into(),
                    cns: cns.into(),
                    ..NvmeIdentify::default()
                },
            },
        )
    }

    fn get_features(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        fid: u32,
        nsid: u32,
        dma_addr: u64,
    ) -> Result<u32> {
        Self::submit_sync_command(
            mq,
            NvmeCommand {
                features: NvmeFeatures {
                    opcode: NvmeAdminOpcode::get_features as _,
                    nsid: nsid.into(),
                    prp1: dma_addr.into(),
                    fid: fid.into(),
                    ..NvmeFeatures::default()
                },
            },
        )
    }

    fn set_features(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        fid: u32,
        dword11: u32,
        dma_addr: u64,
    ) -> Result<u32> {
        pr_info!("fid: {}, dma: {}, dword11: {}\n", fid, dma_addr, dword11);
        let ret = Self::submit_sync_command(
            mq,
            NvmeCommand {
                features: NvmeFeatures {
                    opcode: NvmeAdminOpcode::set_features as _,
                    prp1: dma_addr.into(),
                    fid: fid.into(),
                    dword11: dword11.into(),
                    ..NvmeFeatures::default()
                },
            },
        );
        pr_info!("Set features done!");
        ret
    }

    #[allow(dead_code)]
    fn dbbuf_set(
        mq: &mq::RequestQueue<nvme_mq::AdminQueueOperations>,
        dbs_dma_addr: u64,
        eis_dma_addr: u64,
    ) -> Result<u32> {
        Self::submit_sync_command(
            mq,
            NvmeCommand {
                common: NvmeCommon {
                    opcode: NvmeAdminOpcode::dbbuf as _,
                    prp1: dbs_dma_addr.into(),
                    prp2: eis_dma_addr.into(),
                    ..NvmeCommon::default()
                },
            },
        )
    }
}

// TODO: Cannot put () instead of IdInfo?
// TODO: Comma at end of array?
kernel::pci_device_table!(
    PCI_TABLE,
    MODULE_PCI_TABLE,
    <NvmeDevice as pci::Driver>::IdInfo,
    [(
        pci::DeviceId::from_class(bindings::PCI_CLASS_STORAGE_EXPRESS, 0xffffff),
        ()
    )]
);

impl pci::Driver for NvmeDevice {
    type IdInfo = ();

    const ID_TABLE: pci::IdTable<Self::IdInfo> = &PCI_TABLE;

    fn probe(pci_dev: &pci::Device<device::Core>, _id: &Self::IdInfo) -> impl PinInit<Self, Error> {
        pr_info!("probe called!\n");

        let init = || {
            // TODO: We need to disable the device on error.
            pci_dev.enable_device_mem()?;
            pci_dev.set_master();
            unsafe { pci_dev.dma_set_mask(DmaMask::new::<64>()) }?;
            unsafe { pci_dev.dma_set_coherent_mask(DmaMask::new::<64>()) }?;

            // TODO: Set the right mask.
            let dev: ARef<_> = pci_dev.as_ref().into();

            // TODO: We need to release resources on failure.
            let bar = KBox::pin_init(
                pci_dev.iomap_region_sized::<8192>(0, c_str!("rnvme:controller")),
                GFP_KERNEL,
            )?;

            let param_irq_queue_count = *module_parameters::irq_queue_count.value();
            let param_poll_queue_count = *module_parameters::poll_queue_count.value();
            let irq_queue_count: u32 = if param_irq_queue_count == -1 {
                kernel::num_possible_cpus()
            } else {
                param_irq_queue_count as u32
            };

            let poll_queue_count: u32 = if param_poll_queue_count == -1 {
                kernel::num_possible_cpus()
            } else {
                param_poll_queue_count as u32
            };

            pr_info!(
                "queues irq/polled: {}/{}",
                irq_queue_count,
                poll_queue_count
            );

            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            // TODO: this may overflow
            if id == u32::MAX {
                return Err(EBUSY);
            }

            let cap = u64::from_le(bar.try_access().unwrap().read64(OFFSET_CAP));
            let dma_pool = dma::Pool::try_new(
                c_str!("prp list page"),
                dev,
                NVME_CTRL_PAGE_SIZE / 8,
                NVME_CTRL_PAGE_SIZE,
                0,
            )
            .unwrap();

            let pci_dev_bound: &pci::Device<Bound> = pci_dev.deref();
            let pci_dev: ARef<pci::Device> = pci_dev.into();
            let pci_dev_clone = pci_dev.clone();
            let mut io_queues = KVec::new();
            io_queues.reserve(
                (poll_queue_count + irq_queue_count).try_into()?,
                flags::GFP_KERNEL,
            )?;

            let data: Arc<NvmeData> = Arc::pin_init(
                try_pin_init!(NvmeData {
                    // TODO: Use typed register access
                    db_stride: 1 << (((cap >> 32) & 0xf) + 2),
                    pci_dev: pci_dev_clone,
                    instance: id,
                    shadow: None,
                    dma_pool,
                    queues <- new_spinlock!(
                        NvmeQueues {
                            admin: None,
                            io: io_queues,
                        }),
                    poll_queue_count: poll_queue_count,
                    irq_queue_count: irq_queue_count,
                    bar,
                    irq_vector_registration <- new_spinlock!(None),
                    dma_vec_mempool: MemPool::new(NVME_MAX_SEGMENTS)?,
                }),
                flags::GFP_KERNEL,
            )?;

            // TODO: Handle initialization on a workqueue
            pr_info!("Setting up admin queue");
            let (admin_nvme_queue, admin_mq) = Self::configure_admin_queue(&data, pci_dev_bound)?;
            pr_info!("Created admin queue\n");
            // TODO: Move this to a function. We should not fail `probe` if this fails.
            // if false {
            //     let dbs = dma::try_alloc_coherent::<u32>(dev, NVME_CTRL_PAGE_SIZE / 4, false)?;
            //     let eis = dma::try_alloc_coherent::<u32>(dev, NVME_CTRL_PAGE_SIZE / 4, false)?;

            //     for i in 0..NVME_CTRL_PAGE_SIZE / 4 {
            //         dbs.write(i, &0);
            //         eis.write(i, &0);
            //     }

            //     if Self::nvme_dbbuf_set(&admin_mq, dbs.dma_handle, eis.dma_handle).is_ok() {
            //         // TODO: Fix this.
            //         let x = unsafe { &mut *(&(**data) as *const NvmeData as *mut NvmeData) };
            //         x.shadow = Some(NvmeShadow { dbs, eis });
            //     } else {
            //         return Err(kernel::error::code::EIO);
            //     }
            // }

            if let Err(e) = Self::dev_add(cap, &data, pci_dev_bound, &admin_nvme_queue, &admin_mq) {
                pr_info!("Probe failed: {:?}\n", e);
                return Err(e);
            }

            pr_info!("Probe succeeded!\n");
            Ok(data)
        };

        try_pin_init!(Self { data: init()? })
    }

    // fn remove(_data: &Self::Data) {
    //     todo!()
    // }
}

static NEXT_ID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

kernel::module_pci_driver! {
    type: NvmeDevice,
    name: "rnvme",
    description: "NVMe PCI driver",
    license: "GPL v2",
    params: {
        irq_queue_count: i64 {
            default: 1,
            description: "Number of irq queues (-1 means num_cpu)",
        },
        poll_queue_count: i64 {
            default: 1,
            description: "Number of polled queues (-1 means num_cpu)",
        },
    },
}
