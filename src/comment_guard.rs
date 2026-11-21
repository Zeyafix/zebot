use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    Line,
    LineDoc,
    Block,
    BlockDoc,
}

impl CommentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Line => "line comment",
            Self::LineDoc => "doc comment",
            Self::Block => "block comment",
            Self::BlockDoc => "block doc comment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentViolation {
    pub line: usize,
    pub column: usize,
    pub kind: CommentKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileViolation {
    pub path: PathBuf,
    pub violation: CommentViolation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub watched_paths: Vec<PathBuf>,
    pub violations: Vec<FileViolation>,
}

#[derive(Debug)]
pub struct ScanError {
    pub path: PathBuf,
    pub source: io::Error,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to read {}: {}", self.path.display(), self.source)
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

pub fn scan_roots(directories: &[&Path], extra_files: &[&Path]) -> Result<ScanReport, ScanError> {
    let mut watched_paths = Vec::new();
    let mut violations = Vec::new();

    for directory in directories {
        watched_paths.push((*directory).to_path_buf());
        if directory.is_dir() {
            scan_directory(directory, &mut watched_paths, &mut violations)?;
        }
    }

    for file in extra_files {
        watched_paths.push((*file).to_path_buf());
        if file.is_file() {
            scan_file(file, &mut violations)?;
        }
    }

    Ok(ScanReport {
        watched_paths,
        violations,
    })
}

fn scan_directory(
    dir: &Path,
    watched_paths: &mut Vec<PathBuf>,
    violations: &mut Vec<FileViolation>,
) -> Result<(), ScanError> {
    let entries = fs::read_dir(dir).map_err(|source| ScanError {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ScanError {
            path: dir.to_path_buf(),
            source,
        })?;
        paths.push(entry.path());
    }
    paths.sort();

    for path in paths {
        if path.is_dir() {
            watched_paths.push(path.clone());
            scan_directory(&path, watched_paths, violations)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            watched_paths.push(path.clone());
            scan_file(&path, violations)?;
        }
    }

    Ok(())
}

fn scan_file(path: &Path, violations: &mut Vec<FileViolation>) -> Result<(), ScanError> {
    let source = fs::read_to_string(path).map_err(|source| ScanError {
        path: path.to_path_buf(),
        source,
    })?;

    for violation in find_comments(&source) {
        violations.push(FileViolation {
            path: path.to_path_buf(),
            violation,
        });
    }

    Ok(())
}

pub fn find_comments(source: &str) -> Vec<CommentViolation> {
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0;
    let mut line = 1;
    let mut column = 1;
    let mut prev_was_ident_continue = false;
    let mut violations = Vec::new();

    while let Some(&ch) = chars.get(index) {
        if !prev_was_ident_continue
            && let Some((prefix_len, hashes)) = raw_string_prefix(&chars, index)
        {
            for _ in 0..prefix_len {
                let _ = advance(&chars, &mut index, &mut line, &mut column);
            }
            consume_raw_string(&chars, &mut index, &mut line, &mut column, hashes);
            prev_was_ident_continue = false;
            continue;
        }

        if ch == '"' {
            let _ = advance(&chars, &mut index, &mut line, &mut column);
            consume_quoted_string(&chars, &mut index, &mut line, &mut column);
            prev_was_ident_continue = false;
            continue;
        }

        if ch == '\'' {
            consume_char_or_lifetime(&chars, &mut index, &mut line, &mut column);
            prev_was_ident_continue = false;
            continue;
        }

        if ch == '/' && chars.get(index + 1) == Some(&'/') {
            let kind = classify_line_comment(&chars, index);
            violations.push(CommentViolation { line, column, kind });
            consume_line_comment(&chars, &mut index, &mut line, &mut column);
            prev_was_ident_continue = false;
            continue;
        }

        if ch == '/' && chars.get(index + 1) == Some(&'*') {
            let kind = classify_block_comment(&chars, index);
            violations.push(CommentViolation { line, column, kind });
            consume_block_comment(&chars, &mut index, &mut line, &mut column);
            prev_was_ident_continue = false;
            continue;
        }

        let _ = advance(&chars, &mut index, &mut line, &mut column);
        prev_was_ident_continue = is_ident_continue(ch);
    }

    violations
}

fn advance(
    chars: &[char],
    index: &mut usize,
    line: &mut usize,
    column: &mut usize,
) -> Option<char> {
    let ch = *chars.get(*index)?;
    *index += 1;
    if ch == '\n' {
        *line += 1;
        *column = 1;
    } else {
        *column += 1;
    }
    Some(ch)
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn raw_string_prefix(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let after_prefix = match chars.get(start) {
        Some('r') => start + 1,
        Some('b' | 'c') if chars.get(start + 1) == Some(&'r') => start + 2,
        _ => return None,
    };

    let mut hashes = 0;
    while chars.get(after_prefix + hashes) == Some(&'#') {
        hashes += 1;
    }

    if chars.get(after_prefix + hashes) == Some(&'"') {
        Some(((after_prefix - start) + hashes + 1, hashes))
    } else {
        None
    }
}

fn has_following_hashes(chars: &[char], start: usize, hashes: usize) -> bool {
    for offset in 0..hashes {
        if chars.get(start + offset) != Some(&'#') {
            return false;
        }
    }
    true
}

fn consume_raw_string(
    chars: &[char],
    index: &mut usize,
    line: &mut usize,
    column: &mut usize,
    hashes: usize,
) {
    while let Some(ch) = advance(chars, index, line, column) {
        if ch == '"' && has_following_hashes(chars, *index, hashes) {
            for _ in 0..hashes {
                let _ = advance(chars, index, line, column);
            }
            break;
        }
    }
}

fn consume_quoted_string(chars: &[char], index: &mut usize, line: &mut usize, column: &mut usize) {
    while let Some(ch) = advance(chars, index, line, column) {
        if ch == '\\' {
            let _ = advance(chars, index, line, column);
        } else if ch == '"' {
            break;
        }
    }
}

fn consume_char_or_lifetime(
    chars: &[char],
    index: &mut usize,
    line: &mut usize,
    column: &mut usize,
) {
    match chars.get(*index + 1) {
        Some(&'\\') => {
            let _ = advance(chars, index, line, column);
            let _ = advance(chars, index, line, column);
            let _ = advance(chars, index, line, column);
            while let Some(&next) = chars.get(*index) {
                if next == '\n' || next == '\r' {
                    break;
                }
                let _ = advance(chars, index, line, column);
                if next == '\'' {
                    break;
                }
            }
        }
        Some(&next)
            if next != '\''
                && next != '\n'
                && next != '\r'
                && chars.get(*index + 2) == Some(&'\'') =>
        {
            let _ = advance(chars, index, line, column);
            let _ = advance(chars, index, line, column);
            let _ = advance(chars, index, line, column);
        }
        _ => {
            let _ = advance(chars, index, line, column);
        }
    }
}

fn classify_line_comment(chars: &[char], start: usize) -> CommentKind {
    match chars.get(start + 2) {
        Some(&'!') => CommentKind::LineDoc,
        Some(&'/') if chars.get(start + 3) != Some(&'/') => CommentKind::LineDoc,
        _ => CommentKind::Line,
    }
}

fn classify_block_comment(chars: &[char], start: usize) -> CommentKind {
    match chars.get(start + 2) {
        Some(&'!') => CommentKind::BlockDoc,
        Some(&'*') if !matches!(chars.get(start + 3), Some(&'*' | &'/')) => CommentKind::BlockDoc,
        _ => CommentKind::Block,
    }
}

fn consume_line_comment(chars: &[char], index: &mut usize, line: &mut usize, column: &mut usize) {
    while let Some(ch) = advance(chars, index, line, column) {
        if ch == '\n' {
            break;
        }
    }
}

fn consume_block_comment(chars: &[char], index: &mut usize, line: &mut usize, column: &mut usize) {
    let _ = advance(chars, index, line, column);
    let _ = advance(chars, index, line, column);
    let mut depth: usize = 1;

    while depth > 0 {
        if chars.get(*index) == Some(&'/') && chars.get(*index + 1) == Some(&'*') {
            let _ = advance(chars, index, line, column);
            let _ = advance(chars, index, line, column);
            depth += 1;
        } else if chars.get(*index) == Some(&'*') && chars.get(*index + 1) == Some(&'/') {
            let _ = advance(chars, index, line, column);
            let _ = advance(chars, index, line, column);
            depth -= 1;
        } else if advance(chars, index, line, column).is_none() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CommentKind, CommentViolation, find_comments};

    #[test]
    fn detects_line_comments() {
        let source = "fn main() {\n    let x = 1; // forbidden\n}\n";
        assert_eq!(
            find_comments(source),
            vec![CommentViolation {
                line: 2,
                column: 16,
                kind: CommentKind::Line,
            }]
        );
    }

    #[test]
    fn detects_outer_and_inner_line_doc_comments() {
        let source = "//! module doc\n/// item doc\nfn demo() {}\n";
        assert_eq!(
            find_comments(source),
            vec![
                CommentViolation {
                    line: 1,
                    column: 1,
                    kind: CommentKind::LineDoc,
                },
                CommentViolation {
                    line: 2,
                    column: 1,
                    kind: CommentKind::LineDoc,
                },
            ]
        );
    }

    #[test]
    fn detects_block_and_nested_block_comments() {
        let source = "let a = /* outer /* inner */ still outer */ 42;\nlet b = /** doc */ 7;\nlet c = /*! inner */ 9;\n";
        assert_eq!(
            find_comments(source),
            vec![
                CommentViolation {
                    line: 1,
                    column: 9,
                    kind: CommentKind::Block,
                },
                CommentViolation {
                    line: 2,
                    column: 9,
                    kind: CommentKind::BlockDoc,
                },
                CommentViolation {
                    line: 3,
                    column: 9,
                    kind: CommentKind::BlockDoc,
                },
            ]
        );
    }

    #[test]
    fn ignores_urls_and_comment_tokens_inside_regular_strings() {
        let source = r#"
            const YOUTUBE: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
            const BLOCK: &str = "/* not a comment */ /// also not a comment";
            const BYTE_STR: &[u8] = b"https://youtu.be/abc123 /* still string */";
        "#;
        assert!(find_comments(source).is_empty());
    }

    #[test]
    fn handles_escaped_quotes_and_backslashes_in_strings() {
        let clean = "let a = \"escaped \\\" // not a comment \\\\\\\" /* nope */\";\n";
        assert!(find_comments(clean).is_empty());

        let with_comment = "let a = \"trailing backslash \\\\\"; // real comment\n";
        assert_eq!(
            find_comments(with_comment),
            vec![CommentViolation {
                line: 1,
                column: 34,
                kind: CommentKind::Line,
            }]
        );
    }

    #[test]
    fn handles_raw_strings_with_varying_hash_counts() {
        let clean = concat!(
            "let a = r\"https://youtube.com /* not a comment */\";\n",
            "let b = r#\"contains \" quote and // and /* block */\"#;\n",
            "let c = r##\"contains \"# inside and // still raw\"##;\n",
            "let d = br#\"https://youtu.be/xyz\"#;\n",
            "let e = cr#\"/* c-string */\"#;\n",
        );
        assert!(find_comments(clean).is_empty());

        let with_comment = "let a = r#\"https://youtube.com\"#; // after raw string\n";
        assert_eq!(
            find_comments(with_comment),
            vec![CommentViolation {
                line: 1,
                column: 35,
                kind: CommentKind::Line,
            }]
        );
    }

    #[test]
    fn distinguishes_raw_identifiers_from_raw_strings() {
        let source = "let r#type = 1; // comment after raw ident\n";
        assert_eq!(
            find_comments(source),
            vec![CommentViolation {
                line: 1,
                column: 17,
                kind: CommentKind::Line,
            }]
        );
    }

    #[test]
    fn handles_char_literals_and_lifetimes_accurately() {
        let source = concat!(
            "fn borrow<'a, 'b>(s: &'a str, t: &'b str) -> char {\n",
            "    let quote = '\"';\n",
            "    let escaped_quote = '\\'';\n",
            "    let escaped_slash = '\\\\';\n",
            "    let byte_quote = b'\"';\n",
            "    let hex_quote = '\\x22';\n",
            "    let unicode_quote = '\\u{0022}';\n",
            "    let _ = (s, t, quote, escaped_quote, escaped_slash, byte_quote, hex_quote, unicode_quote);\n",
            "    '/'\n",
            "} // trailing comment\n",
        );
        assert_eq!(
            find_comments(source),
            vec![CommentViolation {
                line: 10,
                column: 3,
                kind: CommentKind::Line,
            }]
        );
    }

    #[test]
    fn ignores_division_operators_and_identifier_suffixes() {
        let source = "let bar = \"https://youtube.com\";\nlet half = 10 / 2;\n";
        assert!(find_comments(source).is_empty());
    }
}
