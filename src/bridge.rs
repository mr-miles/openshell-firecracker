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

use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

/// Relays running at once. The supervisor admits far fewer; this only bounds
/// what a misbehaving guest can make the launcher hold.
const MAX_RELAYS: usize = 64;

/// Accept connections on `listener` and relay each to the socket at `target`.
/// The thread runs until the process exits.
pub fn serve(listener: UnixListener, target: PathBuf) -> JoinHandle<()> {
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(guest) = stream else { continue };
            if active.fetch_add(1, Ordering::AcqRel) >= MAX_RELAYS {
                active.fetch_sub(1, Ordering::AcqRel);
                continue; // dropped: the guest sees the connection close
            }
            let (target, active) = (target.clone(), active.clone());
            std::thread::spawn(move || {
                relay(guest, &target);
                active.fetch_sub(1, Ordering::AcqRel);
            });
        }
    })
}

/// Copy bytes both ways between `guest` and the supervisor at `target` until
/// either side closes. A supervisor that is not listening yet drops the
/// connection; the guest dials again.
fn relay(guest: UnixStream, target: &Path) {
    let Ok(host) = UnixStream::connect(target) else {
        return;
    };
    let (Ok(mut guest_reader), Ok(mut host_writer)) = (guest.try_clone(), host.try_clone()) else {
        return;
    };
    let upstream = std::thread::spawn(move || {
        let _ = io::copy(&mut guest_reader, &mut host_writer);
        let _ = host_writer.shutdown(Shutdown::Write);
    });
    let (mut host_reader, mut guest_writer) = (host, guest);
    let _ = io::copy(&mut host_reader, &mut guest_writer);
    let _ = guest_writer.shutdown(Shutdown::Write);
    let _ = upstream.join();
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
}
