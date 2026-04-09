use super::nvme_defs::*;
use super::nvme_queue::NvmeQueue;
use super::MappingData;
use super::NvmeCommand;
use super::NvmeData;
use super::NvmeNamespace;
use super::NvmeRequest;
use crate::npages_prp;
use core;
use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering};
use kernel::alloc::KBox;
use kernel::bindings;
use kernel::block::error::BlkResult;
use kernel::block::mq;
use kernel::block::mq::Command;
use kernel::block::mq::IoCompletionBatch;
use kernel::error::code::*;
use kernel::new_spinlock;
use kernel::pr_info;
use kernel::prelude::*;
use kernel::sync::Arc;
use kernel::sync::ArcBorrow;
use kernel::types::ARef;
use kernel::types::ForeignOwnable;
use kernel::types::OwnableRefCounted;
use kernel::types::Owned;
use nvme_prp::*;

pub(crate) mod nvme_prp;

pub(crate) struct AdminQueueOperations;

#[kernel::macros::vtable]
impl mq::Operations for AdminQueueOperations {
    type RequestData = NvmeRequest<Self>;
    type QueueData = KBox<NvmeNamespace>;
    type HwData = Arc<NvmeQueue<Self>>;
    type TagSetData = Arc<NvmeData>;

    fn new_request_data(
        tagset_data: <Self::TagSetData as ForeignOwnable>::Borrowed<'_>,
    ) -> impl PinInit<Self::RequestData> {
        // TODO: Can't have these clones inside `pin_init!`, why?
        let device = tagset_data.pci_dev.as_ref().into();
        let dma_pool = tagset_data.dma_pool.clone();

        pin_init!(NvmeRequest {
            result: AtomicU32::new(0),
            status: AtomicU16::new(0),
            direction: AtomicI32::new(bindings::dma_data_direction_DMA_FROM_DEVICE),
            len: AtomicU32::new(0),
            dev: device,
            cmd: SyncUnsafeCell::new(NvmeCommand::default()),
            sg_count: AtomicU32::new(0),
            page_count: AtomicU32::new(0),
            first_dma: AtomicU64::new(0),
            mapping_data <- new_spinlock!(None),
            dma_pool: dma_pool,
        })
    }

    fn queue_rq(
        hw_data: ArcBorrow<'_, NvmeQueue<Self>>,
        queue_data: &NvmeNamespace,
        rq: Owned<mq::IdleRequest<Self>>,
        is_last: bool,
    ) -> BlkResult {
        queue_rq(hw_data, queue_data, rq, is_last)
    }

    fn complete(rq: ARef<mq::Request<Self>>) {
        complete(rq)
    }

    fn commit_rqs(
        queue: <Self::HwData as ForeignOwnable>::Borrowed<'_>,
        _ns: <Self::QueueData as ForeignOwnable>::Borrowed<'_>,
    ) {
        queue.write_sq_db(true);
    }

    fn init_hctx(
        tagset_data: <Self::TagSetData as ForeignOwnable>::Borrowed<'_>,
        _hctx_idx: u32,
    ) -> Result<Self::HwData> {
        let queues = tagset_data.queues.lock();
        Ok(queues.admin.as_ref().ok_or(EINVAL)?.clone())
    }
}

pub(crate) struct IoQueueOperations;

#[kernel::macros::vtable]
impl mq::Operations for IoQueueOperations {
    type RequestData = NvmeRequest<Self>;
    type QueueData = KBox<NvmeNamespace>;
    type HwData = Arc<NvmeQueue<Self>>;
    type TagSetData = Arc<NvmeData>;

    fn new_request_data(
        tagset_data: <Self::TagSetData as ForeignOwnable>::Borrowed<'_>,
    ) -> impl PinInit<Self::RequestData> {
        let device = tagset_data.pci_dev.as_ref().into();
        let dma_pool = tagset_data.dma_pool.clone();

        pin_init!(NvmeRequest {
            result: AtomicU32::new(0),
            status: AtomicU16::new(0),
            direction: AtomicI32::new(bindings::dma_data_direction_DMA_FROM_DEVICE),
            len: AtomicU32::new(0),
            dev: device,
            cmd: SyncUnsafeCell::new(NvmeCommand::default()),
            sg_count: AtomicU32::new(0),
            page_count: AtomicU32::new(0),
            first_dma: AtomicU64::new(0),
            mapping_data <- new_spinlock!(None),
            dma_pool: dma_pool,
        })
    }

    fn init_hctx(
        tagset_data: ArcBorrow<'_, NvmeData>,
        hctx_idx: u32,
    ) -> Result<Arc<NvmeQueue<Self>>> {
        let queues = tagset_data.queues.lock();
        Ok(queues.io[hctx_idx as usize].clone())
    }

    fn queue_rq(
        io_queue: ArcBorrow<'_, NvmeQueue<Self>>,
        ns: &NvmeNamespace,
        rq: Owned<mq::IdleRequest<Self>>,
        is_last: bool,
    ) -> BlkResult {
        queue_rq(io_queue, ns, rq, is_last)
    }

    fn complete(rq: ARef<mq::Request<Self>>) {
        complete(rq)
    }

    fn commit_rqs(io_queue: ArcBorrow<'_, NvmeQueue<Self>>, _ns: &NvmeNamespace) {
        io_queue.write_sq_db(true);
    }

    fn poll(
        hw_data: ArcBorrow<'_, NvmeQueue<Self>>,
        _queue_data: &NvmeNamespace,
        batch: &mut IoCompletionBatch<Self>,
    ) -> Result<bool> {
        Ok(hw_data.process_completions(Some(batch)))
    }

    fn batch_complete(rq: ARef<mq::Request<Self>>) {
        let pdu = rq.data_ref();
        let irq_state: usize = unsafe { bindings::local_irq_save() };
        let mapping_data = pdu.mapping_data.lock().take();
        unsafe { bindings::local_irq_restore(irq_state) };
        if let Some(md) = mapping_data {
            crate::nvme_mq::nvme_prp::free_prps(
                pdu.page_count.load(Ordering::Relaxed) as _,
                &md.prp_data,
                pdu.first_dma.load(Ordering::Relaxed),
                &pdu.dma_pool,
            );
            drop(md);
        }
        // TODO: error handling
    }

    fn map_queues(tag_set: Pin<&mut mq::TagSet<Self>>) {
        // TODO: Build abstractions for these unsafe calls
        unsafe {
            let device_data: Self::TagSetData =
                Self::TagSetData::from_foreign((*tag_set.raw_tag_set()).driver_data.cast());
            let num_maps = (*tag_set.raw_tag_set()).nr_maps;
            pr_info!("num_maps: {}\n", num_maps);
            let mut queue_offset: u32 = 0;
            let mut irq_offset: u32 = 1; //TODO: Unless we only have 1 vector
            for i in 0..num_maps {
                let queue_count = match i {
                    bindings::hctx_type_HCTX_TYPE_DEFAULT => device_data.irq_queue_count,
                    bindings::hctx_type_HCTX_TYPE_POLL => device_data.poll_queue_count,
                    _ => 0,
                };
                let map = &mut (&mut (*tag_set.raw_tag_set()).map)[i as usize];
                map.nr_queues = queue_count;
                if queue_count == 0 {
                    continue;
                }
                map.queue_offset = queue_offset;
                if i != bindings::hctx_type_HCTX_TYPE_POLL && irq_offset != 0 {
                    bindings::blk_mq_map_hw_queues(
                        map,
                        device_data.pci_dev.as_ref().as_raw(),
                        irq_offset,
                    );
                } else {
                    bindings::blk_mq_map_queues(map);
                }
                queue_offset += queue_count;
                irq_offset += queue_count;
            }
        }
        pr_info!("Return from map queues");
    }
}

fn queue_rq<T>(
    io_queue: ArcBorrow<'_, NvmeQueue<T>>,
    ns: &NvmeNamespace,
    rq: Owned<mq::IdleRequest<T>>,
    is_last: bool,
) -> BlkResult
where
    T: mq::Operations<RequestData = NvmeRequest<T>>,
{
    let rq = rq.start();
    match rq.command() {
        Command::DriverIn | Command::DriverOut => {
            let cmd = unsafe { &*rq.data_ref().cmd.get() };
            // TODO: Completion interrupt can arrive while we still hold rq.
            drop(rq);
            io_queue.submit_command(cmd, is_last);
            Ok(())
        }
        Command::Flush => {
            let mut cmd = NvmeCommand::new_flush(ns.id);
            cmd.common.command_id = rq.tag() as u16;
            io_queue.submit_command(&cmd, is_last);
            Ok(())
        }
        Command::Write | Command::Read => {
            let opcode = if rq.command() == Command::Read {
                NvmeOpcode::read
            } else {
                NvmeOpcode::write
            };
            let len = rq.payload_bytes();
            // TODO: Handle unwrap
            let offset = rq.bio().unwrap().raw_iter().bi_sector;
            let mut cmd = NvmeCommand {
                rw: NvmeRw {
                    opcode: opcode as _,
                    command_id: rq.tag() as u16,
                    nsid: ns.id.into(),
                    slba: (offset >> (ns.lba_shift - bindings::SECTOR_SHIFT)).into(),
                    length: ((len >> ns.lba_shift) as u16 - 1).into(),
                    ..NvmeRw::default()
                },
            };

            let rq = OwnableRefCounted::into_shared(rq);
            let mut dma_map_iter = rq.clone().dma_map_iter(
                io_queue.data.pci_dev.as_ref(),
                io_queue.data.dma_vec_mempool.clone(),
            )?;
            let mut prp_mapping_data = [0; npages_prp()];
            let page_count = match setup_prps(
                &io_queue.data,
                &mut cmd,
                &mut dma_map_iter,
                &mut prp_mapping_data,
                len,
            ) {
                Ok(pc) => pc,
                Err(e) => {
                    drop(dma_map_iter);
                    let rq = OwnableRefCounted::try_from_shared(rq)
                        .expect("Expected to reclaim request");
                    core::mem::forget(rq);
                    return Err(e.into());
                }
            };

            let pdu = rq.data_ref();
            pdu.page_count.store(page_count, Ordering::Relaxed);
            pdu.first_dma
                .store(unsafe { cmd.common.prp2.into() }, Ordering::Relaxed);

            let irq_state: usize = unsafe { bindings::local_irq_save() };
            *pdu.mapping_data.lock() = Some(MappingData {
                io_data: dma_map_iter.finish(),
                prp_data: prp_mapping_data,
            });
            unsafe { bindings::local_irq_restore(irq_state) };

            drop(rq);
            io_queue.submit_command(&cmd, is_last);
            Ok(())
        }

        _ => Err(kernel::block::error::code::BLK_STS_IOERR),
    }
}

fn complete<T>(rq: ARef<mq::Request<T>>)
where
    T: mq::Operations<RequestData = NvmeRequest<T>>,
{
    match rq.command() {
        Command::DriverIn | Command::DriverOut | Command::Flush => {
            // We just complete right away if flush completes.
            OwnableRefCounted::try_from_shared(rq)
                .map_err(|_e| kernel::error::code::EIO)
                .expect("Failed to get unique reference\n")
                .end_ok();
            return;
        }
        _ => {}
    }

    let pdu = rq.data_ref();

    let irq_state: usize = unsafe { bindings::local_irq_save() };
    let mapping_data = pdu.mapping_data.lock().take();
    unsafe { bindings::local_irq_restore(irq_state) };
    if let Some(md) = mapping_data {
        crate::nvme_mq::nvme_prp::free_prps(
            pdu.page_count.load(Ordering::Relaxed) as _,
            &md.prp_data,
            pdu.first_dma.load(Ordering::Relaxed),
            &pdu.dma_pool,
        );
        drop(md);
    }

    // On failure, complete the request immediately with an error.
    let status = pdu.status.load(Ordering::Relaxed);

    let rq = OwnableRefCounted::try_from_shared(rq)
        .map_err(|_e| kernel::error::code::EIO)
        .expect("Failed to get unique reference\n");

    if status != 0 {
        rq.end(kernel::block::error::code::BLK_STS_IOERR.to_blk_status());
        return;
    }

    // TODO: Used to loop here.
    rq.end_ok();
}
