// SPDX-License-Identifier: GPL-2.0

use super::HasHrTimer;
use super::HrTimer;
use super::HrTimerCallback;
use super::HrTimerHandle;
use super::HrTimerPointer;
use super::RawHrTimerCallback;
use crate::sync::Arc;
use crate::sync::ArcBorrow;
use crate::time::Ktime;

/// A handle for an `Arc<HasHrTimer<T>>` returned by a call to
/// [`HrTimerPointer::start`].
pub struct ArcHrTimerHandle<T>
where
    T: HasHrTimer<T>,
{
    pub(crate) inner: Arc<T>,
}

// SAFETY: We implement drop below, and we cancel the timer in the drop
// implementation.
unsafe impl<T> HrTimerHandle for ArcHrTimerHandle<T>
where
    T: HasHrTimer<T>,
{
    fn cancel(&mut self) -> bool {
        let timer = <T as HasHrTimer<T>>::timer(self.inner.as_ref());
        HrTimer::<T>::cancel(timer)
    }
}

impl<T> Drop for ArcHrTimerHandle<T>
where
    T: HasHrTimer<T>,
{
    fn drop(&mut self) {
        self.cancel();
    }
}

impl<T> HrTimerPointer for Arc<T>
where
    T: 'static,
    T: Send + Sync,
    T: HasHrTimer<T>,
    T: for<'a> HrTimerCallback<Pointer<'a> = Self>,
    Arc<T>: for<'a> RawHrTimerCallback<CallbackTarget<'a> = ArcBorrow<'a, T>>,
{
    type TimerHandle = ArcHrTimerHandle<T>;

    fn start(self, expires: Ktime) -> ArcHrTimerHandle<T> {
        // SAFETY: We keep `self` alive by wrapping it in a handle below.
        unsafe { T::start_internal(&self, expires) };
        ArcHrTimerHandle { inner: self }
    }
}

impl<T> RawHrTimerCallback for Arc<T>
where
    T: 'static,
    T: HasHrTimer<T>,
    T: for<'a> HrTimerCallback<Pointer<'a> = Self>,
{
    type CallbackTarget<'a> = ArcBorrow<'a, T>;

    unsafe extern "C" fn run(ptr: *mut bindings::hrtimer) -> bindings::hrtimer_restart {
        // `HrTimer` is `repr(C)`
        let timer_ptr = ptr.cast::<super::HrTimer<T>>();

        // SAFETY: By C API contract `ptr` is the pointer we passed when
        // queuing the timer, so it is a `HrTimer<T>` embedded in a `T`.
        let data_ptr = unsafe { T::timer_container_of(timer_ptr) };

        // SAFETY: `data_ptr` points to the `T` that was used to queue the
        // timer. This `T` is contained in an `Arc`.
        let receiver = unsafe { ArcBorrow::from_raw(data_ptr) };

        T::run(receiver).into_c()
    }
}
