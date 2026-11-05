// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::process::ExitCode;

use openshell_firecracker_launcher::cli::{Request, classify};
use openshell_firecracker_launcher::{check, confine, delegate, launcher};

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().collect();

    // The jail shim must run before any thread exists, which
    // `unshare(CLONE_NEWUSER)` requires.
    if let Some(code) = confine::shim(&argv) {
        return exit_code(code);
    }

    match classify(&argv) {
        Ok(Request::Check) => match std::env::current_exe() {
            Ok(me) => exit_code(check::run(&me)),
            Err(error) => fail(&format!("locate this binary: {error}")),
        },
        Ok(Request::Delegate) => fail(&delegate::exec_driver(&argv)),
        Ok(Request::RunVm(args)) => match launcher::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error),
        },
        Err(error) => fail(&error),
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("openshell-firecracker-launcher: {message}");
    ExitCode::FAILURE
}
