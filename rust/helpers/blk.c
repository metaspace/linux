// SPDX-License-Identifier: GPL-2.0

#include <linux/bio.h>
#include <linux/blk-mq.h>
#include <linux/blkdev.h>

void *rust_helper_blk_mq_rq_to_pdu(struct request *rq)
{
	return blk_mq_rq_to_pdu(rq);
}

struct request *rust_helper_blk_mq_rq_from_pdu(void *pdu)
{
	return blk_mq_rq_from_pdu(pdu);
}

void rust_helper_bio_advance_iter_single(const struct bio *bio,
					 struct bvec_iter *iter,
					 unsigned int bytes)
{
	bio_advance_iter_single(bio, iter, bytes);
}

bool rust_helper_blk_mq_add_to_batch(struct request *req,
				     struct io_comp_batch *iob, bool is_error,
				     void (*complete)(struct io_comp_batch *))
{
	return blk_mq_add_to_batch(req, iob, is_error, complete);
}

__rust_helper struct request *rust_helper_rq_list_pop(struct rq_list *rl)
{
	return rq_list_pop(rl);
}

__rust_helper int rust_helper_rq_list_empty(const struct rq_list *rl)
{
	return rq_list_empty(rl);
}

__rust_helper void rust_helper_rq_list_add_tail(struct rq_list *rl,
						struct request *rq)
{
	rq_list_add_tail(rl, rq);
}

__rust_helper void rust_helper_rq_list_init(struct rq_list *rl)
{
	rq_list_init(rl);
}

__rust_helper struct request *rust_helper_rq_list_peek(struct rq_list *rl)
{
	return rq_list_peek(rl);
}

__rust_helper struct request *
rust_helper_blk_mq_tag_to_rq(struct blk_mq_tags *tags, unsigned int tag)
{
	return blk_mq_tag_to_rq(tags, tag);
}
