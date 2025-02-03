// SPDX-License-Identifier: GPL-2.0

use super::HasHrTimer;
use super::HrTimer;
use super::HrTimerCallback;
use super::HrTimerHandle;
use super::RawHrTimerCallback;
use super::UnsafeHrTimerPointer;
use crate::time::Ktime;
use core::pin::Pin;

/// A handle for a `Pin<&HasHrTimer>`. When the handle exists, the timer might be
/// running.
pub struct PinHrTimerHandle<'a, T>
where
    T: HasHrTimer<T>,
{
    pub(crate) inner: Pin<&'a T>,
}

// SAFETY: We cancel the timer when the handle is dropped. The implementation of
// the `cancel` method will block if the timer handler is running.
unsafe impl<'a, T> HrTimerHandle for PinHrTimerHandle<'a, T>
where
    T: HasHrTimer<T>,
{
    fn cancel(&mut self) -> bool {
        let self_ptr: *const T = self.inner.get_ref();

        // SAFETY: As we got `self_ptr` from a reference above, it must point to
        // a valid `T`.
        let timer_ptr = unsafe { <T as HasHrTimer<T>>::raw_get_timer(self_ptr) };

        // SAFETY: As `timer_ptr` is derived from a reference, it must point to
        // a valid and initialized `HrTimer`.
        unsafe { HrTimer::<T>::raw_cancel(timer_ptr) }
    }
}

impl<'a, T> Drop for PinHrTimerHandle<'a, T>
where
    T: HasHrTimer<T>,
{
    fn drop(&mut self) {
        self.cancel();
    }
}

// SAFETY: We capture the lifetime of `Self` when we create a `PinHrTimerHandle`,
// so `Self` will outlive the handle.
unsafe impl<'a, T> UnsafeHrTimerPointer for Pin<&'a T>
where
    T: Send + Sync,
    T: HasHrTimer<T>,
    T: HrTimerCallback<CallbackTarget<'a> = Self>,
{
    type TimerHandle = PinHrTimerHandle<'a, T>;

    unsafe fn start(self, expires: Ktime) -> Self::TimerHandle {
        // Cast to pointer
        let self_ptr: *const T = <Self as core::ops::Deref>::deref(&self);

        // SAFETY: As we derive `self_ptr` from a reference above, it must point
        // to a valid `T`.
        unsafe { T::start(self_ptr, expires) };

        PinHrTimerHandle { inner: self }
    }
}

impl<'a, T> RawHrTimerCallback for Pin<&'a T>
where
    T: HasHrTimer<T>,
    T: HrTimerCallback<CallbackTarget<'a> = Self>,
    T: HrTimerCallback<CallbackTargetParameter<'a> = Self>,
{
    unsafe extern "C" fn run(ptr: *mut bindings::hrtimer) -> bindings::hrtimer_restart {
        // `HrTimer` is `repr(C)`
        let timer_ptr = ptr as *mut HrTimer<T>;

        // SAFETY: By the safety requirement of this function, `timer_ptr`
        // points to a `HrTimer<T>` contained in an `T`.
        let receiver_ptr = unsafe { T::timer_container_of(timer_ptr) };

        // SAFETY: By the safety requirement of this function, `timer_ptr`
        // points to a `HrTimer<T>` contained in an `T`.
        let receiver_ref = unsafe { &*receiver_ptr };

        // SAFETY: `receiver_ref` only exists as pinned, so it is safe to pin it
        // here.
        let receiver_pin = unsafe { Pin::new_unchecked(receiver_ref) };

        T::run(receiver_pin).into()
    }
}
