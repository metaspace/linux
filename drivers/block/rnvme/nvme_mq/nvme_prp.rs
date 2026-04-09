use crate::le;
use crate::npages_prp;
use crate::nvme_driver_defs::*;
use crate::NvmeCommand;
use crate::NvmeData;
use core::mem::ManuallyDrop;
use kernel::block::mq::dma_map_iter::DmaMapIter;
use kernel::block::mq::Operations;
use kernel::dma;
use kernel::dma_read;
use kernel::dma_write;
use kernel::prelude::*;
use kernel::sync::Arc;
use kernel::types::ScopeGuard;

pub(crate) fn free_prps(
    count: usize,
    pages: &[usize],
    first_dma: u64,
    dma_pool: &Arc<dma::Pool<le<u64>>>,
) {
    let mut dma_addr = first_dma;
    for page in &pages[..count] {
        let prp_list = unsafe {
            dma::CoherentAllocation::<le<u64>, dma::Pool<le<u64>>>::from_parts(
                dma_pool,
                *page,
                dma_addr,
                NVME_CTRL_PAGE_SIZE / 8,
            )
        };

        dma_addr = dma_read!(prp_list[NVME_CTRL_PAGE_SIZE / 8 - 1])
            .unwrap()
            .into();
    }
}

pub(crate) fn setup_prps<T: Operations>(
    data: &NvmeData,
    cmd: &mut NvmeCommand,
    dma_map_iter: &mut DmaMapIter<NVME_MAX_SEGMENTS, T>,
    prp_mapping_data: &mut [usize; npages_prp()],
    mut length: u32,
) -> Result<u32> {
    let mut segment_addr = dma_map_iter.address();
    let mut segment_len = dma_map_iter.length();
    let offset = segment_addr & ((NVME_CTRL_PAGE_SIZE - 1) as u64);

    let consumed = ((NVME_CTRL_PAGE_SIZE as u64) - offset) as u32;

    cmd.common.prp1 = segment_addr.into();
    length = length.saturating_sub(consumed);
    if length == 0 {
        return Ok(0);
    }

    segment_len = segment_len.saturating_sub(consumed);
    if segment_len != 0 {
        segment_addr += consumed as u64;
    } else {
        dma_map_iter.next().expect("Expected to get a segment");
        segment_addr = dma_map_iter.address();
        segment_len = dma_map_iter.length();
    }

    if length <= NVME_CTRL_PAGE_SIZE as u32 {
        cmd.common.prp2 = segment_addr.into();
        return Ok(0);
    }

    let mut prp_list = ManuallyDrop::new(data.dma_pool.try_alloc(true)?);

    cmd.common.prp2 = prp_list.dma_handle().into();
    prp_mapping_data[0] = prp_list.start_ptr() as usize;
    struct Data<'a> {
        page_count: usize,
        pages: &'a mut [usize],
        first_dma: u64,
    }
    let mut guard = ScopeGuard::new_with_data(
        Data {
            page_count: 1,
            pages: prp_mapping_data,
            first_dma: prp_list.dma_handle(),
        },
        |g| {
            free_prps(g.page_count, g.pages, g.first_dma, &data.dma_pool);
        },
    );

    let mut j = 0;
    let mut last_dma_addr = 0;
    loop {
        if j == NVME_CTRL_PAGE_SIZE / 8 {
            let new_prp_list = ManuallyDrop::new(data.dma_pool.try_alloc(true)?);
            dma_write!(
                prp_list[NVME_CTRL_PAGE_SIZE / 8 - 1] =
                    new_prp_list.dma_handle().into()
            )?;
            dma_write!(new_prp_list[0] = last_dma_addr.into())?;
            prp_list = new_prp_list;
            let next = guard.page_count;
            guard.pages[next] = prp_list.start_ptr() as usize;
            guard.page_count += 1;
            j = 1;
        }
        last_dma_addr = segment_addr;
        dma_write!(prp_list[j] = segment_addr.into())?;
        j += 1;

        length = length.saturating_sub(NVME_CTRL_PAGE_SIZE as u32);
        if length == 0 {
            break;
        }

        if segment_len > NVME_CTRL_PAGE_SIZE as u32 {
            segment_addr += NVME_CTRL_PAGE_SIZE as u64;
            segment_len -= NVME_CTRL_PAGE_SIZE as u32;
            continue;
        }

        if segment_len < NVME_CTRL_PAGE_SIZE as u32 {
            // TODO: Write warning.
            return Err(EIO);
        }

        dma_map_iter.next().expect("Expected more segments");
        segment_addr = dma_map_iter.address();
        segment_len = dma_map_iter.length();
    }

    Ok(guard.dismiss().page_count as _)
}
