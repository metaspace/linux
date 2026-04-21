// SPDX-License-Identifier: GPL-2.0

//! Memory-mapped IO.
//!
//! C header: [`include/asm-generic/io.h`](srctree/include/asm-generic/io.h)

use crate::{
    bindings,
    prelude::*,
    ptr::KnownSize, //
};

pub mod mem;
pub mod poll;
pub mod register;
pub mod resource;

pub use crate::register;
pub use resource::Resource;

use register::LocatedRegister;

/// Physical address type.
///
/// This is a type alias to either `u32` or `u64` depending on the config option
/// `CONFIG_PHYS_ADDR_T_64BIT`, and it can be a u64 even on 32-bit architectures.
pub type PhysAddr = bindings::phys_addr_t;

/// Resource Size type.
///
/// This is a type alias to either `u32` or `u64` depending on the config option
/// `CONFIG_PHYS_ADDR_T_64BIT`, and it can be a u64 even on 32-bit architectures.
pub type ResourceSize = bindings::resource_size_t;

/// Untyped I/O region.
///
/// This type can be used when an I/O region without known type information has a compile-time known
/// minimum size (and a runtime known actual size).
///
/// The `SIZE` generic parameter indicate the minimum size of the region.
#[repr(transparent)]
pub struct Region<const SIZE: usize = 0> {
    inner: [u8],
}

impl<const SIZE: usize> KnownSize for Region<SIZE> {
    const MIN_SIZE: usize = SIZE;

    #[inline(always)]
    fn size(p: *const Self) -> usize {
        (p as *const [u8]).len()
    }
}

/// Raw representation of an MMIO region.
///
/// `MmioRaw<T>` is equivalent to `T __iomem *` in C.
///
/// By itself, the existence of an instance of this structure does not provide any guarantees that
/// the represented MMIO region does exist or is properly mapped.
///
/// Instead, the bus specific MMIO implementation must convert this raw representation into an
/// `Mmio` instance providing the actual memory accessors. Only by the conversion into an `Mmio`
/// structure any guarantees are given.
pub struct MmioRaw<T: ?Sized> {
    /// Pointer is in I/O address space.
    ///
    /// The provenance does not matter, only the address and metadata do.
    addr: *mut T,
}

// SAFETY: `MmioRaw` is just an address, so is thread-safe.
unsafe impl<T: ?Sized> Send for MmioRaw<T> {}
// SAFETY: `MmioRaw` is just an address, so is thread-safe.
unsafe impl<T: ?Sized> Sync for MmioRaw<T> {}

impl<T> MmioRaw<T> {
    /// Create a `MmioRaw` from address.
    #[inline]
    pub fn new(addr: usize) -> Self {
        Self {
            addr: core::ptr::without_provenance_mut(addr),
        }
    }
}

impl<const SIZE: usize> MmioRaw<Region<SIZE>> {
    /// Create a `MmioRaw` representing a I/O region with given size.
    ///
    /// The size is checked against the minimum size specified via const generics.
    #[inline]
    pub fn new_region(addr: usize, size: usize) -> Result<Self> {
        if size < SIZE {
            return Err(EINVAL);
        }

        let addr = core::ptr::slice_from_raw_parts_mut::<u8>(
            core::ptr::without_provenance_mut(addr),
            size,
        ) as *mut Region<SIZE>;
        Ok(Self { addr })
    }
}

impl<T: ?Sized + KnownSize> MmioRaw<T> {
    /// Returns the base address of the MMIO region.
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.addr
    }

    /// Returns the size of the MMIO region.
    #[inline]
    pub fn size(&self) -> usize {
        KnownSize::size(self.addr)
    }
}

/// IO-mapped memory region.
///
/// The creator (usually a subsystem / bus such as PCI) is responsible for creating the
/// mapping, performing an additional region request etc.
///
/// # Invariant
///
/// `addr` is the start and `maxsize` the length of valid I/O mapped memory region of size
/// `maxsize`.
///
/// # Examples
///
/// ```no_run
/// use kernel::{
///     bindings,
///     ffi::c_void,
///     io::{
///         Io,
///         IoKnownSize,
///         Mmio,
///         MmioRaw,
///         PhysAddr,
///         Region,
///     },
/// };
/// use core::ops::Deref;
///
/// // See also `pci::Bar` for a real example.
/// struct IoMem<const SIZE: usize>(MmioRaw<Region<SIZE>>);
///
/// impl<const SIZE: usize> IoMem<SIZE> {
///     /// # Safety
///     ///
///     /// [`paddr`, `paddr` + `SIZE`) must be a valid MMIO region that is mappable into the CPUs
///     /// virtual address space.
///     unsafe fn new(paddr: usize) -> Result<Self>{
///         // SAFETY: By the safety requirements of this function [`paddr`, `paddr` + `SIZE`) is
///         // valid for `ioremap`.
///         let addr = unsafe { bindings::ioremap(paddr as PhysAddr, SIZE) };
///         if addr.is_null() {
///             return Err(ENOMEM);
///         }
///
///         Ok(IoMem(MmioRaw::new_region(addr as usize, SIZE)?))
///     }
/// }
///
/// impl<const SIZE: usize> Drop for IoMem<SIZE> {
///     fn drop(&mut self) {
///         // SAFETY: `self.0.addr()` is guaranteed to be properly mapped by `Self::new`.
///         unsafe { bindings::iounmap(self.0.as_ptr().cast()); };
///     }
/// }
///
/// impl<const SIZE: usize> Deref for IoMem<SIZE> {
///    type Target = Mmio<Region<SIZE>>;
///
///    fn deref(&self) -> &Self::Target {
///         // SAFETY: The memory range stored in `self` has been properly mapped in `Self::new`.
///         unsafe { Mmio::from_raw(&self.0) }
///    }
/// }
///
///# fn no_run() -> Result<(), Error> {
/// // SAFETY: Invalid usage for example purposes.
/// let iomem = unsafe { IoMem::<{ core::mem::size_of::<u32>() }>::new(0xBAAAAAAD)? };
/// iomem.write32(0x42, 0x0);
/// assert!(iomem.try_write32(0x42, 0x0).is_ok());
/// assert!(iomem.try_write32(0x42, 0x4).is_err());
/// # Ok(())
/// # }
/// ```
#[repr(transparent)]
pub struct Mmio<T: ?Sized>(MmioRaw<T>);

/// Checks whether an access of type `U` at the given `offset`
/// is valid within this region.
#[inline]
const fn offset_valid<U>(offset: usize, size: usize) -> bool {
    let type_size = core::mem::size_of::<U>();
    if let Some(end) = offset.checked_add(type_size) {
        end <= size && offset % type_size == 0
    } else {
        false
    }
}

/// Trait indicating that an I/O backend supports operations of a certain type and providing an
/// implementation for these operations.
///
/// Different I/O backends can implement this trait to expose only the operations they support.
///
/// For example, a PCI configuration space may implement `IoCapable<u8>`, `IoCapable<u16>`,
/// and `IoCapable<u32>`, but not `IoCapable<u64>`, while an MMIO region on a 64-bit
/// system might implement all four.
pub trait IoCapable<T> {
    /// Performs an I/O read of type `T` at `address` and returns the result.
    ///
    /// # Safety
    ///
    /// - The range `[address..address + size_of::<T>()]` must be within the bounds of `Self`.
    /// - `address` must be aligned.
    unsafe fn io_read(&self, address: *mut T) -> T;

    /// Performs an I/O write of `value` at `address`.
    ///
    /// # Safety
    ///
    /// - The range `[address..address + size_of::<T>()]` must be within the bounds of `Self`.
    /// - `address` must be aligned.
    unsafe fn io_write(&self, value: T, address: *mut T);
}

/// Describes a given I/O location: its offset, width, and type to convert the raw value from and
/// into.
///
/// This trait is the key abstraction allowing [`Io::read`], [`Io::write`], and [`Io::update`] (and
/// their fallible [`try_read`](Io::try_read), [`try_write`](Io::try_write) and
/// [`try_update`](Io::try_update) counterparts) to work uniformly with both raw [`usize`] offsets
/// (for primitive types like [`u32`]) and typed ones (like those generated by the [`register!`]
/// macro).
///
/// An `IoLoc<Base, T>` carries the following pieces of information:
///
/// - The valid `Base` to operate on. For most registers, this should be [`Region`].
/// - The offset to access (returned by [`IoLoc::offset`]),
/// - The width of the access (determined by [`IoLoc::IoType`]),
/// - The type `T` in which the raw data is returned or provided.
///
/// `T` and `IoLoc::IoType` may differ: for instance, a typed register has `T` = the register type
/// with its bitfields, and `IoType` = its backing primitive (e.g. `u32`).
pub trait IoLoc<Base: ?Sized, T> {
    /// Size ([`u8`], [`u16`], etc) of the I/O performed on the returned [`offset`](IoLoc::offset).
    type IoType: Into<T> + From<T>;

    /// Consumes `self` and returns the offset of this location.
    fn offset(self) -> usize;
}

/// Implements [`IoLoc<Region<SIZE>, $ty>`] for [`usize`], allowing [`usize`] to be used as a
/// parameter of [`Io::read`] and [`Io::write`].
macro_rules! impl_usize_ioloc {
    ($($ty:ty),*) => {
        $(
            impl<const SIZE: usize> IoLoc<Region<SIZE>, $ty> for usize {
                type IoType = $ty;

                #[inline(always)]
                fn offset(self) -> usize {
                    self
                }
            }
        )*
    }
}

// Provide the ability to read any primitive type from a [`usize`].
impl_usize_ioloc!(u8, u16, u32, u64);

/// Types implementing this trait (e.g. MMIO BARs or PCI config regions)
/// can perform I/O operations on regions of memory.
///
/// This is an abstract representation to be implemented by arbitrary I/O
/// backends (e.g. MMIO, PCI config space, etc.).
///
/// The [`Io`] trait provides:
/// - Base address and size information
/// - Helper methods for offset validation and address calculation
/// - Fallible (runtime checked) accessors for different data widths
///
/// Which I/O methods are available depends on which [`IoCapable<T>`] traits
/// are implemented for the type.
///
/// # Examples
///
/// For MMIO regions, all widths (u8, u16, u32, and u64 on 64-bit systems) are typically
/// supported. For PCI configuration space, u8, u16, and u32 are supported but u64 is not.
pub trait Io {
    /// Type of this I/O region. For untyped I/O regions, [`Region`] type can be used.
    type Type: ?Sized + KnownSize;

    /// Returns the base pointer of this mapping.
    ///
    /// This is a pointer to capture metadata. The specific meaning of the pointer depends on
    /// I/O backend and is not necessarily valid.
    fn as_ptr(&self) -> *mut Self::Type;

    /// Returns the absolute I/O address for a given `offset`,
    /// performing compile-time bound checks.
    // Always inline to optimize out error path of `build_assert`.
    #[inline(always)]
    fn io_addr_assert<U>(&self, offset: usize) -> *mut U {
        build_assert!(offset_valid::<U>(offset, Self::Type::MIN_SIZE));

        self.as_ptr().wrapping_byte_add(offset).cast()
    }

    /// Returns the absolute I/O address for a given `offset`,
    /// performing runtime bound checks.
    #[inline]
    fn io_addr<U>(&self, offset: usize) -> Result<*mut U> {
        let ptr = self.as_ptr();
        if !offset_valid::<U>(offset, Self::Type::size(ptr)) {
            return Err(EINVAL);
        }

        Ok(ptr.wrapping_byte_add(offset).cast())
    }

    /// Fallible 8-bit read with runtime bounds check.
    #[inline(always)]
    fn try_read8(&self, offset: usize) -> Result<u8>
    where
        usize: IoLoc<Self::Type, u8, IoType = u8>,
        Self: IoCapable<u8>,
    {
        self.try_read(offset)
    }

    /// Fallible 16-bit read with runtime bounds check.
    #[inline(always)]
    fn try_read16(&self, offset: usize) -> Result<u16>
    where
        usize: IoLoc<Self::Type, u16, IoType = u16>,
        Self: IoCapable<u16>,
    {
        self.try_read(offset)
    }

    /// Fallible 32-bit read with runtime bounds check.
    #[inline(always)]
    fn try_read32(&self, offset: usize) -> Result<u32>
    where
        usize: IoLoc<Self::Type, u32, IoType = u32>,
        Self: IoCapable<u32>,
    {
        self.try_read(offset)
    }

    /// Fallible 64-bit read with runtime bounds check.
    #[inline(always)]
    fn try_read64(&self, offset: usize) -> Result<u64>
    where
        usize: IoLoc<Self::Type, u64, IoType = u64>,
        Self: IoCapable<u64>,
    {
        self.try_read(offset)
    }

    /// Fallible 8-bit write with runtime bounds check.
    #[inline(always)]
    fn try_write8(&self, value: u8, offset: usize) -> Result
    where
        usize: IoLoc<Self::Type, u8, IoType = u8>,
        Self: IoCapable<u8>,
    {
        self.try_write(offset, value)
    }

    /// Fallible 16-bit write with runtime bounds check.
    #[inline(always)]
    fn try_write16(&self, value: u16, offset: usize) -> Result
    where
        usize: IoLoc<Self::Type, u16, IoType = u16>,
        Self: IoCapable<u16>,
    {
        self.try_write(offset, value)
    }

    /// Fallible 32-bit write with runtime bounds check.
    #[inline(always)]
    fn try_write32(&self, value: u32, offset: usize) -> Result
    where
        usize: IoLoc<Self::Type, u32, IoType = u32>,
        Self: IoCapable<u32>,
    {
        self.try_write(offset, value)
    }

    /// Fallible 64-bit write with runtime bounds check.
    #[inline(always)]
    fn try_write64(&self, value: u64, offset: usize) -> Result
    where
        usize: IoLoc<Self::Type, u64, IoType = u64>,
        Self: IoCapable<u64>,
    {
        self.try_write(offset, value)
    }

    /// Infallible 8-bit read with compile-time bounds check.
    #[inline(always)]
    fn read8(&self, offset: usize) -> u8
    where
        usize: IoLoc<Self::Type, u8, IoType = u8>,
        Self: IoCapable<u8>,
    {
        self.read(offset)
    }

    /// Infallible 16-bit read with compile-time bounds check.
    #[inline(always)]
    fn read16(&self, offset: usize) -> u16
    where
        usize: IoLoc<Self::Type, u16, IoType = u16>,
        Self: IoCapable<u16>,
    {
        self.read(offset)
    }

    /// Infallible 32-bit read with compile-time bounds check.
    #[inline(always)]
    fn read32(&self, offset: usize) -> u32
    where
        usize: IoLoc<Self::Type, u32, IoType = u32>,
        Self: IoCapable<u32>,
    {
        self.read(offset)
    }

    /// Infallible 64-bit read with compile-time bounds check.
    #[inline(always)]
    fn read64(&self, offset: usize) -> u64
    where
        usize: IoLoc<Self::Type, u64, IoType = u64>,
        Self: IoCapable<u64>,
    {
        self.read(offset)
    }

    /// Infallible 8-bit write with compile-time bounds check.
    #[inline(always)]
    fn write8(&self, value: u8, offset: usize)
    where
        usize: IoLoc<Self::Type, u8, IoType = u8>,
        Self: IoCapable<u8>,
    {
        self.write(offset, value)
    }

    /// Infallible 16-bit write with compile-time bounds check.
    #[inline(always)]
    fn write16(&self, value: u16, offset: usize)
    where
        usize: IoLoc<Self::Type, u16, IoType = u16>,
        Self: IoCapable<u16>,
    {
        self.write(offset, value)
    }

    /// Infallible 32-bit write with compile-time bounds check.
    #[inline(always)]
    fn write32(&self, value: u32, offset: usize)
    where
        usize: IoLoc<Self::Type, u32, IoType = u32>,
        Self: IoCapable<u32>,
    {
        self.write(offset, value)
    }

    /// Infallible 64-bit write with compile-time bounds check.
    #[inline(always)]
    fn write64(&self, value: u64, offset: usize)
    where
        usize: IoLoc<Self::Type, u64, IoType = u64>,
        Self: IoCapable<u64>,
    {
        self.write(offset, value)
    }

    /// Generic fallible read with runtime bounds check.
    ///
    /// # Examples
    ///
    /// Read a primitive type from an I/O address:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// fn do_reads(io: &Mmio<Region>) -> Result {
    ///     // 32-bit read from address `0x10`.
    ///     let v: u32 = io.try_read(0x10)?;
    ///
    ///     // 8-bit read from address `0xfff`.
    ///     let v: u8 = io.try_read(0xfff)?;
    ///
    ///     Ok(())
    /// }
    /// ```
    #[inline(always)]
    fn try_read<T, L>(&self, location: L) -> Result<T>
    where
        L: IoLoc<Self::Type, T>,
        Self: IoCapable<L::IoType>,
    {
        let address = self.io_addr::<L::IoType>(location.offset())?;

        // SAFETY: `address` has been validated by `io_addr`.
        Ok(unsafe { self.io_read(address) }.into())
    }

    /// Generic fallible write with runtime bounds check.
    ///
    /// # Examples
    ///
    /// Write a primitive type to an I/O address:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// fn do_writes(io: &Mmio<Region>) -> Result {
    ///     // 32-bit write of value `1` at address `0x10`.
    ///     io.try_write(0x10, 1u32)?;
    ///
    ///     // 8-bit write of value `0xff` at address `0xfff`.
    ///     io.try_write(0xfff, 0xffu8)?;
    ///
    ///     Ok(())
    /// }
    /// ```
    #[inline(always)]
    fn try_write<T, L>(&self, location: L, value: T) -> Result
    where
        L: IoLoc<Self::Type, T>,
        Self: IoCapable<L::IoType>,
    {
        let address = self.io_addr::<L::IoType>(location.offset())?;
        let io_value = value.into();

        // SAFETY: `address` has been validated by `io_addr`.
        unsafe { self.io_write(io_value, address) }

        Ok(())
    }

    /// Generic fallible write of a fully-located register value.
    ///
    /// # Examples
    ///
    /// Tuples carrying a location and a value can be used with this method:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     register,
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// register! {
    ///     VERSION(u32) @ 0x100 {
    ///         15:8 major;
    ///         7:0  minor;
    ///     }
    /// }
    ///
    /// impl VERSION {
    ///     fn new(major: u8, minor: u8) -> Self {
    ///         VERSION::zeroed().with_major(major).with_minor(minor)
    ///     }
    /// }
    ///
    /// fn do_write_reg(io: &Mmio<Region>) -> Result {
    ///
    ///     io.try_write_reg(VERSION::new(1, 0))
    /// }
    /// ```
    #[inline(always)]
    fn try_write_reg<T, L, V>(&self, value: V) -> Result
    where
        L: IoLoc<Self::Type, T>,
        V: LocatedRegister<Self::Type, Location = L, Value = T>,
        Self: IoCapable<L::IoType>,
    {
        let (location, value) = value.into_io_op();

        self.try_write(location, value)
    }

    /// Generic fallible update with runtime bounds check.
    ///
    /// Note: this does not perform any synchronization. The caller is responsible for ensuring
    /// exclusive access if required.
    ///
    /// # Examples
    ///
    /// Read the u32 value at address `0x10`, increment it, and store the updated value back:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// fn do_update(io: &Mmio<Region<0x1000>>) -> Result {
    ///     io.try_update(0x10, |v: u32| {
    ///         v + 1
    ///     })
    /// }
    /// ```
    #[inline(always)]
    fn try_update<T, L, F>(&self, location: L, f: F) -> Result
    where
        L: IoLoc<Self::Type, T>,
        Self: IoCapable<L::IoType>,
        F: FnOnce(T) -> T,
    {
        let address = self.io_addr::<L::IoType>(location.offset())?;

        // SAFETY: `address` has been validated by `io_addr`.
        let value: T = unsafe { self.io_read(address) }.into();
        let io_value = f(value).into();

        // SAFETY: `address` has been validated by `io_addr`.
        unsafe { self.io_write(io_value, address) }

        Ok(())
    }

    /// Generic infallible read with compile-time bounds check.
    ///
    /// # Examples
    ///
    /// Read a primitive type from an I/O address:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// fn do_reads(io: &Mmio<Region<0x1000>>) {
    ///     // 32-bit read from address `0x10`.
    ///     let v: u32 = io.read(0x10);
    ///
    ///     // 8-bit read from the top of the I/O space.
    ///     let v: u8 = io.read(0xfff);
    /// }
    /// ```
    #[inline(always)]
    fn read<T, L>(&self, location: L) -> T
    where
        L: IoLoc<Self::Type, T>,
        Self: IoCapable<L::IoType>,
    {
        let address = self.io_addr_assert::<L::IoType>(location.offset());

        // SAFETY: `address` has been validated by `io_addr_assert`.
        unsafe { self.io_read(address) }.into()
    }

    /// Generic infallible write with compile-time bounds check.
    ///
    /// # Examples
    ///
    /// Write a primitive type to an I/O address:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// fn do_writes(io: &Mmio<Region<0x1000>>) {
    ///     // 32-bit write of value `1` at address `0x10`.
    ///     io.write(0x10, 1u32);
    ///
    ///     // 8-bit write of value `0xff` at the top of the I/O space.
    ///     io.write(0xfff, 0xffu8);
    /// }
    /// ```
    #[inline(always)]
    fn write<T, L>(&self, location: L, value: T)
    where
        L: IoLoc<Self::Type, T>,
        Self: IoCapable<L::IoType>,
    {
        let address = self.io_addr_assert::<L::IoType>(location.offset());
        let io_value = value.into();

        // SAFETY: `address` has been validated by `io_addr_assert`.
        unsafe { self.io_write(io_value, address) }
    }

    /// Generic infallible write of a fully-located register value.
    ///
    /// # Examples
    ///
    /// Tuples carrying a location and a value can be used with this method:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     register,
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// register! {
    ///     VERSION(u32) @ 0x100 {
    ///         15:8 major;
    ///         7:0  minor;
    ///     }
    /// }
    ///
    /// impl VERSION {
    ///     fn new(major: u8, minor: u8) -> Self {
    ///         VERSION::zeroed().with_major(major).with_minor(minor)
    ///     }
    /// }
    ///
    /// fn do_write_reg(io: &Mmio<Region<0x1000>>) {
    ///     io.write_reg(VERSION::new(1, 0));
    /// }
    /// ```
    #[inline(always)]
    fn write_reg<T, L, V>(&self, value: V)
    where
        L: IoLoc<Self::Type, T>,
        V: LocatedRegister<Self::Type, Location = L, Value = T>,
        Self: IoCapable<L::IoType>,
    {
        let (location, value) = value.into_io_op();

        self.write(location, value)
    }

    /// Generic infallible update with compile-time bounds check.
    ///
    /// Note: this does not perform any synchronization. The caller is responsible for ensuring
    /// exclusive access if required.
    ///
    /// # Examples
    ///
    /// Read the u32 value at address `0x10`, increment it, and store the updated value back:
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    /// };
    ///
    /// fn do_update(io: &Mmio<Region<0x1000>>) {
    ///     io.update(0x10, |v: u32| {
    ///         v + 1
    ///     })
    /// }
    /// ```
    #[inline(always)]
    fn update<T, L, F>(&self, location: L, f: F)
    where
        L: IoLoc<Self::Type, T>,
        Self: IoCapable<L::IoType>,
        F: FnOnce(T) -> T,
    {
        let address = self.io_addr_assert::<L::IoType>(location.offset());

        // SAFETY: `address` has been validated by `io_addr_assert`.
        let value: T = unsafe { self.io_read(address) }.into();
        let io_value = f(value).into();

        // SAFETY: `address` has been validated by `io_addr_assert`.
        unsafe { self.io_write(io_value, address) }
    }
}

// For compatibility only.
#[doc(hidden)]
pub trait IoKnownSize: Io {}

impl<T: Io + ?Sized> IoKnownSize for T {}

/// Implements [`IoCapable`] on `$mmio` for `$ty` using `$read_fn` and `$write_fn`.
macro_rules! impl_mmio_io_capable {
    ($mmio:ident, $(#[$attr:meta])* $ty:ty, $read_fn:ident, $write_fn:ident) => {
        $(#[$attr])*
        impl<T: ?Sized> IoCapable<$ty> for $mmio<T> {
            unsafe fn io_read(&self, address: *mut $ty) -> $ty {
                // SAFETY: By the trait invariant `address` is a valid address for MMIO operations.
                unsafe { bindings::$read_fn(address as *const c_void) }
            }

            unsafe fn io_write(&self, value: $ty, address: *mut $ty) {
                // SAFETY: By the trait invariant `address` is a valid address for MMIO operations.
                unsafe { bindings::$write_fn(value, address.cast()) }
            }
        }
    };
}

// MMIO regions support 8, 16, and 32-bit accesses.
impl_mmio_io_capable!(Mmio, u8, readb, writeb);
impl_mmio_io_capable!(Mmio, u16, readw, writew);
impl_mmio_io_capable!(Mmio, u32, readl, writel);
// MMIO regions on 64-bit systems also support 64-bit accesses.
impl_mmio_io_capable!(
    Mmio,
    #[cfg(CONFIG_64BIT)]
    u64,
    readq,
    writeq
);

impl<T: ?Sized + KnownSize> Io for Mmio<T> {
    type Type = T;

    /// Returns the base address of this mapping.
    #[inline]
    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T: ?Sized + KnownSize> Mmio<T> {
    /// Converts an `MmioRaw` into an `Mmio` instance, providing the accessors to the MMIO mapping.
    ///
    /// # Safety
    ///
    /// Callers must ensure that `addr` is the start of a valid I/O mapped memory region of size
    /// `addr.size()`.
    pub unsafe fn from_raw(raw: &MmioRaw<T>) -> &Self {
        // SAFETY: `Mmio` is a transparent wrapper around `MmioRaw`.
        unsafe { &*core::ptr::from_ref(raw).cast() }
    }
}

/// [`Mmio`] wrapper using relaxed accessors.
///
/// This type provides an implementation of [`Io`] that uses relaxed I/O MMIO operands instead of
/// the regular ones.
///
/// See [`Mmio::relaxed`] for a usage example.
#[repr(transparent)]
pub struct RelaxedMmio<T: ?Sized>(Mmio<T>);

impl<T: ?Sized + KnownSize> Io for RelaxedMmio<T> {
    type Type = T;

    #[inline]
    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T: ?Sized> Mmio<T> {
    /// Returns a [`RelaxedMmio`] reference that performs relaxed I/O operations.
    ///
    /// Relaxed accessors do not provide ordering guarantees with respect to DMA or memory accesses
    /// and can be used when such ordering is not required.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kernel::io::{
    ///     Io,
    ///     Mmio,
    ///     Region,
    ///     RelaxedMmio,
    /// };
    ///
    /// fn do_io(io: &Mmio<Region<0x100>>) {
    ///     // The access is performed using `readl_relaxed` instead of `readl`.
    ///     let v = io.relaxed().read32(0x10);
    /// }
    ///
    /// ```
    pub fn relaxed(&self) -> &RelaxedMmio<T> {
        // SAFETY: `RelaxedMmio` is `#[repr(transparent)]` over `Mmio`, so `Mmio<T>` and
        // `RelaxedMmio<T>` have identical layout.
        unsafe { core::mem::transmute(self) }
    }
}

// MMIO regions support 8, 16, and 32-bit accesses.
impl_mmio_io_capable!(RelaxedMmio, u8, readb_relaxed, writeb_relaxed);
impl_mmio_io_capable!(RelaxedMmio, u16, readw_relaxed, writew_relaxed);
impl_mmio_io_capable!(RelaxedMmio, u32, readl_relaxed, writel_relaxed);
// MMIO regions on 64-bit systems also support 64-bit accesses.
impl_mmio_io_capable!(
    RelaxedMmio,
    #[cfg(CONFIG_64BIT)]
    u64,
    readq_relaxed,
    writeq_relaxed
);
