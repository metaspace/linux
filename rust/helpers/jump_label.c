// SPDX-License-Identifier: GPL-2.0

/*
 * Copyright (C) 2024 Google LLC.
 */

#include <linux/jump_label.h>
#include "helpers.h"

#ifndef CONFIG_JUMP_LABEL
__rust_helper int rust_helper_static_key_count(struct static_key *key)
{
	return static_key_count(key);
}
#endif
