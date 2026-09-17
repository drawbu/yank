//! The `yank` binary. Everything lives in the library; see `lib.rs`.

use std::process::ExitCode;

fn main() -> ExitCode {
    match yank::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            yank::cli::report_error(&err);
            ExitCode::FAILURE
        }
    }
}
