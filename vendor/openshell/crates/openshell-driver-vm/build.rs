// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build script for openshell-driver-vm.
//!
//! This crate embeds the sandbox, host supervisor, guest init and umoci
//! artifacts it needs to prepare images and run sandboxes without a separate
//! runtime download. The VMM is an external launcher, so no VMM is embedded.

use std::path::{Path, PathBuf};
use std::{env, fs};

const ARTIFACTS: &[&str] = &[
    "openshell-sandbox.zst",
    "openshell-supervisor.zst",
    "openshell-vm-init.zst",
    "umoci.zst",
];

fn main() {
    if env::var_os("CARGO_FEATURE_COMPUTE_DRIVER").is_none() {
        return;
    }

    println!("cargo:rerun-if-env-changed=OPENSHELL_VM_RUNTIME_COMPRESSED_DIR");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));

    let Ok(dir) = env::var("OPENSHELL_VM_RUNTIME_COMPRESSED_DIR") else {
        println!("cargo:warning=OPENSHELL_VM_RUNTIME_COMPRESSED_DIR not set");
        println!("cargo:warning=Stage the compressed runtime artifacts and point it at them");
        generate_stub_resources(&out_dir);
        return;
    };
    let compressed_dir = PathBuf::from(dir);
    println!("cargo:rerun-if-changed={}", compressed_dir.display());
    assert!(
        compressed_dir.is_dir(),
        "Compressed runtime dir not found: {}",
        compressed_dir.display()
    );

    for name in ARTIFACTS {
        let src_path = compressed_dir.join(name);
        println!("cargo:rerun-if-changed={}", src_path.display());
        let metadata = fs::metadata(&src_path).unwrap_or_else(|e| {
            panic!(
                "Required compressed artifact unavailable: {}: {e}",
                src_path.display()
            )
        });
        assert!(
            metadata.is_file() && metadata.len() != 0,
            "Required compressed artifact is not a non-empty file: {}",
            src_path.display()
        );
        let dst_path = out_dir.join(name);
        let _ = fs::remove_file(&dst_path);
        fs::copy(&src_path, &dst_path).unwrap_or_else(|e| {
            panic!(
                "Failed to copy {} to {}: {e}",
                src_path.display(),
                dst_path.display()
            )
        });
        println!(
            "cargo:warning=Embedded {name}: {} bytes",
            fs::metadata(&dst_path).map_or(0, |m| m.len())
        );
    }
}

fn generate_stub_resources(out_dir: &Path) {
    for name in ARTIFACTS {
        let path = out_dir.join(name);
        if !path.exists() {
            fs::write(&path, b"")
                .unwrap_or_else(|e| panic!("Failed to write stub {}: {e}", path.display()));
        }
    }
}
