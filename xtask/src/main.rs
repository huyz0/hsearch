//! Development entrypoint for the currently implemented bootstrap checks.

use std::env;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let [command] = args.as_slice() else {
        eprintln!("usage: cargo xtask <check|policy|help>");
        return ExitCode::FAILURE;
    };
    let script = match command.as_str() {
        "check" => "tools/check.py",
        "policy" => "tools/workspace.py",
        "help" => {
            println!(
                "check: bootstrap checks; policy: dependency boundaries; see docs/plan/development.md"
            );
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("unknown command: {command}");
            return ExitCode::FAILURE;
        }
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let python = env::var_os("PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let mut child = Command::new(python);
    child.current_dir(root).arg(script);
    if command == "policy" {
        child.arg("check");
    }
    match child.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => {
            eprintln!("{script} failed with {status}");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("could not run {script}: {error}");
            ExitCode::FAILURE
        }
    }
}
