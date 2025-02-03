// SPDX-License-Identifier: GPL-2.0

use super::HasHrTimer;
use super::HrTimer;
use super::HrTimerCallback;
use super::HrTimerHandle;
use super::HrTimerPointer;
use super::RawHrTimerCallback;
use crate::prelude::*;
use crate::time::Ktime;
use core::mem::ManuallyDrop;

/// A handle for a [`Box<HasHrTimer<T>>`] returned by a call to
/// [`HrTimerPointer::start`].
pub struct BoxHrTimerHandle<T, A>
where
    T: HasHrTimer<T>,
    A: crate::alloc::Allocator,
{
    pub(crate) inner: *mut T,
    _p: core::marker::PhantomData<A>,
}

// SAFETY: We implement drop below, and we cancel the timer in the drop
// implementation.
unsafe impl<T, A> HrTimerHandle for BoxHrTimerHandle<T, A>
where
    T: HasHrTimer<T>,
    A: crate::alloc::Allocator,
{
    fn cancel(&mut self) -> bool {
        // SAFETY: As we obtained `self.inner` from a valid reference when we
        // created `self`, it must point to a valid `T`.
        let timer_ptr = unsafe { <T as HasHrTimer<T>>::raw_get_timer(self.inner) };

        // SAFETY: As `timer_ptr` points into `T` and `T` is valid, `timer_ptr`
        // must point to a valid `HrTimer` instance.
        unsafe { HrTimer::<T>::raw_cancel(timer_ptr) }
    }
}

impl<T, A> Drop for BoxHrTimerHandle<T, A>
where
    T: HasHrTimer<T>,
    A: crate::alloc::Allocator,
{
    fn drop(&mut self) {
        self.cancel();
        // SAFETY: `self.inner` came from a `Box::into_raw` call
        drop(unsafe { Box::<T, A>::from_raw(self.inner) })
    }
}

impl<T, A> HrTimerPointer for Pin<Box<T, A>>
where
    T: Send + Sync,
    T: HasHrTimer<T>,
    T: for<'a> HrTimerCallback<CallbackTarget<'a> = Pin<Box<T, A>>>,
    T: for<'a> HrTimerCallback<CallbackTargetParameter<'a> = Pin<&'a T>>,
    A: crate::alloc::Allocator,
{
    type TimerHandle = BoxHrTimerHandle<T, A>;

    fn start(self, expires: Ktime) -> Self::TimerHandle {
        let self_ptr: *const T = <Self as core::ops::Deref>::deref(&self);

        // SAFETY: Since we generate the pointer passed to `start` from a valid
        // reference, it is a valid pointer.
        unsafe { T::start(self_ptr, expires) };

        // SAFETY: We will not move out of this box during timer callback (we
        // pass an immutable reference to the callback).
        let inner = unsafe { Pin::into_inner_unchecked(self) };

        BoxHrTimerHandle {
            inner: Box::into_raw(inner),
            _p: core::marker::PhantomData,
        }
    }
}

impl<T, A> RawHrTimerCallback for Pin<Box<T, A>>
where
    T: HasHrTimer<T>,
    T: for<'a> HrTimerCallback<CallbackTarget<'a> = Pin<Box<T, A>>>,
    T: for<'a> HrTimerCallback<CallbackTargetParameter<'a> = Pin<&'a T>>,
    A: crate::alloc::Allocator,
{
    unsafe extern "C" fn run(ptr: *mut bindings::hrtimer) -> bindings::hrtimer_restart {
        // `HrTimer` is `repr(C)`
        let timer_ptr = ptr.cast::<super::HrTimer<T>>();

        // SAFETY: By C API contract `ptr` is the pointer we passed when
        // queuing the timer, so it is a `HrTimer<T>` embedded in a `T`.
        let data_ptr = unsafe { T::timer_container_of(timer_ptr) };

        // SAFETY: We called `Box::into_raw` when we queued the timer.
        let tbox = ManuallyDrop::new(Box::into_pin(unsafe { Box::<T, A>::from_raw(data_ptr) }));

        T::run(tbox.as_ref()).into()
    }
}
