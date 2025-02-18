// SPDX-License-Identifier: GPL-2.0

//! Types for module parameters.
//!
//! C header: [`include/linux/moduleparam.h`](srctree/include/linux/moduleparam.h)

use crate::prelude::*;
use crate::str::BStr;

/// Newtype to make `bindings::kernel_param` [`Sync`].
#[repr(transparent)]
#[doc(hidden)]
pub struct RacyKernelParam(pub ::kernel::bindings::kernel_param);

// SAFETY: C kernel handles serializing access to this type. We never access
// from Rust module.
unsafe impl Sync for RacyKernelParam {}

/// Types that can be used for module parameters.
///
/// Note that displaying the type in `sysfs` will fail if
/// [`Display`](core::fmt::Display) implementation would write more than
/// [`PAGE_SIZE`] - 1 bytes.
///
/// [`PAGE_SIZE`]: `bindings::PAGE_SIZE`
pub trait ModuleParam: Sized {
    /// The [`ModuleParam`] will be used by the kernel module through this type.
    ///
    /// This may differ from `Self` if, for example, `Self` needs to track
    /// ownership without exposing it or allocate extra space for other possible
    /// parameter values.
    // This is required to support string parameters in the future.
    type Value: ?Sized;

    /// Parse a parameter argument into the parameter value.
    ///
    /// `Err(_)` should be returned when parsing of the argument fails.
    ///
    /// Parameters passed at boot time will be set before [`kmalloc`] is
    /// available (even if the module is loaded at a later time). However, in
    /// this case, the argument buffer will be valid for the entire lifetime of
    /// the kernel. So implementations of this method which need to allocate
    /// should first check that the allocator is available (with
    /// [`crate::bindings::slab_is_available`]) and when it is not available
    /// provide an alternative implementation which doesn't allocate. In cases
    /// where the allocator is not available it is safe to save references to
    /// `arg` in `Self`, but in other cases a copy should be made.
    ///
    /// [`kmalloc`]: srctree/include/linux/slab.h
    fn try_from_param_arg(arg: &'static [u8]) -> Result<Self>;
}

/// Set the module parameter from a string.
///
/// Used to set the parameter value at kernel initialization, when loading
/// the module or when set through `sysfs`.
///
/// `param.arg` is a pointer to `*mut T` as set up by the [`module!`]
/// macro.
///
/// See `struct kernel_param_ops.set`.
///
/// # Safety
///
/// If `val` is non-null then it must point to a valid null-terminated
/// string. The `arg` field of `param` must be an instance of `T`.
///
/// # Invariants
///
/// Currently, we only support read-only parameters that are not readable
/// from `sysfs`. Thus, this function is only called at kernel
/// initialization time, or at module load time, and we have exclusive
/// access to the parameter for the duration of the function.
///
/// [`module!`]: macros::module
unsafe extern "C" fn set_param<T>(
    val: *const kernel::ffi::c_char,
    param: *const crate::bindings::kernel_param,
) -> core::ffi::c_int
where
    T: ModuleParam,
{
    // NOTE: If we start supporting arguments without values, val _is_ allowed
    // to be null here.
    if val.is_null() {
        // TODO: Use pr_warn_once available.
        crate::pr_warn!("Null pointer passed to `module_param::set_param`");
        return crate::error::code::EINVAL.to_errno();
    }

    // SAFETY: By function safety requirement, val is non-null and
    // null-terminated. By C API contract, `val` is live and valid for reads
    // for the duration of this function.
    let arg = unsafe { CStr::from_char_ptr(val).as_bytes() };

    crate::error::from_result(|| {
        let new_value = T::try_from_param_arg(arg)?;

        // SAFETY: `param` is guaranteed to be valid by C API contract
        // and `arg` is guaranteed to point to an instance of `T`.
        let old_value = unsafe { (*param).__bindgen_anon_1.arg as *mut T };

        // SAFETY: `old_value` is valid for writes, as we have exclusive
        // access. `old_value` is pointing to an initialized static, and
        // so it is properly initialized.
        unsafe { core::ptr::replace(old_value, new_value) };
        Ok(0)
    })
}

/// Drop the parameter.
///
/// Called when unloading a module.
///
/// # Safety
///
/// The `arg` field of `param` must be an initialized instance of `T`.
unsafe extern "C" fn free<T>(arg: *mut core::ffi::c_void)
where
    T: ModuleParam,
{
    // SAFETY: By function safety requirement, `arg` is an initialized
    // instance of `T`. By C API contract, `arg` will not be used after
    // this function returns.
    unsafe { core::ptr::drop_in_place(arg as *mut T) };
}

macro_rules! impl_int_module_param {
    ($ty:ident) => {
        impl ModuleParam for $ty {
            type Value = $ty;

            fn try_from_param_arg(arg: &'static [u8]) -> Result<Self> {
                let bstr = BStr::from_bytes(arg);
                <$ty as crate::str::parse_int::ParseInt>::from_str(bstr)
            }
        }
    };
}

impl_int_module_param!(i8);
impl_int_module_param!(u8);
impl_int_module_param!(i16);
impl_int_module_param!(u16);
impl_int_module_param!(i32);
impl_int_module_param!(u32);
impl_int_module_param!(i64);
impl_int_module_param!(u64);
impl_int_module_param!(isize);
impl_int_module_param!(usize);

/// A wrapper for kernel parameters.
///
/// This type is instantiated by the [`module!`] macro when module parameters are
/// defined. You should never need to instantiate this type directly.
#[repr(transparent)]
pub struct ModuleParamAccess<T> {
    data: core::cell::UnsafeCell<T>,
}

// SAFETY: We only create shared references to the contents of this container,
// so if `T` is `Sync`, so is `ModuleParamAccess`.
unsafe impl<T: Sync> Sync for ModuleParamAccess<T> {}

impl<T> ModuleParamAccess<T> {
    #[doc(hidden)]
    pub const fn new(value: T) -> Self {
        Self {
            data: core::cell::UnsafeCell::new(value),
        }
    }

    /// Get a shared reference to the parameter value.
    // Note: When sysfs access to parameters are enabled, we have to pass in a
    // held lock guard here.
    pub fn get(&self) -> &T {
        // SAFETY: As we only support read only parameters with no sysfs
        // exposure, the kernel will not touch the parameter data after module
        // initialization.
        unsafe { &*self.data.get() }
    }

    /// Get a mutable pointer to the parameter value.
    pub const fn as_mut_ptr(&self) -> *mut T {
        self.data.get()
    }
}

#[doc(hidden)]
#[macro_export]
/// Generate a static [`kernel_param_ops`](srctree/include/linux/moduleparam.h) struct.
///
/// # Examples
///
/// ```ignore
/// make_param_ops!(
///     /// Documentation for new param ops.
///     PARAM_OPS_MYTYPE, // Name for the static.
///     MyType // A type which implements [`ModuleParam`].
/// );
/// ```
macro_rules! make_param_ops {
    ($ops:ident, $ty:ty) => {
        ///
        /// Static [`kernel_param_ops`](srctree/include/linux/moduleparam.h)
        /// struct generated by `make_param_ops`
        #[doc = concat!("for [`", stringify!($ty), "`].")]
        pub static $ops: $crate::bindings::kernel_param_ops = $crate::bindings::kernel_param_ops {
            flags: 0,
            set: Some(set_param::<$ty>),
            get: None,
            free: Some(free::<$ty>),
        };
    };
}

make_param_ops!(PARAM_OPS_I8, i8);
make_param_ops!(PARAM_OPS_U8, u8);
make_param_ops!(PARAM_OPS_I16, i16);
make_param_ops!(PARAM_OPS_U16, u16);
make_param_ops!(PARAM_OPS_I32, i32);
make_param_ops!(PARAM_OPS_U32, u32);
make_param_ops!(PARAM_OPS_I64, i64);
make_param_ops!(PARAM_OPS_U64, u64);
make_param_ops!(PARAM_OPS_ISIZE, isize);
make_param_ops!(PARAM_OPS_USIZE, usize);
