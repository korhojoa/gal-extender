// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Native receiver ABI types and interface facts for 32-bit ARM.
//!
//! The symbols module declares mangled names and typed function pointers.
//! The offsets module declares fields accessed in receiver objects. The
//! endpoint module defines the additive eight-slot `ProtocolEndpointBase`
//! interface used by the cluster module.
//!
//! Member calls pass `this` first. references use pointers. Boolean arguments
//! use the width declared by each native signature. ABI declarations do not
//! prove that an unknown firmware implements a compatible object layout.

#![no_std]

pub mod endpoint;
pub mod offsets;
pub mod symbols;

use core::ffi::{c_void, CStr};
use core::marker::PhantomData;

/// `void*` in the signatures below.
pub type Ptr = *mut c_void;
/// `const void*` in the signatures below.
pub type ConstPtr = *const c_void;

/// A gal or vendor symbol with the function type the extension libraries call it with.
pub struct Symbol<F> {
    name: &'static CStr,
    _f: PhantomData<F>,
}

impl<F: Copy> Symbol<F> {
    /// # Safety
    /// `F` must be the function pointer type of the symbol `name`.
    #[must_use]
    pub const unsafe fn new(name: &'static CStr) -> Self {
        Symbol {
            name,
            _f: PhantomData,
        }
    }

    /// The mangled name.
    #[must_use]
    pub const fn name(&self) -> &'static CStr {
        self.name
    }

    /// Look the symbol up in the process's default scope (gal and its
    /// libraries), as the extension libraries' `dlsym(RTLD_DEFAULT, ...)` does.
    #[must_use]
    pub fn resolve(&self) -> Option<F> {
        // SAFETY: `new`'s contract types the symbol.
        unsafe { qnx_dl::symbol::<F>(qnx_dl::RTLD_DEFAULT, self.name) }
    }
}

/// A `&'static CStr` from a string literal, at compile time.
#[macro_export]
#[doc(hidden)]
macro_rules! __cstr {
    ($s:literal) => {
        match ::core::ffi::CStr::from_bytes_with_nul(concat!($s, "\0").as_bytes()) {
            Ok(c) => c,
            Err(_) => panic!("symbol name with a NUL byte"),
        }
    };
}
