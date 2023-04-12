
obj-$(CONFIG_BLK_DEV_RUST_NULL) += rnull_mod.o
rnull_mod-y := rnull.o $(subst m, rnull-helpers.o, $(findstring m, $(CONFIG_BLK_DEV_RUST_NULL)))
LLVM_LINK_FIX_drivers/block/rnull/rnull_mod.o := 1
