use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use zebot::comment_guard::{CommentKind, scan_roots};

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn create(label: &str) -> io::Result<Self> {
        let id = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from("temp").join(format!("zebot-test-{label}-{}-{id}", process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn project_sources_contain_no_comments() {
    let report = scan_roots(
        &[Path::new("src"), Path::new("tests")],
        &[Path::new("build.rs")],
    )
    .expect("scan repository rust sources");

    assert!(
        report.violations.is_empty(),
        "unexpected comment violations: {:?}",
        report.violations
    );
    assert!(!report.watched_paths.is_empty());
}

#[test]
fn scanner_flags_comments_and_allows_urls_in_rust_files() {
    let temp = TempTestDir::create("comment-scan").expect("create temp test directory");
    let nested = temp.path().join("nested");
    fs::create_dir_all(&nested).expect("create nested temp directory");

    let clean_file = temp.path().join("clean.rs");
    fs::write(
        &clean_file,
        "pub const URL: &str = \"https://www.youtube.com/watch?v=abc123\";\n",
    )
    .expect("write clean rust fixture");

    let bad_file = nested.join("with_comment.rs");
    fs::write(&bad_file, "pub fn answer() -> u32 {\n    42 // nope\n}\n")
        .expect("write comment rust fixture");

    let report = scan_roots(&[temp.path()], &[]).expect("scan fixture directory");

    assert_eq!(report.violations.len(), 1);
    assert_eq!(report.violations[0].path, bad_file);
    assert_eq!(report.violations[0].violation.line, 2);
    assert_eq!(report.violations[0].violation.column, 8);
    assert_eq!(report.violations[0].violation.kind, CommentKind::Line);
}
