//! Development entrypoint for the currently implemented bootstrap checks.

use std::env;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let Some((command, remaining)) = args.split_first() else {
        eprintln!("usage: cargo xtask <check|policy|sandbox|task|context|worktree|evidence|review|help>");
        return ExitCode::FAILURE;
    };
    let forwarded = matches!(
        command.as_str(),
        "sandbox" | "task" | "context" | "worktree" | "evidence" | "review"
    );
    if !forwarded && !remaining.is_empty() {
        eprintln!("unexpected arguments for {command}");
        return ExitCode::FAILURE;
    }
    let script = match command.as_str() {
        "check" => "tools/check.py",
        "policy" => "tools/workspace.py",
        "sandbox" => "tools/sandbox.py",
        "context" => "tools/context.py",
        "review" => "tools/review.py",
        "task" | "worktree" | "evidence" => "tools/task.py",
        "help" => {
            println!(
                "check: workspace checks; policy: dependencies; sandbox: resource caps; task/context/worktree/evidence: owned task lifecycle; review: independent evidence gate; see docs/plan/development.md"
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
    } else if matches!(command.as_str(), "sandbox" | "review") {
        child.args(remaining);
    } else if forwarded {
        child.arg(command).args(remaining);
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
