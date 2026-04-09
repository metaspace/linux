// SPDX-License-Identifier: GPL-2.0

#include <linux/delay.h>

void rust_helper_mdelay(uint64_t ms)
{
	mdelay(ms);
}
