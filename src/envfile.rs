// SPDX-License-Identifier: Apache-2.0

//! Guest environment, injected into the overlay disk.
//!
//! Firecracker cannot pass the guest init an environment, so the launcher writes
//! a small shell file into the overlay disk before boot, which the guest init
//! sources early (`/overlay/upper/srv/openshell-env.sh`).
//!
//! The file also sets the guest clock: Firecracker has no RTC on x86_64, so
//! the guest would start in 1970 and fail certificate validity checks. With a
//! zram size, it also turns `/dev/zram0` into compressed swap.
//!
//! The overlay is an ext4 image; it is edited with `debugfs` (package
//! e2fsprogs), which the VM driver already requires.

use std::path::Path;
use std::process::Command;

/// Where the guest init looks for the file, inside the overlay image.
const GUEST_ENV_PATH: &str = "/upper/srv/openshell-env.sh";

/// Escape a string for use inside bash double quotes.
fn shell_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Makes `/dev/zram0` swap of `$__fc_zram_mib` MiB.
///
/// The guest init sources the env file under `set -euo pipefail`, so nothing
/// here may fail: a guest without zram, `mkswap` or `swapon` boots without
/// swap and says so on the console. The init sources the file a second time
/// later in boot: `__fc_zram_tried` makes that a no-op in the same shell, and
/// a zram device that already has a size is left alone in any other.
const ZRAM_SWAP: &str = r#"if [ -n "${__fc_zram_tried:-}" ]; then
    :
elif [ -w /sys/block/zram0/disksize ] \
    && [ "$(cat /sys/block/zram0/disksize 2>/dev/null || echo 1)" = 0 ]; then
    if command -v mkswap >/dev/null 2>&1 && command -v swapon >/dev/null 2>&1; then
        if { echo "${__fc_zram_mib}M" >/sys/block/zram0/disksize; } 2>/dev/null \
            && mkswap /dev/zram0 >/dev/null 2>&1 \
            && swapon -p 100 /dev/zram0 >/dev/null 2>&1; then
            echo "openshell-firecracker: zram swap on (${__fc_zram_mib} MiB)" >&2
        else
            echo "openshell-firecracker: zram swap setup failed; continuing without swap" >&2
        fi
    else
        echo "openshell-firecracker: no mkswap or swapon in the guest; continuing without swap" >&2
    fi
elif [ ! -e /sys/block/zram0 ]; then
    echo "openshell-firecracker: the guest kernel has no zram; continuing without swap" >&2
fi
__fc_zram_tried=1
unset __fc_zram_mib
"#;

/// The shell text for `env` (`KEY=value` entries), the guest clock and, when
/// `zram_mib` is above zero, zram swap of that size.
pub fn render(env: &[String], host_epoch: u64, zram_mib: u64) -> Result<String, String> {
    let mut out = String::new();
    for entry in env {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("environment entry has no '=': {entry:?}"))?;
        if !valid_name(name) {
            return Err(format!("invalid environment variable name: {name:?}"));
        }
        out.push_str(&format!("export {name}=\"{}\"\n", shell_escape(value)));
    }
    // Only ever moves the clock forward.
    out.push_str(&format!(
        "__fc_epoch={host_epoch}\n\
         if [ \"$(date +%s 2>/dev/null || echo 0)\" -lt \"$__fc_epoch\" ]; then\n\
         \x20   date -u -s \"@$__fc_epoch\" >/dev/null 2>&1 || true\n\
         fi\n\
         unset __fc_epoch\n"
    ));
    if zram_mib > 0 {
        out.push_str(&format!("__fc_zram_mib={zram_mib}\n"));
        out.push_str(ZRAM_SWAP);
    }
    Ok(out)
}

fn plain_path(path: &Path) -> Result<&str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    if text.is_empty()
        || text
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        return Err(format!(
            "path must not contain whitespace or quotes: {}",
            path.display()
        ));
    }
    Ok(text)
}

/// Write `content` to the guest env path inside the ext4 image `overlay`,
/// replacing any earlier file, then read it back to confirm.
pub fn write_into_overlay(overlay: &Path, content: &str) -> Result<(), String> {
    let image = plain_path(overlay)?;
    let staged = overlay.with_extension("env.tmp");
    let commands = overlay.with_extension("env.cmds");
    let staged_text = plain_path(&staged)?;

    std::fs::write(&staged, content).map_err(|error| format!("stage env file: {error}"))?;
    let script = format!(
        "mkdir /upper\nmkdir /upper/srv\nrm {GUEST_ENV_PATH}\nwrite {staged_text} {GUEST_ENV_PATH}\n"
    );
    let result = std::fs::write(&commands, script)
        .map_err(|error| format!("stage debugfs script: {error}"))
        .and_then(|()| {
            // mkdir and rm fail harmlessly when the target exists or is absent.
            Command::new("debugfs")
                .args(["-w", "-f"])
                .arg(&commands)
                .arg(image)
                .output()
                .map_err(|error| format!("run debugfs (package e2fsprogs): {error}"))
        })
        .and_then(|_| read_back(image, content));
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_file(&commands);
    result
}

fn read_back(image: &str, expected: &str) -> Result<(), String> {
    let output = Command::new("debugfs")
        .args(["-R", &format!("cat {GUEST_ENV_PATH}"), image])
        .output()
        .map_err(|error| format!("run debugfs: {error}"))?;
    if output.stdout == expected.as_bytes() {
        Ok(())
    } else {
        Err(format!(
            "the guest environment file was not written to {image}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn variables_are_exported_and_escaped() {
        let text = render(&["A=1".into(), "B=say \"$x\" `id`".into()], 7, 0).unwrap();
        assert!(text.contains("export A=\"1\"\n"));
        assert!(text.contains("export B=\"say \\\"\\$x\\\" \\`id\\`\"\n"));
    }

    #[test]
    fn values_may_contain_equals_signs() {
        let text = render(&["URL=http://h/?a=b".into()], 7, 0).unwrap();
        assert!(text.contains("export URL=\"http://h/?a=b\"\n"));
    }

    #[test]
    fn bad_entries_are_rejected() {
        assert!(render(&["NOEQUALS".into()], 7, 0).is_err());
        assert!(render(&["1BAD=x".into()], 7, 0).is_err());
        assert!(render(&["BAD NAME=x".into()], 7, 0).is_err());
        assert!(render(&["=x".into()], 7, 0).is_err());
    }

    #[test]
    fn the_clock_only_moves_forward() {
        let text = render(&[], 1_700_000_000, 0).unwrap();
        assert!(text.contains("__fc_epoch=1700000000"));
        assert!(text.contains("-lt \"$__fc_epoch\""));
        assert!(text.contains("date -u -s"));
    }

    #[test]
    fn zram_swap_is_only_set_up_with_a_size() {
        let off = render(&[], 7, 0).unwrap();
        assert!(!off.contains("zram"), "{off}");
        let on = render(&[], 7, 1024).unwrap();
        assert!(on.contains("__fc_zram_mib=1024\n"), "{on}");
        assert!(on.contains("swapon -p 100 /dev/zram0"), "{on}");
        assert!(on.ends_with("unset __fc_zram_mib\n"), "{on}");
    }

    /// A guest stand-in for the zram snippet: a fake sysfs node and device, and
    /// a `PATH` holding only `cat` and whichever fake tools a case asks for.
    struct Guest {
        dir: tempfile::TempDir,
        bash: PathBuf,
    }

    impl Guest {
        /// `None` where bash or cat is missing, so the test is skipped.
        fn new(zram: bool, tools: &[(&str, &str)]) -> Option<Self> {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let find = |name: &str| {
                ["/bin", "/usr/bin"]
                    .iter()
                    .map(|dir| Path::new(dir).join(name))
                    .find(|path| path.is_file())
            };
            let (bash, cat) = (find("bash")?, find("cat")?);
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            symlink(cat, bin.join("cat")).unwrap();
            for (name, body) in tools {
                let path = bin.join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            if zram {
                std::fs::create_dir(dir.path().join("zram0")).unwrap();
                std::fs::write(dir.path().join("zram0/disksize"), "0\n").unwrap();
            }
            Some(Self { dir, bash })
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// Source the env file twice under the guest init's shell options,
        /// as the init does. Returns (stdout, stderr).
        fn boot(&self, zram_mib: u64) -> (String, String) {
            let sys = self.path("zram0");
            let dev = self.path("dev-zram0");
            let text = render(&[], 7, zram_mib)
                .unwrap()
                .replace("/sys/block/zram0", sys.to_str().unwrap())
                .replace("/dev/zram0", dev.to_str().unwrap())
                // The clock is not under test and must not touch the host's.
                .replace("date -u -s", "true");
            let file = self.path("openshell-env.sh");
            std::fs::write(&file, text).unwrap();
            let output = Command::new(&self.bash)
                .env_clear()
                .env("PATH", self.path("bin"))
                .env("LOG", self.path("calls"))
                .arg("-c")
                .arg("set -euo pipefail; source \"$1\"; source \"$1\"; echo booted")
                .arg("init")
                .arg(&file)
                .output()
                .unwrap();
            (
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.path("calls")).unwrap_or_default()
        }
    }

    const RECORD: &str = r#"echo "${0##*/} $*" >> "$LOG""#;

    #[test]
    fn zram_swap_is_set_up_once() {
        let Some(guest) = Guest::new(true, &[("mkswap", RECORD), ("swapon", RECORD)]) else {
            return;
        };
        let (stdout, stderr) = guest.boot(1024);
        assert_eq!(stdout, "booted\n", "{stderr}");
        assert_eq!(
            std::fs::read_to_string(guest.path("zram0/disksize")).unwrap(),
            "1024M\n"
        );
        let dev = guest.path("dev-zram0");
        assert_eq!(
            guest.calls(),
            format!("mkswap {0}\nswapon -p 100 {0}\n", dev.display()),
            "the second source leaves the sized device alone"
        );
        assert_eq!(
            stderr.matches("zram swap on (1024 MiB)").count(),
            1,
            "{stderr}"
        );
    }

    #[test]
    fn a_guest_without_zram_support_still_boots() {
        // No mkswap or swapon in the guest.
        let Some(guest) = Guest::new(true, &[]) else {
            return;
        };
        let (stdout, stderr) = guest.boot(1024);
        assert_eq!(stdout, "booted\n", "{stderr}");
        assert!(stderr.contains("no mkswap or swapon"), "{stderr}");

        // A kernel without zram.
        let guest = Guest::new(false, &[("mkswap", RECORD), ("swapon", RECORD)]).unwrap();
        let (stdout, stderr) = guest.boot(1024);
        assert_eq!(stdout, "booted\n", "{stderr}");
        assert!(stderr.contains("the guest kernel has no zram"), "{stderr}");
        assert_eq!(guest.calls(), "");

        // A failing mkswap.
        let guest = Guest::new(true, &[("mkswap", "exit 1"), ("swapon", RECORD)]).unwrap();
        let (stdout, stderr) = guest.boot(1024);
        assert_eq!(stdout, "booted\n", "{stderr}");
        assert!(stderr.contains("zram swap setup failed"), "{stderr}");
        assert_eq!(guest.calls(), "", "swapon is not tried");
    }

    #[test]
    fn paths_with_spaces_or_quotes_are_refused() {
        assert!(plain_path(Path::new("/a/b.ext4")).is_ok());
        assert!(plain_path(Path::new("/a b/c")).is_err());
        assert!(plain_path(Path::new("/a\"b")).is_err());
    }

    /// Needs e2fsprogs; skipped where it is missing.
    #[test]
    fn the_file_round_trips_through_an_ext4_image() {
        let have = |tool: &str| {
            Command::new(tool)
                .arg("-V")
                .output()
                .is_ok_and(|out| out.status.success() || !out.stderr.is_empty())
        };
        if !have("mke2fs") || !have("debugfs") {
            eprintln!("skipping: e2fsprogs not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("overlay.ext4");
        std::fs::File::create(&image)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        let made = Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-F"])
            .arg(&image)
            .output()
            .unwrap();
        assert!(made.status.success(), "mke2fs failed");

        let first = render(&["A=1".into()], 100, 0).unwrap();
        write_into_overlay(&image, &first).unwrap();
        // Writing again replaces the file.
        let second = render(&["A=2".into()], 200, 0).unwrap();
        write_into_overlay(&image, &second).unwrap();
    }
}
