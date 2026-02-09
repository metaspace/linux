// SPDX-License-Identifier: GPL-2.0

#include <linux/xarray.h>

int rust_helper_xa_err(void *entry)
{
	return xa_err(entry);
}

void rust_helper_xa_init_flags(struct xarray *xa, gfp_t flags)
{
	return xa_init_flags(xa, flags);
}

int rust_helper_xa_trylock(struct xarray *xa)
{
	return xa_trylock(xa);
}

void rust_helper_xa_lock(struct xarray *xa)
{
	return xa_lock(xa);
}

void rust_helper_xa_unlock(struct xarray *xa)
{
	return xa_unlock(xa);
}

void *rust_helper_xas_result(struct xa_state *xas, void *curr)
{
	if (xa_err(xas->xa_node))
		curr = xas->xa_node;
	return curr;
}

void *rust_helper_xa_zero_to_null(void *entry)
{
	return xa_is_zero(entry) ? NULL : entry;
}

int rust_helper_xas_error(const struct xa_state *xas)
{
	return xas_error(xas);
}
