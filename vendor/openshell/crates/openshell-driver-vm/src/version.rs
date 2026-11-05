// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The version this driver reports to the gateway and in its cache identities.
//!
//! This is the Firecracker plugin's release version, not `OpenShell`'s. `openshell-core`
//! derives its own version from git tags in the checkout it is built from; as a git
//! dependency, cargo's checkout has no tags, so it reports a development string such as
//! `0.0.1-dev.1934+g5d25211b2` even for a release. The plugin's build sets
//! `OPENSHELL_DRIVER_VERSION` (`driver/version.sh`: the `v*` tag being built, else a
//! development version derived from the latest tag), and this reports that instead.
//! Built without it (a plain `cargo build`), the behaviour is unchanged.

/// The release version if the build set one, else `fallback`.
const fn pick<'a>(release: Option<&'a str>, fallback: &'a str) -> &'a str {
    match release {
        Some(version) if !version.is_empty() => version,
        _ => fallback,
    }
}

/// The driver's version, for gateway negotiation, `--version`, tracing and caches.
pub const VERSION: &str = pick(
    option_env!("OPENSHELL_DRIVER_VERSION"),
    openshell_core::VERSION,
);

#[cfg(test)]
mod tests {
    use super::pick;

    #[test]
    fn a_release_version_wins() {
        assert_eq!(pick(Some("0.1.7"), "0.0.1-dev.1+gabc"), "0.1.7");
    }

    #[test]
    fn an_unset_or_empty_release_version_falls_back() {
        assert_eq!(pick(None, "0.0.1-dev.1+gabc"), "0.0.1-dev.1+gabc");
        assert_eq!(pick(Some(""), "0.0.1-dev.1+gabc"), "0.0.1-dev.1+gabc");
    }

    #[test]
    fn the_version_is_never_empty() {
        assert!(!super::VERSION.is_empty());
    }
}
