// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// `defaults-without-telemetry` is an alias for the default feature set minus
// `telemetry`, not a switch that turns telemetry off. Cargo cannot subtract a
// default feature, so adding it on top of the defaults would otherwise produce
// a telemetry-on build that reads as telemetry-free. Fail the build instead.
#[cfg(all(feature = "telemetry", feature = "defaults-without-telemetry"))]
compile_error!(
    "features `telemetry` and `defaults-without-telemetry` are mutually exclusive; \
     build a telemetry-free VM driver with `--no-default-features --features defaults-without-telemetry`"
);

#[cfg(feature = "allocation-tracking")]
pub mod allocation_tracking;

#[cfg(feature = "compute-driver")]
pub mod driver;
#[cfg(feature = "compute-driver")]
mod image_config;
#[cfg(feature = "compute-driver")]
mod isolation;
#[cfg(feature = "compute-driver")]
mod layer_applier;
#[cfg(feature = "compute-driver")]
pub mod lifecycle;
#[cfg(feature = "compute-driver")]
pub mod otel_tracing;
#[cfg(feature = "compute-driver")]
mod podman_socket;
#[cfg(feature = "compute-driver")]
pub mod procguard;
#[cfg(feature = "compute-driver")]
mod rootfs;
#[cfg(feature = "compute-driver")]
pub mod sandbox_storage;
#[cfg(feature = "compute-driver")]
pub mod supervisor_identity;
mod version;

#[cfg(feature = "compute-driver")]
pub use driver::{VmDriver, VmDriverConfig};
#[cfg(feature = "compute-driver")]
pub use lifecycle::{
    ExtensionCapabilities, ExtensionDescriptor, GuestInitDropin, LaunchAbortReason, LaunchPlan,
    LifecycleError, LifecycleExtension, LifecycleExtensionRegistry, LifecycleResult,
    RestoreContext,
};
pub use version::VERSION;
