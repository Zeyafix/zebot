use std::error::Error;
use std::fmt;
use std::io;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersions {
    pub yt_dlp: String,
    pub ffmpeg: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutput {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFailureReason {
    NotFound,
    SpawnFailed(String),
    NonZeroExit { code: Option<i32>, stderr: String },
    EmptyVersionOutput,
}

impl fmt::Display for ToolFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "executable not found on PATH"),
            Self::SpawnFailed(message) => write!(f, "failed to launch executable: {message}"),
            Self::NonZeroExit { code, stderr } => {
                let detail = first_non_empty_line(stderr).unwrap_or("no error output");
                match code {
                    Some(exit_code) => write!(f, "exited with status {exit_code} ({detail})"),
                    None => write!(f, "terminated by signal ({detail})"),
                }
            }
            Self::EmptyVersionOutput => write!(f, "produced empty version output"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    pub name: &'static str,
    pub program: String,
    pub reason: ToolFailureReason,
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ('{}'): {}", self.name, self.program, self.reason)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCheckError {
    pub failures: Vec<ToolFailure>,
}

impl fmt::Display for DependencyCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "required external audio tools are unavailable:")?;
        for failure in &self.failures {
            writeln!(f, "  - {failure}")?;
        }
        write!(
            f,
            "install the missing tools and ensure they are on PATH (see docs/quick-start.md)"
        )
    }
}

impl Error for DependencyCheckError {}

pub fn check_external_tools(
    yt_dlp_path: &str,
    ffmpeg_path: &str,
) -> Result<ToolVersions, DependencyCheckError> {
    check_external_tools_with(yt_dlp_path, ffmpeg_path, run_probe)
}

pub fn check_external_tools_with<F>(
    yt_dlp_path: &str,
    ffmpeg_path: &str,
    probe: F,
) -> Result<ToolVersions, DependencyCheckError>
where
    F: Fn(&str, &[&str]) -> Result<ProbeOutput, ToolFailureReason>,
{
    let mut failures = Vec::new();

    let yt_dlp = probe_single_tool("yt-dlp", yt_dlp_path, &["--version"], &probe, &mut failures);
    let ffmpeg = probe_single_tool("ffmpeg", ffmpeg_path, &["-version"], &probe, &mut failures);

    if !failures.is_empty() {
        return Err(DependencyCheckError { failures });
    }

    match (yt_dlp, ffmpeg) {
        (Some(yt_dlp), Some(ffmpeg)) => Ok(ToolVersions { yt_dlp, ffmpeg }),
        _ => Err(DependencyCheckError { failures }),
    }
}

fn probe_single_tool<F>(
    name: &'static str,
    program: &str,
    args: &[&str],
    probe: &F,
    failures: &mut Vec<ToolFailure>,
) -> Option<String>
where
    F: Fn(&str, &[&str]) -> Result<ProbeOutput, ToolFailureReason>,
{
    match probe(program, args) {
        Ok(output) => {
            if !output.success {
                failures.push(ToolFailure {
                    name,
                    program: program.to_string(),
                    reason: ToolFailureReason::NonZeroExit {
                        code: output.exit_code,
                        stderr: output.stderr,
                    },
                });
                return None;
            }

            match first_non_empty_line(&output.stdout) {
                Some(version_line) => Some(version_line.to_string()),
                None => {
                    failures.push(ToolFailure {
                        name,
                        program: program.to_string(),
                        reason: ToolFailureReason::EmptyVersionOutput,
                    });
                    None
                }
            }
        }
        Err(reason) => {
            failures.push(ToolFailure {
                name,
                program: program.to_string(),
                reason,
            });
            None
        }
    }
}

fn run_probe(program: &str, args: &[&str]) -> Result<ProbeOutput, ToolFailureReason> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ToolFailureReason::NotFound
            } else {
                ToolFailureReason::SpawnFailed(error.to_string())
            }
        })?;

    Ok(ProbeOutput {
        exit_code: output.status.code(),
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        ProbeOutput, ToolFailure, ToolFailureReason, ToolVersions, check_external_tools,
        check_external_tools_with,
    };

    #[test]
    fn returns_versions_when_both_tools_succeed() {
        let versions = check_external_tools_with("yt-dlp", "ffmpeg", |program, args| {
            if program == "yt-dlp" && args == ["--version"] {
                Ok(ProbeOutput {
                    exit_code: Some(0),
                    success: true,
                    stdout: "2026.10.12\n".to_string(),
                    stderr: String::new(),
                })
            } else if program == "ffmpeg" && args == ["-version"] {
                Ok(ProbeOutput {
                    exit_code: Some(0),
                    success: true,
                    stdout: "ffmpeg version 9.0.1\nbuilt with gcc\n".to_string(),
                    stderr: String::new(),
                })
            } else {
                Err(ToolFailureReason::NotFound)
            }
        })
        .unwrap();

        assert_eq!(
            versions,
            ToolVersions {
                yt_dlp: "2026.10.12".to_string(),
                ffmpeg: "ffmpeg version 9.0.1".to_string(),
            }
        );
    }

    #[test]
    fn reports_both_failures_when_both_tools_are_missing() {
        let error =
            check_external_tools_with("yt-dlp", "ffmpeg", |_, _| Err(ToolFailureReason::NotFound))
                .unwrap_err();

        assert_eq!(
            error.failures,
            vec![
                ToolFailure {
                    name: "yt-dlp",
                    program: "yt-dlp".to_string(),
                    reason: ToolFailureReason::NotFound,
                },
                ToolFailure {
                    name: "ffmpeg",
                    program: "ffmpeg".to_string(),
                    reason: ToolFailureReason::NotFound,
                },
            ]
        );
    }

    #[test]
    fn reports_non_zero_exit_and_empty_output() {
        let error = check_external_tools_with("yt-dlp", "ffmpeg", |program, _| {
            if program == "yt-dlp" {
                Ok(ProbeOutput {
                    exit_code: Some(2),
                    success: false,
                    stdout: String::new(),
                    stderr: "permission denied\n".to_string(),
                })
            } else {
                Ok(ProbeOutput {
                    exit_code: Some(0),
                    success: true,
                    stdout: "   \n".to_string(),
                    stderr: String::new(),
                })
            }
        })
        .unwrap_err();

        assert_eq!(
            error.failures,
            vec![
                ToolFailure {
                    name: "yt-dlp",
                    program: "yt-dlp".to_string(),
                    reason: ToolFailureReason::NonZeroExit {
                        code: Some(2),
                        stderr: "permission denied\n".to_string(),
                    },
                },
                ToolFailure {
                    name: "ffmpeg",
                    program: "ffmpeg".to_string(),
                    reason: ToolFailureReason::EmptyVersionOutput,
                },
            ]
        );
    }

    #[test]
    fn real_process_probe_detects_missing_executables() {
        let missing = "zebot-nonexistent-executable-0000";
        let error = check_external_tools(missing, missing).unwrap_err();
        assert_eq!(error.failures.len(), 2);
        assert_eq!(error.failures[0].reason, ToolFailureReason::NotFound);
        assert_eq!(error.failures[1].reason, ToolFailureReason::NotFound);
    }
}
