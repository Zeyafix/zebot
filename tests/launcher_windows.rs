#![cfg(windows)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static FIXTURE_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct LauncherFixture {
    root: PathBuf,
    caller_dir: PathBuf,
    launch_bat: PathBuf,
    exe_path: PathBuf,
    pid_file: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    child_pid_file: PathBuf,
}

impl LauncherFixture {
    fn create(label: &str) -> io::Result<Self> {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let workspace = std::env::current_dir()?;
        let base = workspace.join("temp").join(format!(
            "zebot-launcher-test-{label}-{}-{id}",
            std::process::id()
        ));
        let root = base.join("repo dir");
        let caller_dir = base.join("other cwd");
        let target_debug = root.join("target").join("debug");

        fs::create_dir_all(&target_debug)?;
        fs::create_dir_all(&caller_dir)?;

        let source_bat = workspace.join("launch.bat");
        let launch_bat = root.join("launch.bat");
        fs::copy(&source_bat, &launch_bat)?;

        let exe_path = target_debug.join("zebot.exe");
        let launcher_dir = root.join("temp").join("launcher");
        let pid_file = launcher_dir.join("zebot.pid");
        let stdout_log = launcher_dir.join("zebot.stdout.log");
        let stderr_log = launcher_dir.join("zebot.stderr.log");
        let child_pid_file = root.join("temp").join("child.pid");

        Ok(Self {
            root,
            caller_dir,
            launch_bat,
            exe_path,
            pid_file,
            stdout_log,
            stderr_log,
            child_pid_file,
        })
    }

    fn write_dotenv(&self) -> io::Result<()> {
        fs::write(
            self.root.join(".env"),
            "DISCORD_TOKEN=test-token\r\nDISCORD_GUILD_ID=123\r\nALLOWED_CHANNEL_ID=456\r\nAPI_TOKEN=api-token\r\n",
        )
    }

    fn compile_fake_zebot(&self, rust_source: &str) -> io::Result<Output> {
        let src_path = self.root.join("fake_zebot.rs");
        fs::write(&src_path, rust_source)?;

        Command::new("rustc")
            .arg("--edition=2024")
            .arg(&src_path)
            .arg("-o")
            .arg(&self.exe_path)
            .output()
    }

    fn run_launcher(&self, args: &[&str], cwd: &Path) -> io::Result<Output> {
        let mut command = Command::new("cmd.exe");
        command.arg("/d").arg("/c").arg(&self.launch_bat);
        for arg in args {
            command.arg(arg);
        }
        command
            .current_dir(cwd)
            .env_remove("DISCORD_TOKEN")
            .output()
    }
}

impl Drop for LauncherFixture {
    fn drop(&mut self) {
        let _ = self.run_launcher(&["stop"], &self.caller_dir);
        if let Some(parent) = self.root.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

fn is_pid_alive(pid: u32) -> io::Result<bool> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
            ),
        ])
        .output()?;
    Ok(output.status.success())
}

fn wait_for_file_u32(path: &Path, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(value) = text.trim().parse::<u32>()
        {
            return Some(value);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

const HEALTHY_FAKE_ZEBOT_RS: &str = r#"
use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn main() {
    let cwd = env::current_dir().expect("cwd");
    assert!(cwd.join(".env").exists(), "expected cwd to be repository root");

    if env::var("SPAWN_CHILD").is_ok() {
        let child = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        let pid_path = cwd.join("temp").join("child.pid");
        if let Some(parent) = pid_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(pid_path, child.id().to_string()).expect("write child pid");
    }

    println!("zebot preflight checks passed");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
"#;

const FAILING_FAKE_ZEBOT_RS: &str = r#"
use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("error: required external audio tools are unavailable:");
    eprintln!("  - yt-dlp ('yt-dlp'): executable not found on PATH");
    ExitCode::FAILURE
}
"#;

#[test]
fn start_status_stop_lifecycle_is_reversible_idempotent_and_cleans_child_processes() {
    let fixture = LauncherFixture::create("lifecycle").expect("create fixture");
    fixture.write_dotenv().expect("write .env");
    let compile = fixture
        .compile_fake_zebot(HEALTHY_FAKE_ZEBOT_RS)
        .expect("compile fake zebot");
    assert!(
        compile.status.success(),
        "failed to compile fake zebot: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let initial_status = fixture
        .run_launcher(&["status"], &fixture.caller_dir)
        .expect("run initial status");
    assert_eq!(initial_status.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&initial_status.stdout).contains("Zebot is stopped."),
        "unexpected initial status stdout: {}",
        String::from_utf8_lossy(&initial_status.stdout)
    );

    let first_start = Command::new("cmd.exe")
        .arg("/d")
        .arg("/c")
        .arg(&fixture.launch_bat)
        .arg("start")
        .current_dir(&fixture.caller_dir)
        .env_remove("DISCORD_TOKEN")
        .env("SPAWN_CHILD", "1")
        .output()
        .expect("start launcher");
    assert_eq!(
        first_start.status.code(),
        Some(0),
        "start failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&first_start.stdout),
        String::from_utf8_lossy(&first_start.stderr)
    );
    let start_stdout = String::from_utf8_lossy(&first_start.stdout);
    assert!(start_stdout.contains("Zebot started (PID "));
    assert!(fixture.pid_file.exists());
    assert!(fixture.stdout_log.exists());
    assert!(fixture.stderr_log.exists());

    let child_pid = wait_for_file_u32(&fixture.child_pid_file, Duration::from_secs(5))
        .expect("timed out waiting for child pid");
    assert!(
        is_pid_alive(child_pid).expect("check child alive"),
        "expected spawned child process to be alive"
    );

    let running_status = fixture
        .run_launcher(&["status"], &fixture.caller_dir)
        .expect("run running status");
    assert_eq!(running_status.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&running_status.stdout).contains("Zebot is running (PID "));

    let pid_before_duplicate = fs::read_to_string(&fixture.pid_file).expect("read pid file");
    let duplicate_start = fixture
        .run_launcher(&["start"], &fixture.caller_dir)
        .expect("run duplicate start");
    assert_eq!(duplicate_start.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&duplicate_start.stdout).contains("Zebot is already running (PID ")
    );
    let pid_after_duplicate = fs::read_to_string(&fixture.pid_file).expect("read pid file");
    assert_eq!(pid_before_duplicate, pid_after_duplicate);

    fs::remove_file(&fixture.pid_file).expect("delete pid file while running");
    let recovered_status = fixture
        .run_launcher(&["status"], &fixture.caller_dir)
        .expect("run status after pid file removal");
    assert_eq!(recovered_status.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&recovered_status.stdout).contains("Zebot is running (PID "));
    assert!(fixture.pid_file.exists());

    let stop_out = fixture
        .run_launcher(&["stop"], &fixture.caller_dir)
        .expect("run stop");
    assert_eq!(
        stop_out.status.code(),
        Some(0),
        "stop failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&stop_out.stdout),
        String::from_utf8_lossy(&stop_out.stderr)
    );
    assert!(String::from_utf8_lossy(&stop_out.stdout).contains("Zebot stopped (PID "));
    assert!(!fixture.pid_file.exists());
    assert!(
        !is_pid_alive(child_pid).expect("check child stopped"),
        "child process must not survive launch.bat stop"
    );

    let second_stop = fixture
        .run_launcher(&["stop"], &fixture.caller_dir)
        .expect("run second stop");
    assert_eq!(second_stop.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&second_stop.stdout).contains("Zebot is already stopped."));

    let final_status = fixture
        .run_launcher(&["status"], &fixture.caller_dir)
        .expect("run final status");
    assert_eq!(final_status.status.code(), Some(3));
}

#[test]
fn stale_pid_file_is_ignored_and_cleaned_without_killing_unrelated_process() {
    let fixture = LauncherFixture::create("stale-pid").expect("create fixture");
    fixture.write_dotenv().expect("write .env");
    let compile = fixture
        .compile_fake_zebot(HEALTHY_FAKE_ZEBOT_RS)
        .expect("compile fake zebot");
    assert!(compile.status.success());

    fs::create_dir_all(fixture.pid_file.parent().expect("launcher dir")).expect("mkdir launcher");
    let unrelated_pid = std::process::id();
    fs::write(&fixture.pid_file, unrelated_pid.to_string()).expect("write stale pid");

    let status_out = fixture
        .run_launcher(&["status"], &fixture.caller_dir)
        .expect("run status");
    assert_eq!(status_out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&status_out.stdout);
    assert!(stdout.contains("Cleaned stale PID file"));
    assert!(stdout.contains("Zebot is stopped."));
    assert!(!fixture.pid_file.exists());
    assert!(is_pid_alive(unrelated_pid).expect("check unrelated process"));

    fs::write(&fixture.pid_file, unrelated_pid.to_string()).expect("write stale pid again");
    let stop_out = fixture
        .run_launcher(&["stop"], &fixture.caller_dir)
        .expect("run stop");
    assert_eq!(stop_out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&stop_out.stdout).contains("Zebot is already stopped."));
    assert!(!fixture.pid_file.exists());
    assert!(is_pid_alive(unrelated_pid).expect("check unrelated process after stop"));
}

#[test]
fn start_reports_missing_dotenv_and_preflight_failures_cleanly() {
    let fixture = LauncherFixture::create("errors").expect("create fixture");
    let compile = fixture
        .compile_fake_zebot(FAILING_FAKE_ZEBOT_RS)
        .expect("compile fake zebot");
    assert!(compile.status.success());

    let missing_env = fixture
        .run_launcher(&["start"], &fixture.caller_dir)
        .expect("run start without .env");
    assert_eq!(missing_env.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing_env.stdout).contains(".env file not found"),
        "expected missing .env message, got: {}",
        String::from_utf8_lossy(&missing_env.stdout)
    );
    assert!(!fixture.pid_file.exists());

    fixture.write_dotenv().expect("write .env");
    let failed_preflight = fixture
        .run_launcher(&["start"], &fixture.caller_dir)
        .expect("run start with failing preflight");
    assert_eq!(failed_preflight.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&failed_preflight.stdout);
    assert!(stdout.contains("Zebot failed to start"));
    assert!(stdout.contains("yt-dlp"));
    assert!(!fixture.pid_file.exists());
}

#[test]
fn help_and_invalid_arguments_return_expected_exit_codes() {
    let fixture = LauncherFixture::create("args").expect("create fixture");

    let help = fixture
        .run_launcher(&["help"], &fixture.caller_dir)
        .expect("run help");
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: launch.bat"));

    let invalid = fixture
        .run_launcher(&["unknown"], &fixture.caller_dir)
        .expect("run unknown command");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stdout).contains("invalid arguments"));

    let extra = fixture
        .run_launcher(&["start", "extra"], &fixture.caller_dir)
        .expect("run extra args");
    assert_eq!(extra.status.code(), Some(2));
}

#[test]
fn interactive_mode_holds_console_and_stops_on_enter_and_prompts_when_already_running() {
    let fixture = LauncherFixture::create("interactive").expect("create fixture");
    fixture.write_dotenv().expect("write .env");
    let compile = fixture
        .compile_fake_zebot(HEALTHY_FAKE_ZEBOT_RS)
        .expect("compile fake zebot");
    assert!(compile.status.success());

    let mut session = Command::new("cmd.exe")
        .arg("/d")
        .arg("/c")
        .arg(&fixture.launch_bat)
        .current_dir(&fixture.caller_dir)
        .env_remove("DISCORD_TOKEN")
        .env("SPAWN_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn interactive launch.bat");

    let child_pid = wait_for_file_u32(&fixture.child_pid_file, Duration::from_secs(10))
        .expect("timed out waiting for interactive child pid");
    assert!(is_pid_alive(child_pid).expect("check interactive child alive"));
    assert_eq!(
        fixture
            .run_launcher(&["status"], &fixture.caller_dir)
            .expect("status while interactive console is open")
            .status
            .code(),
        Some(0)
    );

    if let Some(mut stdin) = session.stdin.take() {
        stdin
            .write_all(b"\r\n")
            .expect("send Enter to interactive session");
    }
    let session_output = session
        .wait_with_output()
        .expect("wait for interactive session to stop on Enter");
    let session_stdout = String::from_utf8_lossy(&session_output.stdout);
    assert_eq!(
        session_output.status.code(),
        Some(0),
        "interactive session failed: stdout={session_stdout}, stderr={}",
        String::from_utf8_lossy(&session_output.stderr)
    );
    assert!(session_stdout.contains("Press Enter or close this window to stop Zebot."));
    assert!(session_stdout.contains("Zebot stopped (PID "));
    assert!(!fixture.pid_file.exists());
    assert!(
        !is_pid_alive(child_pid).expect("check child stopped after Enter"),
        "child process must not survive pressing Enter in interactive console"
    );
    assert_eq!(
        fixture
            .run_launcher(&["status"], &fixture.caller_dir)
            .expect("status after interactive Enter stop")
            .status
            .code(),
        Some(3)
    );

    let bg_start = fixture
        .run_launcher(&["start"], &fixture.caller_dir)
        .expect("start background zebot");
    assert_eq!(bg_start.status.code(), Some(0));

    let run_interactive_prompt = |stdin_text: &str| -> Output {
        let mut child = Command::new("cmd.exe")
            .arg("/d")
            .arg("/c")
            .arg(&fixture.launch_bat)
            .current_dir(&fixture.caller_dir)
            .env_remove("DISCORD_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn interactive prompt launch.bat");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(stdin_text.as_bytes());
        }
        child
            .wait_with_output()
            .expect("wait interactive prompt launch.bat")
    };

    let kept = run_interactive_prompt("n\r\n\r\n");
    assert_eq!(kept.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&kept.stdout).contains("Keeping Zebot running."));
    assert_eq!(
        fixture
            .run_launcher(&["status"], &fixture.caller_dir)
            .expect("status after keep")
            .status
            .code(),
        Some(0)
    );

    let stopped = run_interactive_prompt("\r\n\r\n");
    assert_eq!(stopped.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("Zebot stopped (PID "));
    assert_eq!(
        fixture
            .run_launcher(&["status"], &fixture.caller_dir)
            .expect("status after interactive prompt stop")
            .status
            .code(),
        Some(3)
    );
}

#[test]
fn start_reports_missing_cargo_when_cargo_toml_exists_without_binary() {
    let fixture = LauncherFixture::create("missing-cargo").expect("create fixture");
    fixture.write_dotenv().expect("write .env");
    fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname = \"zebot\"\n",
    )
    .expect("write placeholder Cargo.toml");

    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let minimal_path = format!(
        "{system_root}\\System32;{system_root};{system_root}\\System32\\WindowsPowerShell\\v1.0"
    );

    let output = Command::new("cmd.exe")
        .arg("/d")
        .arg("/c")
        .arg(&fixture.launch_bat)
        .arg("start")
        .current_dir(&fixture.caller_dir)
        .env_remove("DISCORD_TOKEN")
        .env("PATH", minimal_path)
        .output()
        .expect("run launch.bat without cargo");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Rust toolchain (cargo) was not found on PATH"),
        "expected missing cargo message, got: {stdout}"
    );
}
