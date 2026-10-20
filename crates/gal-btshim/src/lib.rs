// SPDX-License-Identifier: GPL-3.0-only
//! Native AA bootstrap. Hardware entry points compile only for ARM QNX.
//! Loading the library is inert unless `GAL_NATIVE_BT=1`.
pub mod config;
pub mod engine;
#[cfg_attr(not(target_os = "nto"), allow(dead_code))]
mod events;
pub mod identity;
pub mod nudge_input;
#[cfg_attr(not(feature = "native-check"), allow(dead_code))]
mod nudge_state;
#[cfg_attr(not(target_os = "nto"), allow(dead_code))]
mod ownership;
pub mod policy;
pub mod policy_input;
pub mod pps_io;
pub mod sdp;
pub mod session;
pub mod startup;
#[cfg_attr(not(feature = "native-check"), allow(dead_code))]
mod status;

#[cfg(any(
    all(target_os = "nto", target_arch = "arm", target_pointer_width = "32"),
    feature = "native-check"
))]
mod native;

// Compile the complete native call graph without executing the constructor.
#[cfg(all(feature = "native-check", not(target_os = "nto")))]
#[used]
static NATIVE_CHECK: extern "C" fn() = native::initialize;

#[cfg(all(target_os = "nto", target_arch = "arm", target_pointer_width = "32"))]
#[used]
#[link_section = ".init_array"]
static INIT: extern "C" fn() = native::initialize;
