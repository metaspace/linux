#include <linux/bio.h>
#include <linux/blk-mq.h>
#include <linux/blkdev.h>

__attribute__((always_inline))
void rust_helper_bio_advance_iter_single(const struct bio *bio,
                                           struct bvec_iter *iter, unsigned int bytes)
{
	bio_advance_iter_single(bio, iter, bytes);
}

__attribute__((always_inline)) void *rust_helper_kmap(struct page *page)
{
	return kmap(page);
}

__attribute__((always_inline)) void rust_helper_kunmap(struct page *page)
{
	return kunmap(page);
}

__attribute__((always_inline)) void *rust_helper_kmap_atomic(struct page *page)
{
	return kmap_atomic(page);
}

__attribute__((always_inline)) void rust_helper_kunmap_atomic(void *address)
{
	kunmap_atomic(address);
}

__attribute__((always_inline)) struct page *
rust_helper_alloc_pages(gfp_t gfp_mask, unsigned int order)
{
	return alloc_pages(gfp_mask, order);
}

__attribute__((always_inline)) void rust_helper_spin_lock_irq(spinlock_t *lock)
{
	spin_lock_irq(lock);
}

__attribute__((always_inline)) void
rust_helper_spin_unlock_irq(spinlock_t *lock)
{
	spin_unlock_irq(lock);
}
__attribute__((always_inline)) unsigned long
rust_helper_spin_lock_irqsave(spinlock_t *lock)
{
	unsigned long flags;

	spin_lock_irqsave(lock, flags);

	return flags;
}
__attribute__((always_inline)) void
rust_helper_spin_unlock_irqrestore(spinlock_t *lock, unsigned long flags)
{
	spin_unlock_irqrestore(lock, flags);
}

__attribute__((always_inline)) struct request *
rust_helper_blk_mq_rq_from_pdu(void *pdu)
{
	return blk_mq_rq_from_pdu(pdu);
}

__attribute__((always_inline)) void *
rust_helper_blk_mq_rq_to_pdu(struct request *rq)
{
	return blk_mq_rq_to_pdu(rq);
}

__attribute__((always_inline)) struct page *
rust_helper_folio_page(struct folio *folio, size_t n)
{
	return folio_page(folio, n);
}

__attribute__((always_inline)) bool rust_helper_IS_ERR(__force const void *ptr)
{
	return IS_ERR(ptr);
}

__attribute__((always_inline)) void *
rust_helper_kmap_local_page(struct page *page)
{
	return kmap_local_page(page);
}

__attribute__((always_inline)) void rust_helper_kunmap_local(const void *addr)
{
	kunmap_local(addr);
}

__attribute__((always_inline)) long rust_helper_PTR_ERR(__force const void *ptr)
{
	return PTR_ERR(ptr);
}

__attribute__((always_inline)) bool
rust_helper_refcount_dec_and_test(refcount_t *r)
{
	return refcount_dec_and_test(r);
}

__attribute__((always_inline)) bool
rust_helper_req_ref_inc_not_zero(struct request *req)
{
	return atomic_inc_not_zero(&req->ref);
}

__attribute__((always_inline)) bool
rust_helper_req_ref_put_and_test(struct request *req)
{
	return atomic_dec_and_test(&req->ref);
}

__attribute__((always_inline)) int rust_helper_xa_err(void *entry)
{
	return xa_err(entry);
}

__attribute__((always_inline)) void rust_helper_xa_lock(struct xarray *xa)
{
	xa_lock(xa);
}

__attribute__((always_inline)) void rust_helper_xa_unlock(struct xarray *xa)
{
	xa_unlock(xa);
}

__attribute__((always_inline)) struct folio *
rust_helper_folio_alloc(gfp_t gfp, unsigned int order)
{
	return folio_alloc(gfp, order);
}

__attribute__((always_inline)) void rust_helper_folio_put(struct folio *folio)
{
	folio_put(folio);
}
