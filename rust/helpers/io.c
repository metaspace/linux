#include <linux/io.h>

u8 rust_helper_readb(const volatile void __iomem *addr)
{
	return readb(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readb);

u16 rust_helper_readw(const volatile void __iomem *addr)
{
	return readw(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readw);

u32 rust_helper_readl(const volatile void __iomem *addr)
{
	return readl(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readl);

#ifdef CONFIG_64BIT
u64 rust_helper_readq(const volatile void __iomem *addr)
{
	return readq(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readq);
#endif

void rust_helper_writeb(u8 value, volatile void __iomem *addr)
{
	writeb(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writeb);

void rust_helper_writew(u16 value, volatile void __iomem *addr)
{
	writew(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writew);

void rust_helper_writel(u32 value, volatile void __iomem *addr)
{
	writel(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writel);

#ifdef CONFIG_64BIT
void rust_helper_writeq(u64 value, volatile void __iomem *addr)
{
	writeq(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writeq);
#endif

u8 rust_helper_readb_relaxed(const volatile void __iomem *addr)
{
	return readb_relaxed(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readb_relaxed);

u16 rust_helper_readw_relaxed(const volatile void __iomem *addr)
{
	return readw_relaxed(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readw_relaxed);

u32 rust_helper_readl_relaxed(const volatile void __iomem *addr)
{
	return readl_relaxed(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readl_relaxed);

#ifdef CONFIG_64BIT
u64 rust_helper_readq_relaxed(const volatile void __iomem *addr)
{
	return readq_relaxed(addr);
}
EXPORT_SYMBOL_GPL(rust_helper_readq_relaxed);
#endif

void rust_helper_writeb_relaxed(u8 value, volatile void __iomem *addr)
{
	writeb_relaxed(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writeb_relaxed);

void rust_helper_writew_relaxed(u16 value, volatile void __iomem *addr)
{
	writew_relaxed(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writew_relaxed);

void rust_helper_writel_relaxed(u32 value, volatile void __iomem *addr)
{
	writel_relaxed(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writel_relaxed);

#ifdef CONFIG_64BIT
void rust_helper_writeq_relaxed(u64 value, volatile void __iomem *addr)
{
	writeq_relaxed(value, addr);
}
EXPORT_SYMBOL_GPL(rust_helper_writeq_relaxed);
#endif
