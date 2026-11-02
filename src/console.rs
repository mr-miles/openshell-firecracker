// SPDX-License-Identifier: Apache-2.0

//! Capture of the guest serial console and VMM diagnostics.
//!
//! Firecracker writes the guest's serial console to its stdout and its own
//! diagnostics to stderr. Both land in the sandbox's console log under a fixed
//! budget, so a guest cannot fill the host's disk through its console and the
//! VMM never blocks on a full pipe.
//!
//! The log keeps the start of the output (how the guest booted) and the end
//! (why it stopped). The first [`HEAD_LIMIT`] bytes are written as they
//! arrive. After that, output is held in a ring of the last [`TAIL_LIMIT`]
//! bytes, which is appended with a truncation marker when the VMM exits.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Bytes written to the log as they arrive.
pub const HEAD_LIMIT: u64 = 1 << 20;
/// Most recent bytes kept after the head is full, written when the VMM exits.
pub const TAIL_LIMIT: usize = 4 << 20;

/// Which bytes of a console stream reach the log.
struct Retention {
    head_limit: u64,
    tail_limit: usize,
    head_written: u64,
    tail: VecDeque<u8>,
    dropped: u64,
}

impl Retention {
    fn new(head_limit: u64, tail_limit: usize) -> Self {
        Self {
            head_limit,
            tail_limit,
            head_written: 0,
            tail: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Take `data`; returns the prefix to write to the log now. The rest goes
    /// to the tail ring, evicting the oldest bytes when it is full.
    fn push<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        let room = usize::try_from(self.head_limit.saturating_sub(self.head_written))
            .unwrap_or(usize::MAX);
        let (now, rest) = data.split_at(room.min(data.len()));
        self.head_written += now.len() as u64;
        if rest.is_empty() {
            return now;
        }
        if rest.len() >= self.tail_limit {
            self.dropped += (self.tail.len() + rest.len() - self.tail_limit) as u64;
            self.tail.clear();
            self.tail
                .extend(rest[rest.len() - self.tail_limit..].iter().copied());
        } else {
            let overflow = (self.tail.len() + rest.len()).saturating_sub(self.tail_limit);
            if overflow > 0 {
                self.tail.drain(..overflow);
                self.dropped += overflow as u64;
            }
            self.tail.extend(rest.iter().copied());
        }
        now
    }

    /// The bytes still owed to the log once the stream has ended.
    fn finish(&mut self) -> Vec<u8> {
        if self.tail.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.tail.len() + 64);
        if self.dropped > 0 {
            out.extend_from_slice(
                format!(
                    "\n[... {} bytes of console output dropped ...]\n",
                    self.dropped
                )
                .as_bytes(),
            );
        }
        out.extend(self.tail.drain(..));
        out
    }
}

struct Shared {
    file: File,
    retention: Retention,
}

/// Running copiers for a VMM's output streams.
pub struct ConsolePump {
    handles: Vec<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
}

/// Copy each stream into the console log at `path`. The copiers end when the
/// streams close, which happens when the VMM exits; call
/// [`ConsolePump::finish`] after that.
pub fn pump(streams: Vec<Box<dyn Read + Send>>, path: &Path) -> std::io::Result<ConsolePump> {
    pump_with(streams, path, HEAD_LIMIT, TAIL_LIMIT)
}

fn pump_with(
    streams: Vec<Box<dyn Read + Send>>,
    path: &Path,
    head_limit: u64,
    tail_limit: usize,
) -> std::io::Result<ConsolePump> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let shared = Arc::new(Mutex::new(Shared {
        file,
        retention: Retention::new(head_limit, tail_limit),
    }));
    let handles = streams
        .into_iter()
        .map(|mut stream| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut guard) = shared.lock() {
                        let Shared { file, retention } = &mut *guard;
                        let now = retention.push(&buf[..n]);
                        // Flushed each time: a partial line must survive the
                        // VMM being killed.
                        let _ = file.write_all(now).and_then(|()| file.flush());
                    }
                }
            })
        })
        .collect();
    Ok(ConsolePump { handles, shared })
}

impl ConsolePump {
    /// Wait for the copiers, then append the retained tail.
    pub fn finish(self) {
        for handle in self.handles {
            let _ = handle.join();
        }
        if let Ok(mut guard) = self.shared.lock() {
            let tail = guard.retention.finish();
            let _ = guard
                .file
                .write_all(&tail)
                .and_then(|()| guard.file.flush());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(retention: &mut Retention, chunks: &[&[u8]]) -> Vec<u8> {
        let mut log = Vec::new();
        for chunk in chunks {
            log.extend_from_slice(retention.push(chunk));
        }
        log.extend(retention.finish());
        log
    }

    #[test]
    fn output_under_the_head_budget_is_kept_whole() {
        let mut r = Retention::new(100, 10);
        assert_eq!(collect(&mut r, &[b"boot ", b"ok\n"]), b"boot ok\n");
        assert_eq!(r.dropped, 0);
    }

    #[test]
    fn the_head_and_the_tail_survive_a_flood() {
        let mut r = Retention::new(4, 6);
        let log = collect(&mut r, &[b"HEAD", b"aaaaaaaa", b"bbbbbbbb", b"TAIL42"]);
        let text = String::from_utf8(log).unwrap();
        assert!(text.starts_with("HEAD"), "{text}");
        assert!(text.ends_with("TAIL42"), "{text}");
        assert!(text.contains("bytes of console output dropped"), "{text}");
        assert_eq!(r.dropped, 16);
    }

    #[test]
    fn a_chunk_straddling_the_head_boundary_is_split() {
        let mut r = Retention::new(3, 100);
        let log = collect(&mut r, &[b"abcdef"]);
        assert_eq!(log, b"abcdef", "nothing dropped while the tail has room");
        assert_eq!(r.head_written, 3);
    }

    #[test]
    fn pump_writes_head_immediately_and_the_tail_on_finish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let data: Vec<u8> = (0..40u8).collect();
        let streams: Vec<Box<dyn Read + Send>> = vec![Box::new(std::io::Cursor::new(data.clone()))];
        let pump = pump_with(streams, &path, 10, 5).unwrap();
        pump.finish();
        let log = std::fs::read(&path).unwrap();
        assert_eq!(&log[..10], &data[..10]);
        assert_eq!(&log[log.len() - 5..], &data[35..]);
        assert!(String::from_utf8_lossy(&log).contains("dropped"));
    }
}
