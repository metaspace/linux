// SPDX-License-Identifier: GPL-2.0

//! Intrusive high resolution timers.
//!
//! Allows running timer callbacks without doing allocations at the time of
//! starting the timer. For now, only one timer per type is allowed.
//!
//! # Vocabulary
//!
//! States:
//!
//! * Stopped
//! * Running
//!
//! Operations:
//!
//! * Start
//! * Cancel
//! * Stop
//! * Restart
//!
//! Events:
//!
//! * Expire
//!
//! ## State Diagram
//!
//! ```text
//!                  <-- Stop ----
//!                  <-- Cancel --
//!                  --- Start -->
//!        +---------+        +---------+
//!   O--->| Stopped |        | Running |---o
//!        +---------+        +---------+   |
//!                                  ^      |
//!                  <- Expire --    |      |
//!                                  o------o
//!                                   Restart
//! ```
//!
//! A timer is initialized in the **stopped** state. A stopped timer can be
//! **started** with an **expiry** time. After the timer is started, it is
//! **running**. When the timer **expires**, the timer handler is executed.
//! After the handler has executed, the timer may be **restarted** or
//! **stopped**. A running timer can be **canceled** before it's handler is
//! executed. A timer that is cancelled enters the **stopped** state.
//!

use crate::{init::PinInit, prelude::*, time::Ktime, types::Opaque};
use core::marker::PhantomData;

/// A timer backed by a C `struct hrtimer`.
///
/// # Invariants
///
/// * `self.timer` is initialized by `bindings::hrtimer_setup`.
#[pin_data]
#[repr(C)]
pub struct HrTimer<T> {
    #[pin]
    timer: Opaque<bindings::hrtimer>,
    _t: PhantomData<T>,
}

// SAFETY: Ownership of an `HrTimer` can be moved to other threads and
// used/dropped from there.
unsafe impl<T> Send for HrTimer<T> {}

// SAFETY: Timer operations are locked on C side, so it is safe to operate on a
// timer from multiple threads
unsafe impl<T> Sync for HrTimer<T> {}

impl<T> HrTimer<T> {
    /// Return an initializer for a new timer instance.
    pub fn new() -> impl PinInit<Self>
    where
        T: HrTimerCallback,
    {
        pin_init!(Self {
            // INVARIANTS: We initialize `timer` with `hrtimer_setup` below.
            timer <- Opaque::ffi_init(move |place: *mut bindings::hrtimer| {
                // SAFETY: By design of `pin_init!`, `place` is a pointer to a
                // live allocation. hrtimer_setup will initialize `place` and
                // does not require `place` to be initialized prior to the call.
                unsafe {
                    bindings::hrtimer_setup(
                        place,
                        Some(T::CallbackTarget::run),
                        bindings::CLOCK_MONOTONIC as i32,
                        bindings::hrtimer_mode_HRTIMER_MODE_REL,
                    );
                }
            }),
            _t: PhantomData,
        })
    }

    /// Get a pointer to the contained `bindings::hrtimer`.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live allocation of at least the size of `Self`.
    unsafe fn raw_get(ptr: *const Self) -> *mut bindings::hrtimer {
        // SAFETY: The field projection to `timer` does not go out of bounds,
        // because the caller of this function promises that `ptr` points to an
        // allocation of at least the size of `Self`.
        unsafe { Opaque::raw_get(core::ptr::addr_of!((*ptr).timer)) }
    }

    /// Cancel an initialized and potentially running timer.
    ///
    /// If the timer handler is running, this will block until the handler is
    /// finished.
    ///
    /// Users of the `HrTimer` API would not usually call this method directly.
    /// Instead they would use the safe `cancel` method on the [`HrTimerHandle`]
    /// returned when the timer was started.
    ///
    /// # Safety
    ///
    /// `self_ptr` must point to a valid `Self`.
    pub(crate) unsafe fn raw_cancel(self_ptr: *const Self) -> bool {
        // SAFETY: timer_ptr points to an allocation of at least `HrTimer` size.
        let c_timer_ptr = unsafe { HrTimer::raw_get(self_ptr) };

        // If the handler is running, this will wait for the handler to finish
        // before returning.
        // SAFETY: `c_timer_ptr` is initialized and valid. Synchronization is
        // handled on C side.
        unsafe { bindings::hrtimer_cancel(c_timer_ptr) != 0 }
    }
}

/// Implemented by pointer types that point to structs that embed a [`HrTimer`].
///
/// Target (pointee) must be [`Sync`] because timer callbacks happen in another
/// thread of execution (hard or soft interrupt context).
///
/// Starting a timer returns a [`HrTimerHandle`] that can be used to manipulate
/// the timer. Note that it is OK to call the start function repeatedly, and
/// that more than one [`HrTimerHandle`] associated with a [`HrTimerPointer`] may
/// exist. A timer can be manipulated through any of the handles, and a handle
/// may represent a cancelled timer.
pub trait HrTimerPointer: Sync + Sized {
    /// A handle representing a started or restarted timer.
    ///
    /// If the timer is running or if the timer callback is executing when the
    /// handle is dropped, the drop method of [`HrTimerHandle`] should not return
    /// until the timer is stopped and the callback has completed.
    ///
    /// Note: When implementing this trait, consider that it is not unsafe to
    /// leak the handle.
    type TimerHandle: HrTimerHandle;

    /// Start the timer with expiry after `expires` time units. If the timer was
    /// already running, it is restarted with the new expiry time.
    fn start(self, expires: Ktime) -> Self::TimerHandle;
}

/// Unsafe version of [`HrTimerPointer`] for situations where leaking the
/// [`HrTimerHandle`] returned by `start` would be unsound. This is the case for
/// stack allocated timers.
///
/// Typical implementers are pinned references such as [`Pin<&T>`].
///
/// # Safety
///
/// Implementers of this trait must ensure that instances of types implementing
/// [`UnsafeHrTimerPointer`] outlives any associated [`HrTimerPointer::TimerHandle`]
/// instances.
pub unsafe trait UnsafeHrTimerPointer: Sync + Sized {
    /// A handle representing a running timer.
    ///
    /// # Safety
    ///
    /// If the timer is running, or if the timer callback is executing when the
    /// handle is dropped, the drop method of [`Self::TimerHandle`] must not return
    /// until the timer is stopped and the callback has completed.
    type TimerHandle: HrTimerHandle;

    /// Start the timer after `expires` time units. If the timer was already
    /// running, it is restarted at the new expiry time.
    ///
    /// # Safety
    ///
    /// Caller promises keep the timer structure alive until the timer is dead.
    /// Caller can ensure this by not leaking the returned [`Self::TimerHandle`].
    unsafe fn start(self, expires: Ktime) -> Self::TimerHandle;
}

/// A trait for stack allocated timers.
///
/// # Safety
///
/// Implementers must ensure that `start_scoped` does not return until the
/// timer is dead and the timer handler is not running.
pub unsafe trait ScopedHrTimerPointer {
    /// Start the timer to run after `expires` time units and immediately
    /// after call `f`. When `f` returns, the timer is cancelled.
    fn start_scoped<T, F>(self, expires: Ktime, f: F) -> T
    where
        F: FnOnce() -> T;
}

// SAFETY: By the safety requirement of [`UnsafeHrTimerPointer`], dropping the
// handle returned by [`UnsafeHrTimerPointer::start`] ensures that the timer is
// killed.
unsafe impl<T> ScopedHrTimerPointer for T
where
    T: UnsafeHrTimerPointer,
{
    fn start_scoped<U, F>(self, expires: Ktime, f: F) -> U
    where
        F: FnOnce() -> U,
    {
        // SAFETY: We drop the timer handle below before returning.
        let handle = unsafe { UnsafeHrTimerPointer::start(self, expires) };
        let t = f();
        drop(handle);
        t
    }
}

/// Implemented by [`HrTimerPointer`] implementers to give the C timer callback a
/// function to call.
// This is split from `HrTimerPointer` to make it easier to specify trait bounds.
pub trait RawHrTimerCallback {
    /// Callback to be called from C when timer fires.
    ///
    /// # Safety
    ///
    /// Only to be called by C code in `hrtimer` subsystem. `ptr` must point to
    /// the `bindings::hrtimer` structure that was used to start the timer.
    unsafe extern "C" fn run(ptr: *mut bindings::hrtimer) -> bindings::hrtimer_restart;
}

/// Implemented by structs that can be the target of a timer callback.
pub trait HrTimerCallback {
    /// The type whose [`RawHrTimerCallback::run`] method will be invoked when
    /// the timer expires.
    type CallbackTarget<'a>: RawHrTimerCallback;

    /// This type is passed to the timer callback function. It may be a borrow
    /// of [`Self::CallbackTarget`], or it may be `Self::CallbackTarget` if the
    /// implementation can guarantee exclusive access to the target during timer
    /// handler execution.
    type CallbackTargetParameter<'a>;

    /// Called by the timer logic when the timer fires.
    fn run(this: Self::CallbackTargetParameter<'_>) -> HrTimerRestart
    where
        Self: Sized;
}

/// A handle representing a potentially running timer.
///
/// More than one handle representing the same timer might exist.
///
/// # Safety
///
/// When dropped, the timer represented by this handle must be cancelled, if it
/// is running. If the timer handler is running when the handle is dropped, the
/// drop method must wait for the handler to finish before returning.
pub unsafe trait HrTimerHandle {
    /// Cancel the timer, if it is running. If the timer handler is running, block
    /// till the handler has finished.
    fn cancel(&mut self) -> bool;
}

/// Implemented by structs that contain timer nodes.
///
/// Clients of the timer API would usually safely implement this trait by using
/// the [`crate::impl_has_hr_timer`] macro.
///
/// # Safety
///
/// Implementers of this trait must ensure that the implementer has a [`HrTimer`]
/// field at the offset specified by `OFFSET` and that all trait methods are
/// implemented according to their documentation.
///
/// [`impl_has_timer`]: crate::impl_has_timer
pub unsafe trait HasHrTimer<T> {
    /// Offset of the [`HrTimer`] field within `Self`
    const OFFSET: usize;

    /// Return a pointer to the [`HrTimer`] within `Self`.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid struct of type `Self`.
    unsafe fn raw_get_timer(ptr: *const Self) -> *const HrTimer<T> {
        // SAFETY: By the safety requirement of this trait, the trait
        // implementor will have a `HrTimer` field at the specified offset.
        unsafe { ptr.cast::<u8>().add(Self::OFFSET).cast::<HrTimer<T>>() }
    }

    /// Return a pointer to the struct that is embedding the [`HrTimer`] pointed
    /// to by `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a [`HrTimer<T>`] field in a struct of type `Self`.
    unsafe fn timer_container_of(ptr: *mut HrTimer<T>) -> *mut Self
    where
        Self: Sized,
    {
        // SAFETY: By the safety requirement of this function and the `HasHrTimer`
        // trait, the following expression will yield a pointer to the `Self`
        // containing the timer addressed by `ptr`.
        unsafe { ptr.cast::<u8>().sub(Self::OFFSET).cast::<Self>() }
    }

    /// Get pointer to embedded `bindings::hrtimer` struct.
    ///
    /// # Safety
    ///
    /// `self_ptr` must point to a valid `Self`.
    unsafe fn c_timer_ptr(self_ptr: *const Self) -> *const bindings::hrtimer {
        // SAFETY: `self_ptr` is a valid pointer to a `Self`.
        let timer_ptr = unsafe { Self::raw_get_timer(self_ptr) };

        // SAFETY: timer_ptr points to an allocation of at least `HrTimer` size.
        unsafe { HrTimer::raw_get(timer_ptr) }
    }

    /// Start the timer contained in the `Self` pointed to by `self_ptr`. If
    /// it is already running it is removed and inserted.
    ///
    /// # Safety
    ///
    /// `self_ptr` must point to a valid `Self`.
    unsafe fn start(self_ptr: *const Self, expires: Ktime) {
        // SAFETY: By function safety requirement, `self_ptr`is a valid `Self`.
        unsafe {
            bindings::hrtimer_start_range_ns(
                Self::c_timer_ptr(self_ptr).cast_mut(),
                expires.to_ns(),
                0,
                bindings::hrtimer_mode_HRTIMER_MODE_REL,
            );
        }
    }
}

/// Restart policy for timers.
pub enum HrTimerRestart {
    /// Timer should not be restarted.
    NoRestart,
    /// Timer should be restarted.
    Restart,
}

impl From<bindings::hrtimer_restart> for HrTimerRestart {
    fn from(value: u32) -> Self {
        match value {
            bindings::hrtimer_restart_HRTIMER_NORESTART => Self::NoRestart,
            _ => Self::Restart,
        }
    }
}

impl From<HrTimerRestart> for bindings::hrtimer_restart {
    fn from(value: HrTimerRestart) -> Self {
        match value {
            HrTimerRestart::NoRestart => bindings::hrtimer_restart_HRTIMER_NORESTART,
            HrTimerRestart::Restart => bindings::hrtimer_restart_HRTIMER_RESTART,
        }
    }
}

/// Use to implement the [`HasHrTimer<T>`] trait.
///
/// See [`module`] documentation for an example.
///
/// [`module`]: crate::time::hrtimer
#[macro_export]
macro_rules! impl_has_hr_timer {
    (
        impl$({$($generics:tt)*})?
            HasHrTimer<$timer_type:ty>
            for $self:ty
        { self.$field:ident }
        $($rest:tt)*
    ) => {
        // SAFETY: This implementation of `raw_get_timer` only compiles if the
        // field has the right type.
        unsafe impl$(<$($generics)*>)? $crate::time::hrtimer::HasHrTimer<$timer_type> for $self {
            const OFFSET: usize = ::core::mem::offset_of!(Self, $field) as usize;

            #[inline]
            unsafe fn raw_get_timer(ptr: *const Self) ->
                *const $crate::time::hrtimer::HrTimer<$timer_type>
            {
                // SAFETY: The caller promises that the pointer is not dangling.
                unsafe {
                    ::core::ptr::addr_of!((*ptr).$field)
                }
            }
        }
    }
}

mod arc;
mod pin;
mod pin_mut;
