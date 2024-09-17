// SPDX-License-Identifier: GPL-2.0

use super::HasHrTimer;
use super::HrTimer;
use super::HrTimerCallback;
use super::HrTimerHandle;
use super::RawHrTimerCallback;
use super::UnsafeHrTimerPointer;
use crate::time::Ktime;
use core::pin::Pin;

/// A handle for a `Pin<&mut HasHrTimer>`. When the handle exists, the timer might
/// be running.
pub struct PinMutHrTimerHandle<'a, T>
where
    T: HasHrTimer<T>,
{
    pub(crate) inner: Pin<&'a mut T>,
}

// SAFETY: We cancel the timer when the handle is dropped. The implementation of
// the `cancel` method will block if the timer handler is running.
unsafe impl<'a, T> HrTimerHandle for PinMutHrTimerHandle<'a, T>
where
    T: HasHrTimer<T>,
{
    fn cancel(&mut self) -> bool {
        // SAFETY: We are not moving out of `self` or handing out mutable
        // references to `self`.
        let container_ref = unsafe { self.inner.as_mut().get_unchecked_mut() };

        let timer = <T as HasHrTimer<T>>::timer(container_ref);
        HrTimer::<T>::cancel(timer)
    }
}

impl<'a, T> Drop for PinMutHrTimerHandle<'a, T>
where
    T: HasHrTimer<T>,
{
    fn drop(&mut self) {
        self.cancel();
    }
}

// SAFETY: We capture the lifetime of `Self` when we create a
// `PinMutHrTimerHandle`, so `Self` will outlive the handle.
unsafe impl<'a, T> UnsafeHrTimerPointer for Pin<&'a mut T>
where
    T: Send + Sync,
    T: HasHrTimer<T>,
    T: HrTimerCallback<Pointer<'a> = Self>,
    Pin<&'a mut T>: RawHrTimerCallback<CallbackTarget<'a> = Self>,
{
    type TimerHandle = PinMutHrTimerHandle<'a, T>;

    unsafe fn start(self, expires: Ktime) -> Self::TimerHandle {
        // SAFETY: We keep `self` alive by wrapping it in a handle below.
        unsafe { T::start_internal(&self, expires) };

        PinMutHrTimerHandle { inner: self }
    }
}

impl<'a, T> RawHrTimerCallback for Pin<&'a mut T>
where
    T: HasHrTimer<T>,
    T: HrTimerCallback<Pointer<'a> = Self>,
{
    type CallbackTarget<'b> = Self;

    unsafe extern "C" fn run(ptr: *mut bindings::hrtimer) -> bindings::hrtimer_restart {
        // `HrTimer` is `repr(C)`
        let timer_ptr = ptr as *mut HrTimer<T>;

        // SAFETY: By the safety requirement of this function, `timer_ptr`
        // points to a `HrTimer<T>` contained in an `T`.
        let receiver_ptr = unsafe { T::timer_container_of(timer_ptr) };

        // SAFETY: By the safety requirement of this function, `timer_ptr`
        // points to a `HrTimer<T>` contained in an `T`.
        let receiver_ref = unsafe { &mut *receiver_ptr };

        // SAFETY: `receiver_ref` only exists as pinned, so it is safe to pin it
        // here.
        let receiver_pin = unsafe { Pin::new_unchecked(receiver_ref) };

        T::run(receiver_pin).into_c()
    }
}
