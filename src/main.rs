#![cfg_attr(
    test,
    expect(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        reason = "tests use deliberate fixture indexing and panic assertions"
    )
)]

use std::process::ExitCode;

mod cli;
mod compiler;
mod protected_fs;

#[cfg(not(target_os = "linux"))]
compile_error!("canonforge requires Linux openat2 and renameat2 support");

fn escaped_error(error: &anyhow::Error) -> String {
    format!("{error:#}").escape_debug().to_string()
}

fn main() -> ExitCode {
    if let Err(error) = cli::run() {
        eprintln!("error: {}", escaped_error(&error));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_escape_terminal_controls() {
        let rendered = escaped_error(&anyhow::anyhow!("fictional\u{1b}]52;unsafe\u{7}"));
        assert_eq!(rendered, r"fictional\u{1b}]52;unsafe\u{7}");
    }
}
