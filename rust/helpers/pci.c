// SPDX-License-Identifier: GPL-2.0

#include <linux/pci.h>
#include "helpers.h"

__rust_helper void rust_helper_pci_set_drvdata(struct pci_dev *pdev, void *data)
{
	pci_set_drvdata(pdev, data);
}

__rust_helper void *rust_helper_pci_get_drvdata(struct pci_dev *pdev)
{
	return pci_get_drvdata(pdev);
}

__rust_helper resource_size_t rust_helper_pci_resource_len(struct pci_dev *pdev,
							   int bar)
{
	return pci_resource_len(pdev, bar);
}
