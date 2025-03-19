// SPDX-License-Identifier: GPL-2.0

#include <linux/platform_device.h>
#include "helpers.h"

__rust_helper void *
rust_helper_platform_get_drvdata(const struct platform_device *pdev)
{
	return platform_get_drvdata(pdev);
}

__rust_helper void
rust_helper_platform_set_drvdata(struct platform_device *pdev, void *data)
{
	platform_set_drvdata(pdev, data);
}
