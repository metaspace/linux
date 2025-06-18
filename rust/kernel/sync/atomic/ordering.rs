// SPDX-License-Identifier: GPL-2.0

//! Memory orderings.
//!
//! The semantics of these orderings follows the [`LKMM`] definitions and rules.
//!
//! - [`Acquire`] and [`Release`] are similar to their counterpart in Rust memory model.
//! - [`Full`] means "fully-ordered", that is:
//!   - It provides ordering between all the preceding memory accesses and the annotated operation.
//!   - It provides ordering between the annotated operation and all the following memory accesses.
//!   - It provides ordering between all the preceding memory accesses and all the fllowing memory
//!     accesses.
//!   - All the orderings are the same strong as a full memory barrier (i.e. `smp_mb()`).
//! - [`Relaxed`] is similar to the counterpart in Rust memory model, except that dependency
//!   orderings are also honored in [`LKMM`]. Dependency orderings are described in "DEPENDENCY
//!   RELATIONS" in [`LKMM`]'s [`explanation`].
//!
//! [`LKMM`]: srctree/tools/memory-model/
//! [`explanation`]: srctree/tools/memory-model/Documentation/explanation.txt

/// The annotation type for relaxed memory ordering.
pub struct Relaxed;

/// The annotation type for acquire memory ordering.
pub struct Acquire;

/// The annotation type for release memory ordering.
pub struct Release;

/// The annotation type for fully-order memory ordering.
pub struct Full;

/// Describes the exact memory ordering.
pub enum OrderingType {
    /// Relaxed ordering.
    Relaxed,
    /// Acquire ordering.
    Acquire,
    /// Release ordering.
    Release,
    /// Fully-ordered.
    Full,
}

mod internal {
    /// Unit types for ordering annotation.
    ///
    /// Sealed trait, can be only implemented inside atomic mod.
    pub trait OrderingUnit {
        /// Describes the exact memory ordering.
        const TYPE: super::OrderingType;
    }
}

impl internal::OrderingUnit for Relaxed {
    const TYPE: OrderingType = OrderingType::Relaxed;
}

impl internal::OrderingUnit for Acquire {
    const TYPE: OrderingType = OrderingType::Acquire;
}

impl internal::OrderingUnit for Release {
    const TYPE: OrderingType = OrderingType::Release;
}

impl internal::OrderingUnit for Full {
    const TYPE: OrderingType = OrderingType::Full;
}

/// The trait bound for annotating operations that should support all orderings.
pub trait All: internal::OrderingUnit {}

impl All for Relaxed {}
impl All for Acquire {}
impl All for Release {}
impl All for Full {}

/// The trait bound for operations that only support acquire or relaxed ordering.
pub trait AcquireOrRelaxed: All {
    /// Describes whether an ordering is relaxed or not.
    const IS_RELAXED: bool = false;
}

impl AcquireOrRelaxed for Acquire {}

impl AcquireOrRelaxed for Relaxed {
    const IS_RELAXED: bool = true;
}

/// The trait bound for operations that only support release or relaxed ordering.
pub trait ReleaseOrRelaxed: All {
    /// Describes whether an ordering is relaxed or not.
    const IS_RELAXED: bool = false;
}

impl ReleaseOrRelaxed for Release {}

impl ReleaseOrRelaxed for Relaxed {
    const IS_RELAXED: bool = true;
}

/// The trait bound for operations that only support relaxed ordering.
pub trait RelaxedOnly: AcquireOrRelaxed + ReleaseOrRelaxed + All {}

impl RelaxedOnly for Relaxed {}
