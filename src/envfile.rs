// SPDX-License-Identifier: Apache-2.0

//! Guest environment, injected into the overlay disk.
//!
//! Firecracker cannot pass the guest init an environment, so the launcher writes
//! a small shell file into the overlay disk before boot, which the guest init
//! sources early (`/overlay/upper/srv/openshell-env.sh`).
//!
//! The file also sets the guest clock: Firecracker has no RTC on x86_64, so
//! the guest would start in 1970 and fail certificate validity checks.
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

/// The shell text for `env` (`KEY=value` entries) and the guest clock.
pub fn render(env: &[String], host_epoch: u64) -> Result<String, String> {
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

    #[test]
    fn variables_are_exported_and_escaped() {
        let text = render(&["A=1".into(), "B=say \"$x\" `id`".into()], 7).unwrap();
        assert!(text.contains("export A=\"1\"\n"));
        assert!(text.contains("export B=\"say \\\"\\$x\\\" \\`id\\`\"\n"));
    }

    #[test]
    fn values_may_contain_equals_signs() {
        let text = render(&["URL=http://h/?a=b".into()], 7).unwrap();
        assert!(text.contains("export URL=\"http://h/?a=b\"\n"));
    }

    #[test]
    fn bad_entries_are_rejected() {
        assert!(render(&["NOEQUALS".into()], 7).is_err());
        assert!(render(&["1BAD=x".into()], 7).is_err());
        assert!(render(&["BAD NAME=x".into()], 7).is_err());
        assert!(render(&["=x".into()], 7).is_err());
    }

    #[test]
    fn the_clock_only_moves_forward() {
        let text = render(&[], 1_700_000_000).unwrap();
        assert!(text.contains("__fc_epoch=1700000000"));
        assert!(text.contains("-lt \"$__fc_epoch\""));
        assert!(text.contains("date -u -s"));
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

        let first = render(&["A=1".into()], 100).unwrap();
        write_into_overlay(&image, &first).unwrap();
        // Writing again replaces the file.
        let second = render(&["A=2".into()], 200).unwrap();
        write_into_overlay(&image, &second).unwrap();
    }
}
