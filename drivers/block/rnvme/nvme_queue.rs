use crate::NvmeCommand;
use crate::NvmeCompletion;
use crate::NvmeData;
use crate::NvmeRequest;
use core;
use core::sync::atomic::fence;
use core::sync::atomic::AtomicU16;
use core::sync::atomic::Ordering;
use kernel::alloc::flags;
use kernel::block::mq;
use kernel::block::mq::IoCompletionBatch;
use kernel::device;
use kernel::device::Bound;
use kernel::dma;
use kernel::dma_read;
use kernel::dma_write;
use kernel::irq;
use kernel::new_spinlock;
use kernel::pci;
use kernel::pci::IrqVector;
use kernel::pr_info;
use kernel::pr_warn;
use kernel::prelude::*;
use kernel::sync::Arc;
use kernel::sync::SpinLock;
use kernel::transmute::AsBytes;
use kernel::transmute::FromBytes;

struct NvmeQueueInner<T: mq::Operations<RequestData = NvmeRequest<T>> + 'static> {
    sq_tail: u16,
    last_sq_tail: u16,
    irq: Option<Pin<KBox<irq::Registration<IrqHandler<T>>>>>,
}

#[pin_data]
pub(crate) struct NvmeQueue<T: mq::Operations<RequestData = NvmeRequest<T>> + 'static> {
    pub(crate) data: Arc<NvmeData>,
    pub(crate) db_offset: usize,
    pub(crate) sdb_index: usize,
    pub(crate) qid: u16,
    pub(crate) polled: bool,

    cq_head: AtomicU16,
    cq_phase: AtomicU16,

    pub(crate) sq: dma::CoherentAllocation<NvmeCommand, dma::CoherentAllocator>,
    pub(crate) cq: dma::CoherentAllocation<NvmeCompletion, dma::CoherentAllocator>,

    pub(crate) q_depth: u16,

    #[pin]
    inner: SpinLock<NvmeQueueInner<T>>,
    tagset: Arc<mq::TagSet<T>>,
}

impl<T> NvmeQueue<T>
where
    T: mq::Operations<RequestData = NvmeRequest<T>>,
{
    pub(crate) fn try_new(
        data: Arc<NvmeData>,
        dev: &pci::Device<device::Bound>,
        qid: u16,
        depth: u16,
        tagset: Arc<mq::TagSet<T>>,
        polled: bool,
    ) -> Result<Arc<Self>> {
        let cq = dma::CoherentAllocator::alloc_coherent::<NvmeCompletion>(
            dev.as_ref().into(),
            depth.into(),
            flags::GFP_KERNEL,
        )?;
        let sq = dma::CoherentAllocator::alloc_coherent(
            dev.as_ref().into(),
            depth.into(),
            flags::GFP_KERNEL,
        )?;

        // Zero out all completions. This is necessary so that we can check the phase.
        for i in 0..depth {
            dma_write!(cq[i.into()] = NvmeCompletion::default())?;
        }

        let sdb_offset = (qid as usize) * data.db_stride * 2;
        let db_offset = sdb_offset + 4096;
        let queue = Arc::pin_init(
            pin_init!( Self {
                data,
                db_offset,
                sdb_index: sdb_offset / 4,
                qid,
                sq,
                cq,
                q_depth: depth,
                tagset,
                cq_head: AtomicU16::new(0),
                cq_phase: AtomicU16::new(1),
                // SAFETY: `spinlock_init` is called below.
                inner <- new_spinlock!(NvmeQueueInner {
                    sq_tail: 0,
                    last_sq_tail: 0,
                    irq: None,
                }),
                polled,
            }),
            flags::GFP_KERNEL,
        )?;

        Ok(queue)
    }

    /// Processes the completion queue.
    ///
    /// Returns `true` if at least one entry was processed, `false` otherwise.
    pub(crate) fn process_completions(&self, mut batch: Option<&mut IoCompletionBatch<T>>) -> bool {
        let mut head = self.cq_head.load(Ordering::Relaxed);
        let mut phase = self.cq_phase.load(Ordering::Relaxed);
        let mut found = 0;

        loop {
            let cqe = dma_read!(self.cq[head.into()]).unwrap();

            if cqe.status.into() & 1 != phase {
                break;
            }

            let cqe = dma_read!(self.cq[head.into()]).unwrap();

            found += 1;
            head += 1;
            if head == self.q_depth {
                head = 0;
                phase ^= 1;
            }

            if let Some(rq) = self
                .tagset
                .tag_to_rq(self.qid.saturating_sub(1).into(), cqe.command_id.into())
            {
                let pdu = rq.data_ref();
                pdu.result.store(cqe.result.into(), Ordering::Relaxed);
                let status = cqe.status.into() >> 1;
                pdu.status.store(status, Ordering::Relaxed);
                if let Some(ref mut batch) = batch {
                    if let Err(rq) = batch.add_request(rq, status != 0) {
                        kernel::block::mq::Request::complete(rq);
                    }
                } else {
                    kernel::block::mq::Request::complete(rq);
                }
            } else {
                let command_id = cqe.command_id;
                pr_warn!("invalid id completed: {}\n", command_id);
            }
        }

        if found == 0 {
            return false;
        }

        if self.dbbuf_update_and_check_event(head.into(), self.data.db_stride / 4) {
            if let Some(bar) = self.data.bar.try_access() {
                let _ = bar.try_write32(head.into(), self.db_offset + self.data.db_stride);
            }
        }

        // TODO: Comment on why it's ok.
        self.cq_head.store(head, Ordering::Relaxed);
        self.cq_phase.store(phase, Ordering::Relaxed);

        true
    }

    pub(crate) fn dbbuf_need_event(event_idx: u16, new_idx: u16, old: u16) -> bool {
        new_idx.wrapping_sub(event_idx).wrapping_sub(1) < new_idx.wrapping_sub(old)
    }

    pub(crate) fn dbbuf_update_and_check_event(&self, value: u16, extra_index: usize) -> bool {
        if self.qid == 0 {
            return true;
        }

        let shadow = if let Some(s) = &self.data.shadow {
            s
        } else {
            return true;
        };

        let index = self.sdb_index + extra_index;

        // TODO: This should be a wmb (sfence on x86-64).
        // Ensure that the queue is written before updating the doorbell in memory.
        fence(Ordering::SeqCst);

        let old_value = dma_read!(shadow.dbs[index]).unwrap();
        dma_write!(shadow.dbs[index] = value.into()).unwrap();

        // Ensure that the doorbell is updated before reading the event index from memory. The
        // controller needs to provide similar ordering to ensure the envent index is updated
        // before reading the doorbell.
        fence(Ordering::SeqCst);

        let ei = dma_read!(shadow.eis[index]).unwrap();
        Self::dbbuf_need_event(ei as _, value, old_value as _)
    }

    pub(crate) fn write_sq_db(&self, write_sq: bool) {
        //let mut inner = self.inner.lock_irqdisable();
        // TODO: irqdisable
        let mut inner = self.inner.lock();
        self.write_sq_db_locked(write_sq, &mut inner);
    }

    fn write_sq_db_locked(&self, write_sq: bool, inner: &mut NvmeQueueInner<T>) {
        if !write_sq {
            let mut next_tail = inner.sq_tail + 1;
            if next_tail == self.q_depth {
                next_tail = 0;
            }
            if next_tail != inner.last_sq_tail {
                return;
            }
        }

        if self.dbbuf_update_and_check_event(inner.sq_tail, 0) {
            if let Some(bar) = self.data.bar.try_access() {
                let _ = bar.try_write32(inner.sq_tail.into(), self.db_offset);
            }
        }
        inner.last_sq_tail = inner.sq_tail;
    }

    pub(crate) fn submit_command(&self, cmd: &NvmeCommand, is_last: bool) {
        // TODO: irqdisable
        let mut inner = self.inner.lock();
        let _ = dma_write!(self.sq[inner.sq_tail.into()] = *cmd);
        inner.sq_tail += 1;
        if inner.sq_tail == self.q_depth {
            inner.sq_tail = 0;
        }
        self.write_sq_db_locked(is_last, &mut inner);
    }

    pub(crate) fn unregister_irq(&self) {
        // Do not drop registration while spinlock is held, irq::free will take
        // a mutex and might sleep.
        // TODO: irqdisable
        let registration = self.inner.lock().irq.take();
        drop(registration);
    }

    pub(crate) fn register_irq(
        self: &Arc<Self>,
        pci_dev: &pci::Device<Bound>,
        vector: IrqVector<'_>,
    ) -> Result {
        pr_info!(
            "Registering irq for queue qid: {}, vector {}\n",
            self.qid,
            vector.index(),
        );

        //let name = CString::try_from_fmt(format_args!("nvme{}q{}", self.data.instance, self.qid))?;
        let irq_registration = pci_dev.request_irq(
            vector,
            // TODO: Should be part of PCI?
            irq::Flags::SHARED,
            c"rnvme",
            try_pin_init!(IrqHandler {
                queue: self.clone(),
            }),
        );
        let irq_registration = KBox::try_pin_init(irq_registration, GFP_KERNEL)?;

        self.inner.lock().irq.replace(irq_registration);

        Ok(())
    }
}

#[pin_data]
struct IrqHandler<T: mq::Operations<RequestData = NvmeRequest<T>> + 'static> {
    queue: Arc<NvmeQueue<T>>,
}

impl<T> irq::Handler for IrqHandler<T>
where
    T: mq::Operations<RequestData = NvmeRequest<T>> + 'static,
{
    fn handle(&self, _device: &device::Device<device::Bound>) -> irq::IrqReturn {
        if self.queue.process_completions(None) {
            irq::IrqReturn::Handled
        } else {
            irq::IrqReturn::None
        }
    }
}

unsafe impl kernel::transmute::AsBytes for NvmeCompletion {}
unsafe impl kernel::transmute::FromBytes for NvmeCompletion {}
unsafe impl kernel::transmute::AsBytes for NvmeCommand {}
unsafe impl kernel::transmute::FromBytes for NvmeCommand {}
unsafe impl<T: FromBytes> kernel::transmute::FromBytes for crate::nvme_defs::le<T> {}
unsafe impl<T: AsBytes> kernel::transmute::AsBytes for crate::nvme_defs::le<T> {}
