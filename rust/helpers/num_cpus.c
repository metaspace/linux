// SPDX-License-Identifier: GPL-2.0

#include <linux/cpumask.h>

unsigned int rust_helper_num_possible_cpus(void)
{
	return  num_possible_cpus();
}
