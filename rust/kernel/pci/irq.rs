// SPDX-License-Identifier: GPL-2.0

//! PCI interrupt infrastructure.

use super::Device;
use crate::{
    bindings,
    device,
    device::Bound,
    devres,
    error::to_result,
    irq::{
        self,
        IrqRequest, //
    },
    prelude::*,
    str::CStr,
    sync::aref::ARef, //
};

/// IRQ type flags for PCI interrupt allocation.
#[derive(Debug, Clone, Copy)]
pub enum IrqType {
    /// INTx interrupts.
    Intx,
    /// Message Signaled Interrupts (MSI).
    Msi,
    /// Extended Message Signaled Interrupts (MSI-X).
    MsiX,
}

impl IrqType {
    /// Convert to the corresponding kernel flags.
    const fn as_raw(self) -> u32 {
        match self {
            IrqType::Intx => bindings::PCI_IRQ_INTX,
            IrqType::Msi => bindings::PCI_IRQ_MSI,
            IrqType::MsiX => bindings::PCI_IRQ_MSIX,
        }
    }
}

/// Set of IRQ types that can be used for PCI interrupt allocation.
// TODO: This is a general flag thing, not just types.
#[derive(Debug, Clone, Copy, Default)]
pub struct IrqTypes(u32);

impl IrqTypes {
    /// Create a set containing all IRQ types (MSI-X, MSI, and INTx).
    pub const fn all() -> Self {
        Self(bindings::PCI_IRQ_ALL_TYPES)
    }

    /// Build a set of IRQ types.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Create a set with only MSI and MSI-X (no INTx interrupts).
    /// let msi_only = IrqTypes::default()
    ///     .with(IrqType::Msi)
    ///     .with(IrqType::MsiX);
    /// ```
    pub const fn with(self, irq_type: IrqType) -> Self {
        Self(self.0 | irq_type.as_raw())
    }

    /// Get the raw flags value.
    const fn as_raw(self) -> u32 {
        self.0
    }
}

/// Represents an allocated IRQ vector for a specific PCI device.
///
/// This type ties an IRQ vector to the device it was allocated for,
/// ensuring the vector is only used with the correct device.
#[derive(Clone, Copy)]
pub struct IrqVector<'a> {
    dev: &'a Device<Bound>,
    index: u32,
}

impl<'a> IrqVector<'a> {
    /// Creates a new [`IrqVector`] for the given device and index.
    ///
    /// # Safety
    ///
    /// - `index` must be a valid IRQ vector index for `dev`.
    /// - `dev` must point to a [`Device`] that has successfully allocated IRQ vectors.
    unsafe fn new(dev: &'a Device<Bound>, index: u32) -> Self {
        Self { dev, index }
    }

    /// Returns the raw vector index.
    pub fn index(&self) -> u32 {
        self.index
    }
}

impl<'a> TryInto<IrqRequest<'a>> for IrqVector<'a> {
    type Error = Error;

    fn try_into(self) -> Result<IrqRequest<'a>> {
        // SAFETY: `self.as_raw` returns a valid pointer to a `struct pci_dev`.
        let irq = unsafe { bindings::pci_irq_vector(self.dev.as_raw(), self.index()) };
        if irq < 0 {
            return Err(crate::error::Error::from_errno(irq));
        }
        // SAFETY: `irq` is guaranteed to be a valid IRQ number for `&self`.
        Ok(unsafe { IrqRequest::new(self.dev.as_ref(), irq as u32) })
    }
}

/// The set of IRQ vectors allocated for a PCI device.
///
/// Returned by [`Device::alloc_irq_vectors`]. Provides indexed access and iteration over the
/// allocated [`IrqVector`]s.
///
/// # Invariants
///
/// `count` is the number of IRQ vectors successfully allocated for `dev`.
#[derive(Clone, Copy)]
pub struct IrqVectors<'a> {
    dev: &'a Device<Bound>,
    count: u32,
}

impl<'a> IrqVectors<'a> {
    /// Creates a new [`IrqVectors`] for the given device and number of vectors.
    ///
    /// # Safety
    ///
    /// - `dev` must point to a [`Device`] that has successfully allocated IRQ vectors.
    /// - `0..count` must be valid irq vector indexes for `dev`.
    unsafe fn new(dev: &'a Device<Bound>, count: u32) -> Self {
        Self { dev, count }
    }

    /// Returns the number of allocated IRQ vectors.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Returns the nth IRQ vector, or `None` if out of bounds.
    pub fn nth(&self, index: u32) -> Option<IrqVector<'a>> {
        if index >= self.count {
            return None;
        }

        // SAFETY: `index` is within bounds of the allocated vectors.
        Some(unsafe { IrqVector::new(self.dev, index) })
    }

    /// Returns an iterator over all allocated IRQ vectors.
    pub fn iter(&self) -> IrqVectorIter<'a> {
        IrqVectorIter {
            dev: self.dev,
            index: 0,
            count: self.count,
        }
    }
}

impl<'a> IntoIterator for IrqVectors<'a> {
    type Item = IrqVector<'a>;
    type IntoIter = IrqVectorIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// An iterator over the [`IrqVector`]s of an [`IrqVectors`] allocation.
pub struct IrqVectorIter<'a> {
    dev: &'a Device<Bound>,
    index: u32,
    count: u32,
}

impl<'a> Iterator for IrqVectorIter<'a> {
    type Item = IrqVector<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.count {
            return None;
        }

        // SAFETY: `self.index` is within bounds of the allocated vectors.
        let vector = unsafe { IrqVector::new(self.dev, self.index) };
        self.index += 1;
        Some(vector)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.count - self.index) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for IrqVectorIter<'_> {}

/// Represents an IRQ vector allocation for a PCI device.
///
/// This type ensures that IRQ vectors are properly allocated and freed by
/// tying the allocation to the lifetime of this registration object.
///
/// # Invariants
///
/// The [`Device`] has successfully allocated IRQ vectors.
pub struct IrqVectorRegistration {
    dev: ARef<Device>,
}

impl IrqVectorRegistration {
    /// Allocate and register IRQ vectors for the given PCI device.
    ///
    /// Allocates IRQ vectors and registers them with devres for automatic cleanup.
    /// Returns a range of valid IRQ vectors.
    fn register_devres<'a>(
        dev: &'a Device<Bound>,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
    ) -> Result<IrqVectors<'a>> {
        let (irq_vecs, range) = Self::register(dev, min_vecs, max_vecs, irq_types)?;
        devres::register(dev.as_ref(), irq_vecs, GFP_KERNEL)?;
        Ok(range)
    }

    /// Allocate and register IRQ vectors for the given PCI device.
    ///
    /// Allocates IRQ vectors and registers them with devres for automatic cleanup.
    /// Returns a range of valid IRQ vectors.
    fn register<'a>(
        dev: &'a Device<Bound>,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
    ) -> Result<(Self, IrqVectors<'a>)> {
        Self::register_affinity(dev, min_vecs, max_vecs, irq_types, 0, 0)
    }

    fn register_affinity<'a>(
        dev: &'a Device<Bound>,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
        pre_vectors: u32,
        post_vectors: u32,
    ) -> Result<(Self, IrqVectors<'a>)> {
        let mut affinity = bindings::irq_affinity {
            pre_vectors,
            post_vectors,
            ..bindings::irq_affinity::default()
        };

        let mut flags = irq_types.as_raw();
        let mut affinity_ptr = core::ptr::null_mut();
        if pre_vectors != 0 || post_vectors != 0 {
            flags |= bindings::PCI_IRQ_AFFINITY;
            affinity_ptr = &mut affinity;
        }

        flags |= bindings::IRQF_SHARED;

        // SAFETY:
        // - `dev.as_raw()` is guaranteed to be a valid pointer to a `struct pci_dev`
        //   by the type invariant of `Device`.
        // - `pci_alloc_irq_vectors` internally validates all other parameters
        //   and returns error codes.
        let ret = unsafe {
            bindings::pci_alloc_irq_vectors_affinity(
                dev.as_raw(),
                min_vecs,
                max_vecs,
                flags,
                affinity_ptr,
            )
        };

        to_result(ret)?;
        let count = ret as u32;

        // SAFETY:
        // - `pci_alloc_irq_vectors` returns the number of allocated vectors on success.
        // - Vectors are 0-based, so valid indices are [0, count-1].
        // - `pci_alloc_irq_vectors` guarantees `count >= min_vecs > 0`, so both `0..count` are
        //   valid IRQ vector indices for `dev`.
        let range = unsafe { IrqVectors::new(dev, count) };

        // INVARIANT: The IRQ vector allocation for `dev` above was successful.
        let irq_vecs = Self { dev: dev.into() };

        Ok((irq_vecs, range))
    }
}

impl Drop for IrqVectorRegistration {
    fn drop(&mut self) {
        pr_warn!("Dropping IrqVectorRegistration\n");
        // SAFETY:
        // - By the type invariant, `self.dev.as_raw()` is a valid pointer to a `struct pci_dev`.
        // - `self.dev` has successfully allocated IRQ vectors.
        unsafe { bindings::pci_free_irq_vectors(self.dev.as_raw()) };
    }
}

impl Device<device::Bound> {
    /// Returns a [`kernel::irq::Registration`] for the given IRQ vector.
    pub fn request_irq<'a, T: crate::irq::Handler + 'static>(
        &'a self,
        vector: IrqVector<'a>,
        flags: irq::Flags,
        // TODO: Bad decision
        name: &'static CStr,
        handler: impl PinInit<T, Error> + 'a,
    ) -> impl PinInit<irq::Registration<T>, Error> + 'a {
        pin_init::pin_init_scope(move || {
            let request = vector.try_into()?;

            // TODO: Keep IrqVectorRegistration alive with refcounting.
            Ok(irq::Registration::<T>::new(request, flags, name, handler))
        })
    }

    /// Returns a [`kernel::irq::ThreadedRegistration`] for the given IRQ vector.
    pub fn request_threaded_irq<'a, T: crate::irq::ThreadedHandler + 'static>(
        &'a self,
        vector: IrqVector<'a>,
        flags: irq::Flags,
        name: &'static CStr,
        handler: impl PinInit<T, Error> + 'a,
    ) -> impl PinInit<irq::ThreadedRegistration<T>, Error> + 'a {
        pin_init::pin_init_scope(move || {
            let request = vector.try_into()?;

            Ok(irq::ThreadedRegistration::<T>::new(
                request, flags, name, handler,
            ))
        })
    }

    /// Allocate IRQ vectors for this PCI device with automatic cleanup.
    ///
    /// Allocates between `min_vecs` and `max_vecs` interrupt vectors for the device.
    /// The allocation will use MSI-X, MSI, or INTx interrupts based on the `irq_types`
    /// parameter and hardware capabilities. When multiple types are specified, the kernel
    /// will try them in order of preference: MSI-X first, then MSI, then INTx interrupts.
    ///
    /// The allocated vectors are automatically freed when the device is unbound, using the
    /// devres (device resource management) system.
    ///
    /// # Arguments
    ///
    /// * `min_vecs` - Minimum number of vectors required.
    /// * `max_vecs` - Maximum number of vectors to allocate.
    /// * `irq_types` - Types of interrupts that can be used.
    ///
    /// # Returns
    ///
    /// Returns a range of IRQ vectors that were successfully allocated, or an error if the
    /// allocation fails or cannot meet the minimum requirement.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{ device::Bound, pci};
    /// # fn no_run(dev: &pci::Device<Bound>) -> Result {
    /// // Allocate using any available interrupt type in the order mentioned above.
    /// let vectors = dev.alloc_irq_vectors(1, 32, pci::IrqTypes::all())?;
    ///
    /// // Allocate MSI or MSI-X only (no INTx interrupts).
    /// let msi_only = pci::IrqTypes::default()
    ///     .with(pci::IrqType::Msi)
    ///     .with(pci::IrqType::MsiX);
    /// let vectors = dev.alloc_irq_vectors(4, 16, msi_only)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn alloc_irq_vectors_devres(
        &self,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
    ) -> Result<IrqVectors<'_>> {
        IrqVectorRegistration::register_devres(self, min_vecs, max_vecs, irq_types)
    }

    /// Allocate IRQ vectors for this PCI device with automatic cleanup.
    ///
    /// See [`Self::alloc_irq_vectoers_devres`] for details.
    ///
    /// Cleanup happens when the returned [`IrqVectorRegistration`] is dropped.
    pub fn alloc_irq_vectors(
        &self,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
    ) -> Result<(IrqVectorRegistration, IrqVectors<'_>)> {
        IrqVectorRegistration::register(self, min_vecs, max_vecs, irq_types)
    }

    /// Allocate IRQ vectors for this PCI device with automatic cleanup.
    ///
    /// See [`Self::alloc_irq_vectoers_devres`] for details.
    ///
    /// Cleanup happens when the returned [`IrqVectorRegistration`] is dropped.
    pub fn alloc_irq_vectors_affinity(
        &self,
        min_vecs: u32,
        max_vecs: u32,
        irq_types: IrqTypes,
        pre_vectors: u32,
        post_vectors: u32,
    ) -> Result<(IrqVectorRegistration, IrqVectors<'_>)> {
        IrqVectorRegistration::register_affinity(
            self,
            min_vecs,
            max_vecs,
            irq_types,
            pre_vectors,
            post_vectors,
        )
    }
}
