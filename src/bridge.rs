// SPDX-License-Identifier: Apache-2.0

//! Bridge from Firecracker's vsock socket to the host supervisor.
//!
//! The driver tells its launcher where the supervisor listens
//! (`--vm-vsock-control-socket`) and expects the guest's connections to host
//! port `--vm-vsock-control-port` to arrive there. Firecracker connects to `<uds_path>_<port>` for each such
//! connection. The launcher listens on that path and relays every connection
//! to the supervisor.
//!
//! This also keeps the supervisor's socket out of the VMM jail: the VMM sees
//! only the bridge socket, and the relay runs outside the jail as the driver's
//! user. The supervisor still authenticates the guest with TLS and bearer
//! tokens; the bridge carries bytes and trusts nothing.
//!
//! When a sandbox loses its supervisor connection, the guest and the
//! supervisor each see only that their peer went away. The bridge sits between
//! them, so it logs to stderr, with host timestamps, when each relay opens and
//! how and when each direction ends. These lines show which side closed first.
//! The driver passes the launcher's stderr through to its own.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Relays running at once. The supervisor admits far fewer; this only bounds
/// what a misbehaving guest can make the launcher hold.
const MAX_RELAYS: usize = 64;

/// Bytes moved per read in each direction.
const RELAY_BUFFER: usize = 64 * 1024;

/// State shared by the accept loop and every relay of one bridge.
struct Bridge {
    target: PathBuf,
    active: AtomicUsize,
    next_id: AtomicU64,
    /// A refusal at [`MAX_RELAYS`] was logged; cleared when a relay is
    /// admitted again, so a saturated bridge logs once, not per connection.
    saturated: AtomicBool,
    /// A failed connect to the supervisor was logged; cleared on the next
    /// successful connect. The guest redials quickly while the supervisor is
    /// not listening yet, so each failure is not logged.
    unreachable: AtomicBool,
}

/// Accept connections on `listener` and relay each to the socket at `target`.
/// The thread runs until the process exits.
pub fn serve(listener: UnixListener, target: PathBuf) -> JoinHandle<()> {
    let bridge = Arc::new(Bridge {
        target,
        active: AtomicUsize::new(0),
        next_id: AtomicU64::new(1),
        saturated: AtomicBool::new(false),
        unreachable: AtomicBool::new(false),
    });
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(guest) = stream else { continue };
            let id = bridge.next_id.fetch_add(1, Ordering::Relaxed);
            if bridge.active.fetch_add(1, Ordering::AcqRel) >= MAX_RELAYS {
                bridge.active.fetch_sub(1, Ordering::AcqRel);
                if !bridge.saturated.swap(true, Ordering::Relaxed) {
                    log(format_args!(
                        "vsock relay {id}: refused, {MAX_RELAYS} relays to {} are already open; \
                         further refusals are not logged until a relay is admitted",
                        bridge.target.display()
                    ));
                }
                continue; // dropped: the guest sees the connection close
            }
            bridge.saturated.store(false, Ordering::Relaxed);
            let bridge = bridge.clone();
            std::thread::spawn(move || {
                relay(id, guest, &bridge);
                bridge.active.fetch_sub(1, Ordering::AcqRel);
            });
        }
    })
}

/// Copy bytes both ways between `guest` and the supervisor until both
/// directions end. A supervisor that is not listening yet drops the
/// connection; the guest dials again.
fn relay(id: u64, guest: UnixStream, bridge: &Bridge) {
    let opened = Instant::now();
    let host = match UnixStream::connect(&bridge.target) {
        Ok(host) => host,
        Err(error) => {
            if !bridge.unreachable.swap(true, Ordering::Relaxed) {
                log(format_args!(
                    "vsock relay {id}: connect to the supervisor at {} failed: {error}; \
                     dropping the guest connection \
                     (repeats are not logged until a connect succeeds)",
                    bridge.target.display()
                ));
            }
            return;
        }
    };
    if bridge.unreachable.swap(false, Ordering::Relaxed) {
        log(format_args!(
            "vsock relay {id}: the supervisor at {} accepts connections again",
            bridge.target.display()
        ));
    }
    let (mut guest_reader, mut host_writer) = match (guest.try_clone(), host.try_clone()) {
        (Ok(guest_reader), Ok(host_writer)) => (guest_reader, host_writer),
        (Err(error), _) | (_, Err(error)) => {
            log(format_args!(
                "vsock relay {id}: duplicate socket handles: {error}; dropping the connection"
            ));
            return;
        }
    };
    log(format_args!(
        "vsock relay {id}: opened to {}",
        bridge.target.display()
    ));
    let upstream = std::thread::spawn(move || {
        let (bytes, end) = pump(&mut guest_reader, &mut host_writer);
        log_end(
            id,
            Direction::GuestToSupervisor,
            opened.elapsed(),
            bytes,
            &end,
        );
        let _ = host_writer.shutdown(Shutdown::Write);
    });
    let (mut host_reader, mut guest_writer) = (host, guest);
    let (bytes, end) = pump(&mut host_reader, &mut guest_writer);
    log_end(
        id,
        Direction::SupervisorToGuest,
        opened.elapsed(),
        bytes,
        &end,
    );
    let _ = guest_writer.shutdown(Shutdown::Write);
    let _ = upstream.join();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    GuestToSupervisor,
    SupervisorToGuest,
}

impl Direction {
    fn source(self) -> &'static str {
        match self {
            Self::GuestToSupervisor => "the guest",
            Self::SupervisorToGuest => "the supervisor",
        }
    }

    fn sink(self) -> &'static str {
        match self {
            Self::GuestToSupervisor => "the supervisor",
            Self::SupervisorToGuest => "the guest",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} -> {}", self.source(), self.sink())
    }
}

/// How one direction of a relay stopped.
#[derive(Debug)]
enum End {
    /// The source closed its side cleanly.
    Eof,
    /// Reading from the source failed.
    Read(io::Error),
    /// Writing to the sink failed.
    Write(io::Error),
}

/// Copy from `from` to `to` until `from` ends or either side fails. Returns
/// the bytes delivered to `to` and how the copy ended. Unlike `io::copy`,
/// this keeps the count on error and says which side failed.
fn pump(from: &mut impl Read, to: &mut impl Write) -> (u64, End) {
    let mut buffer = vec![0_u8; RELAY_BUFFER];
    let mut delivered = 0_u64;
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return (delivered, End::Eof),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return (delivered, End::Read(error)),
        };
        if let Err(error) = to.write_all(&buffer[..read]) {
            return (delivered, End::Write(error));
        }
        delivered += read as u64;
    }
}

fn describe_end(direction: Direction, end: &End) -> String {
    match end {
        End::Eof => format!("{} closed its side", direction.source()),
        End::Read(error) => format!("reading from {} failed: {error}", direction.source()),
        End::Write(error) => format!("writing to {} failed: {error}", direction.sink()),
    }
}

fn log_end(id: u64, direction: Direction, open_for: Duration, bytes: u64, end: &End) {
    log(format_args!(
        "vsock relay {id}: {direction} ended after {:.3}s and {bytes} bytes: {}",
        open_for.as_secs_f64(),
        describe_end(direction, end)
    ));
}

fn log(message: fmt::Arguments<'_>) {
    eprintln!(
        "{} openshell-firecracker-launcher: {message}",
        utc_timestamp(SystemTime::now())
    );
}

/// `time` as RFC 3339 UTC with milliseconds, the format OpenShell logs use,
/// so bridge lines line up with the supervisor's and the guest's.
fn utc_timestamp(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60,
        since_epoch.subsec_millis()
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, for non-negative days).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468; // days since 0000-03-01
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153; // March = 0
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn bytes_cross_the_bridge_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("supervisor.sock");
        let bridge = dir.path().join("fc_5500");

        // A supervisor stand-in that upper-cases what it receives.
        let supervisor = UnixListener::bind(&target).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = supervisor.accept().unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).unwrap();
            stream.write_all(&buf.to_ascii_uppercase()).unwrap();
        });

        serve(UnixListener::bind(&bridge).unwrap(), target);

        let mut guest = UnixStream::connect(&bridge).unwrap();
        guest.write_all(b"hello").unwrap();
        let mut reply = [0u8; 5];
        guest.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"HELLO");
    }

    #[test]
    fn a_missing_supervisor_closes_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = dir.path().join("fc_5500");
        serve(
            UnixListener::bind(&bridge).unwrap(),
            dir.path().join("absent.sock"),
        );

        let mut guest = UnixStream::connect(&bridge).unwrap();
        let mut buf = [0u8; 1];
        // The bridge drops the connection: EOF (or reset), not a hang.
        match guest.read(&mut buf) {
            Ok(0) | Err(_) => {}
            Ok(n) => panic!("unexpected {n} bytes"),
        }
    }

    #[test]
    fn a_supervisor_close_reaches_the_guest() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("supervisor.sock");
        let bridge = dir.path().join("fc_5500");

        // A supervisor stand-in that closes as soon as it accepts.
        let supervisor = UnixListener::bind(&target).unwrap();
        std::thread::spawn(move || drop(supervisor.accept().unwrap()));

        serve(UnixListener::bind(&bridge).unwrap(), target);

        let mut guest = UnixStream::connect(&bridge).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(guest.read(&mut buf).unwrap(), 0, "EOF, not a hang");
    }

    #[test]
    fn a_clean_close_is_an_eof_with_the_bytes_delivered() {
        let mut sink = Vec::new();
        let (bytes, end) = pump(&mut &b"twelve bytes"[..], &mut sink);
        assert_eq!(bytes, 12);
        assert!(matches!(end, End::Eof), "{end:?}");
        assert_eq!(sink, b"twelve bytes");
    }

    struct Failing(io::ErrorKind);

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
    }

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failure_names_the_side_that_failed() {
        let (bytes, end) = pump(
            &mut Failing(io::ErrorKind::ConnectionReset),
            &mut Vec::new(),
        );
        assert_eq!(bytes, 0);
        assert!(
            matches!(&end, End::Read(error) if error.kind() == io::ErrorKind::ConnectionReset),
            "{end:?}"
        );
        assert_eq!(
            describe_end(Direction::GuestToSupervisor, &end),
            "reading from the guest failed: connection reset"
        );

        let (bytes, end) = pump(&mut &b"data"[..], &mut Failing(io::ErrorKind::BrokenPipe));
        assert_eq!(bytes, 0, "nothing was delivered");
        assert!(matches!(end, End::Write(_)), "{end:?}");
        assert_eq!(
            describe_end(Direction::GuestToSupervisor, &end),
            "writing to the supervisor failed: broken pipe"
        );
        assert_eq!(
            describe_end(Direction::SupervisorToGuest, &End::Eof),
            "the supervisor closed its side"
        );
    }

    #[test]
    fn interrupted_reads_are_retried() {
        struct InterruptOnce(bool, &'static [u8]);
        impl Read for InterruptOnce {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.1.read(buf)
            }
        }
        let mut sink = Vec::new();
        let (bytes, end) = pump(&mut InterruptOnce(false, b"abc"), &mut sink);
        assert_eq!((bytes, sink.as_slice()), (3, &b"abc"[..]));
        assert!(matches!(end, End::Eof), "{end:?}");
    }

    #[test]
    fn timestamps_are_rfc3339_utc_with_milliseconds() {
        let at = |millis: u64| utc_timestamp(UNIX_EPOCH + Duration::from_millis(millis));
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(at(951_782_400_000), "2000-02-29T00:00:00.000Z", "leap day");
        assert_eq!(at(1_798_761_599_999), "2026-12-31T23:59:59.999Z");
        assert_eq!(at(1_803_859_200_000), "2027-03-01T00:00:00.000Z");
        assert_eq!(at(4_107_542_399_000), "2100-02-28T23:59:59.000Z");
        assert_eq!(
            at(4_107_542_400_000),
            "2100-03-01T00:00:00.000Z",
            "2100 is not leap"
        );
    }
}
