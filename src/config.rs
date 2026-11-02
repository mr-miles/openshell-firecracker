// SPDX-License-Identifier: Apache-2.0

//! The Firecracker `--config-file` document for one VM generation.
//!
//! Pure data: nothing here touches files or processes, so the device model and
//! kernel command line stay unit-testable on hosts without KVM.
//!
//! The device model mirrors the stock VM driver's other backends so the guest
//! init needs no Firecracker-specific code: `/dev/vda` is the read-only
//! bootstrap root, `/dev/vdb` the per-sandbox writable overlay, and `/dev/vdc`
//! the optional read-only prepared image disk. The guest has no network device.
//! The only host channel is the vsock device.

use std::path::Path;

use serde_json::{Value, json};

/// Guest CID of the only vsock device. Every microVM has its own device, so
/// CIDs do not have to be unique across VMs.
pub const GUEST_CID: u32 = 3;

/// Basename of Firecracker's own vsock endpoint inside the VMM socket
/// directory. Firecracker also connects to `<this>_<port>` for every
/// guest-initiated connection to host port `<port>`.
pub const VSOCK_UDS_NAME: &str = "fc";

/// Per-VM directory holding only the Firecracker vsock sockets. It is the only
/// socket directory visible inside the VMM jail.
pub const VMM_SOCKET_SUBDIR: &str = "vmm";

/// Firecracker supports at most this many vCPUs per microVM.
const MAX_VCPUS: u8 = 32;

/// Oldest Firecracker the generated config supports: the `entropy` device
/// arrived in 1.4.
pub const MIN_FIRECRACKER_VERSION: (u32, u32, u32) = (1, 4, 0);

/// Basename of the bridge socket Firecracker connects to for a guest-initiated
/// vsock port.
pub fn bridge_socket_name(guest_port: u32) -> String {
    format!("{VSOCK_UDS_NAME}_{guest_port}")
}

/// Extract `(major, minor, patch)` from `firecracker --version` output such as
/// `Firecracker v1.10.1`.
pub fn parse_firecracker_version(output: &str) -> Option<(u32, u32, u32)> {
    let token = output.split_whitespace().find_map(|word| {
        word.strip_prefix('v')
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    })?;
    let mut parts = token.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// Kernel command line. Firecracker appends `root=/dev/vda` and `ro` itself
/// for the root drive and adds the virtio-mmio device parameters.
pub fn boot_args(init_path: &str) -> Result<String, String> {
    if !init_path.starts_with('/')
        || init_path
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"')
    {
        return Err(format!("invalid guest init path: {init_path:?}"));
    }
    Ok(format!(
        "console=ttyS0 reboot=k panic=-1 pci=off random.trust_cpu=on \
         i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd \
         rootfstype=ext4 init={init_path}"
    ))
}

/// Inputs for [`VmmConfig::render`].
pub struct VmmConfig<'a> {
    pub kernel: &'a Path,
    pub root_disk: &'a Path,
    pub overlay_disk: &'a Path,
    pub image_disk: Option<&'a Path>,
    pub vcpus: u8,
    pub mem_mib: u32,
    pub init_path: &'a str,
    /// Firecracker's `uds_path`. `None` omits the vsock device (image
    /// preparation has no host channel).
    pub vsock_uds: Option<&'a Path>,
}

fn utf8(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

impl VmmConfig<'_> {
    pub fn render(&self) -> Result<Value, String> {
        if self.vcpus == 0 || self.vcpus > MAX_VCPUS {
            return Err(format!(
                "Firecracker supports 1 to {MAX_VCPUS} vCPUs, got {}",
                self.vcpus
            ));
        }
        if self.mem_mib == 0 {
            return Err("Firecracker guest memory must be greater than zero".to_string());
        }

        // Order fixes the guest device names: vda, vdb, vdc.
        let mut drives = vec![
            json!({
                "drive_id": "root",
                "path_on_host": utf8(self.root_disk)?,
                "is_root_device": true,
                "is_read_only": true,
            }),
            json!({
                "drive_id": "overlay",
                "path_on_host": utf8(self.overlay_disk)?,
                "is_root_device": false,
                "is_read_only": false,
                // Honor guest flushes so fsync reaches the host file and a
                // stopped sandbox keeps what it committed.
                "cache_type": "Writeback",
            }),
        ];
        if let Some(image) = self.image_disk {
            drives.push(json!({
                "drive_id": "image",
                "path_on_host": utf8(image)?,
                "is_root_device": false,
                "is_read_only": true,
            }));
        }

        let mut machine = json!({
            "vcpu_count": self.vcpus,
            "mem_size_mib": self.mem_mib,
        });
        // Firecracker rejects `smt` outside x86_64.
        #[cfg(target_arch = "x86_64")]
        {
            machine["smt"] = json!(false);
        }

        let mut config = json!({
            "boot-source": {
                "kernel_image_path": utf8(self.kernel)?,
                "boot_args": boot_args(self.init_path)?,
            },
            "drives": drives,
            "machine-config": machine,
            // No `network-interfaces`: the guest has no NIC by construction.
            "entropy": {},
        });
        if let Some(uds) = self.vsock_uds {
            config["vsock"] = json!({
                "guest_cid": GUEST_CID,
                "uds_path": utf8(uds)?,
            });
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample<'a>(image: Option<&'a Path>, uds: Option<&'a Path>) -> VmmConfig<'a> {
        VmmConfig {
            kernel: Path::new("/k/vmlinux"),
            root_disk: Path::new("/c/root.ext4"),
            overlay_disk: Path::new("/s/overlay.ext4"),
            image_disk: image,
            vcpus: 2,
            mem_mib: 2048,
            init_path: "/srv/openshell-vm-sandbox-init.sh",
            vsock_uds: uds,
        }
    }

    #[test]
    fn drives_keep_the_guest_device_order_and_access() {
        let cfg = sample(Some(Path::new("/c/image.ext4")), None)
            .render()
            .unwrap();
        let drives = cfg["drives"].as_array().unwrap();
        let ids: Vec<_> = drives
            .iter()
            .map(|d| d["drive_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["root", "overlay", "image"]);
        assert_eq!(drives[0]["is_root_device"], true);
        assert_eq!(drives[0]["is_read_only"], true);
        assert_eq!(drives[1]["is_read_only"], false);
        assert_eq!(drives[1]["cache_type"], "Writeback");
        assert_eq!(drives[2]["is_read_only"], true);
    }

    #[test]
    fn image_disk_is_optional() {
        let cfg = sample(None, None).render().unwrap();
        assert_eq!(cfg["drives"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_guest_never_gets_a_network_interface() {
        let cfg = sample(None, Some(Path::new("/s/vmm/fc"))).render().unwrap();
        assert!(cfg.get("network-interfaces").is_none());
        assert_eq!(cfg["vsock"]["guest_cid"], GUEST_CID);
        assert_eq!(cfg["vsock"]["uds_path"], "/s/vmm/fc");
    }

    #[test]
    fn vsock_is_omitted_without_a_host_channel() {
        assert!(sample(None, None).render().unwrap().get("vsock").is_none());
    }

    #[test]
    fn machine_config_matches_the_request() {
        let cfg = sample(None, None).render().unwrap();
        assert_eq!(cfg["machine-config"]["vcpu_count"], 2);
        assert_eq!(cfg["machine-config"]["mem_size_mib"], 2048);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(cfg["machine-config"]["smt"], false);
        #[cfg(not(target_arch = "x86_64"))]
        assert!(cfg["machine-config"].get("smt").is_none());
    }

    #[test]
    fn rejects_unbootable_sizes() {
        for vcpus in [0, 33] {
            let mut c = sample(None, None);
            c.vcpus = vcpus;
            assert!(c.render().is_err(), "{vcpus} vCPUs");
        }
        let mut c = sample(None, None);
        c.mem_mib = 0;
        assert!(c.render().is_err());
    }

    #[test]
    fn boot_args_pin_the_init_and_leave_root_to_firecracker() {
        let args = boot_args("/srv/openshell-vm-sandbox-init.sh").unwrap();
        assert!(args.contains("init=/srv/openshell-vm-sandbox-init.sh"));
        assert!(args.contains("pci=off"));
        assert!(
            !args.contains("root=/dev"),
            "Firecracker adds the root device"
        );
    }

    #[test]
    fn boot_args_reject_command_line_injection() {
        for bad in ["init.sh", "/a b", "/a\tb", "/a\nb", "/a\"b", ""] {
            assert!(boot_args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn firecracker_versions_parse() {
        assert_eq!(
            parse_firecracker_version("Firecracker v1.10.1\n"),
            Some((1, 10, 1))
        );
        assert_eq!(
            parse_firecracker_version("Firecracker v1.13.0-dev"),
            Some((1, 13, 0))
        );
        assert_eq!(
            parse_firecracker_version("Firecracker v1.4"),
            Some((1, 4, 0))
        );
        assert_eq!(parse_firecracker_version("not firecracker"), None);
        assert_eq!(parse_firecracker_version("version vX.Y"), None);
        assert!(parse_firecracker_version("Firecracker v1.3.3").unwrap() < MIN_FIRECRACKER_VERSION);
    }

    #[test]
    fn the_bridge_socket_is_named_for_the_port() {
        assert_eq!(bridge_socket_name(5500), "fc_5500");
    }
}
