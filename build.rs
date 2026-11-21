#[path = "src/comment_guard.rs"]
mod comment_guard;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let directories = [Path::new("src"), Path::new("tests")];
    let extra_files = [Path::new("build.rs")];

    let report = match comment_guard::scan_roots(&directories, &extra_files) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("error: comment guard failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    for path in &report.watched_paths {
        println!("cargo::rerun-if-changed={}", path.display());
    }

    if report.violations.is_empty() {
        return ExitCode::SUCCESS;
    }

    for entry in &report.violations {
        eprintln!(
            "error: forbidden {} at {}:{}:{}",
            entry.violation.kind.label(),
            entry.path.display(),
            entry.violation.line,
            entry.violation.column
        );
    }

    eprintln!(
        "hint: comments are forbidden in project Rust sources; express intent through clear names and structure"
    );

    ExitCode::FAILURE
}
