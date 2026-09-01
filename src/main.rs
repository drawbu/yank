//! The `yank` binary. Everything lives in the library; see `lib.rs`.

use std::process::ExitCode;

fn main() -> ExitCode {
    color_eyre::install().expect("color_eyre installs once");

    match yank::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            yank::cli::report_error(&err);
            ExitCode::FAILURE
        }
    }
}
