# Quick Start for Devs

This guide covers installing prerequisites, configuring a Discord bot application, and running the standard verification checks for Zebot.

## 1. Required Tools

Install the following tools before building or running Zebot:

- **Rust toolchain (`rustup`):** Install from [rustup.rs](https://rustup.rs/) with the `rustfmt` and `clippy` components.
- **Git:** Required for version control and repository workflows.
- **C/C++ Build Tools + `cmake` (Windows):** Required for compiling native Discord voice/Opus dependencies (`songbird` / `opus2`).
  - Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the **Desktop development with C++** workload (MSVC compiler + Windows SDK).
  - Install `cmake` (for example via `winget install Kitware.CMake`) and ensure it is on `PATH`.
  - On Linux/macOS, install your platform C/C++ toolchain (`build-essential` / Xcode Command Line Tools) and `cmake`.
- **`yt-dlp`:** Used to resolve YouTube audio streams.
  - Windows example: `winget install yt-dlp.yt-dlp`
- **`ffmpeg`:** Used to transcode/pipe audio into Discord voice.
  - Windows example: `winget install Gyan.FFmpeg`

## 2. Verify Your Environment

Run these commands in a fresh terminal to confirm every prerequisite is available on `PATH`:

```powershell
rustc --version
cargo --version
cargo clippy --version
cargo fmt --version
git --version
cmake --version
yt-dlp --version
ffmpeg -version | Select-Object -First 1
```

If `yt-dlp` or `ffmpeg` are installed outside `PATH`, you can set `YT_DLP_PATH` and `FFMPEG_PATH` in `.env`.

## 3. Discord Application Setup

1. Open the [Discord Developer Portal](https://discord.com/developers/applications) and create a new application.
2. Go to the **Bot** tab:
   - Create/reset the bot token and save it for `DISCORD_TOKEN`.
   - Under **Privileged Gateway Intents**, no privileged intents are required for Slash Commands (`GUILD_VOICE_STATES` is a standard gateway intent requested by the bot at runtime).
3. Go to **OAuth2 -> URL Generator**:
   - Scopes: `bot`, `applications.commands`.
   - Bot Permissions: `View Channels`, `Send Messages`, `Connect`, `Speak`.
   - Open the generated URL and invite the bot to your local development server.
4. Collect your server and channel IDs:
   - In Discord, enable **User Settings -> Advanced -> Developer Mode**.
   - Right-click your development server -> **Copy Server ID** (`DISCORD_GUILD_ID`).
   - Right-click the single text channel where slash commands should be accepted -> **Copy Channel ID** (`ALLOWED_CHANNEL_ID`).

## 4. Configure `.env`

Copy `.env.example` to `.env` and fill in your values:

```powershell
Copy-Item .env.example .env
```

Required variables:

| Variable | Description |
| --- | --- |
| `DISCORD_TOKEN` | Discord bot token from the Developer Portal |
| `DISCORD_GUILD_ID` | Numeric Discord server (guild) ID |
| `ALLOWED_CHANNEL_ID` | Numeric text channel ID where bot commands are allowed |
| `API_TOKEN` | Strong random Bearer token for the local `127.0.0.1` HTTP API |

Optional variables:

| Variable | Default | Description |
| --- | --- | --- |
| `API_PORT` | `8080` | Port for the local HTTP/JSON API on `127.0.0.1` |
| `YT_DLP_PATH` | `yt-dlp` | Path or command name for `yt-dlp` |
| `FFMPEG_PATH` | `ffmpeg` | Path or command name for `ffmpeg` |

Generate a random `API_TOKEN` in PowerShell:

```powershell
[Convert]::ToBase64String((1..32 | ForEach-Object { Get-Random -Maximum 256 }))
```

Never commit `.env` (it is ignored by `.gitignore`).

## 5. Starting, Checking, and Stopping Zebot on Windows (`launch.bat`)

Use `launch.bat` in the repository root to manage Zebot without keeping a manual `cargo run` terminal open. The launcher resolves the repository root from its own location, so it works when double-clicked from Windows Explorer or called from any working directory.

### Interactive Usage (Windows Explorer)

Double-click `launch.bat` (or run it with no arguments):

- When Zebot is **stopped**, `launch.bat` builds the binary if needed, starts Zebot, streams its live output in the console window, and keeps the window open. Press `Enter` or close the console window (`X`) to stop Zebot and its child processes cleanly. If startup fails, the window stays open until you press `Enter` so you can read the error.
- When Zebot is **already running** (for example, started in the background via `launch.bat start`), `launch.bat` shows its PID and prompts whether to stop it (`Y/n`, default `Y`).

### Non-Interactive CLI Usage (Terminal and AI Agents)

Both human hosts and AI agents operate Zebot through the same non-interactive commands (they never block on `pause`):

```powershell
.\launch.bat start
.\launch.bat status
.\launch.bat stop
.\launch.bat help
```

Behavior and guarantees:

- **`start`:** Checks for `.env` (unless `DISCORD_TOKEN` is provided in the environment), builds an up-to-date `target\debug\zebot.exe` with `cargo build --bin zebot`, launches a single background Zebot instance, and verifies startup preflight checks (`.env`, `yt-dlp`, and `ffmpeg`). Calling `start` when Zebot is already running is safe and idempotent.
- **`status`:** Validates the tracked PID against the live Windows process table (`ExecutablePath` and process start time), removes any stale PID file automatically, and reports whether Zebot is running or stopped.
- **`stop`:** Terminates Zebot and its child process tree cleanly so no orphaned `zebot`, `yt-dlp`, or `ffmpeg` processes remain, and removes the PID file. Calling `stop` when Zebot is already stopped is safe and idempotent.

Exit codes:

| Exit code | Meaning |
| --- | --- |
| `0` | Success (or Zebot is running for `status`) |
| `1` | Runtime or prerequisite error (missing `.env`, missing Rust toolchain, build failure, failed `yt-dlp`/`ffmpeg` preflight, or failed stop) |
| `2` | Invalid command-line arguments |
| `3` | Zebot is stopped (`status` only) |

Runtime state and logs are stored strictly under `temp\launcher\` (ignored by Git):

- `temp\launcher\zebot.pid` — tracked PID and process metadata for the live Zebot instance
- `temp\launcher\zebot.stdout.log` — standard output from the latest run
- `temp\launcher\zebot.stderr.log` — standard error from the latest run

## 6. Standard Verification Commands

Run the full check suite before opening a PR:

```powershell
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

To run Zebot directly in the foreground during development (runs `.env` and external tool preflight checks, connects to Discord, registers `/status`, `/play`, `/stop`, and `/leave` in `DISCORD_GUILD_ID`, starts the local HTTP/JSON API on `127.0.0.1:<API_PORT>`, and shuts down cleanly on `Ctrl+C`):

```powershell
cargo run
```

For local HTTP/JSON API endpoints, authentication details, and copy-pasteable host/AI agent examples, see [Local HTTP/JSON API](local-api.md).

## 7. Code Guardrails

Zebot enforces strict compile-time and lint-time guardrails for both human developers and AI agents:

- **No `unsafe` code (`unsafe_code = "forbid"`):** Any `unsafe` block anywhere in `src/`, `tests/`, or `build.rs` fails compilation.
- **No ignored results (`unused_must_use = "deny"`):** Values marked `#[must_use]` (such as `Result`) must be handled explicitly.
- **No panicking shortcuts in production code:** Clippy denies `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, and `dbg_macro` in production code. `unwrap` and `expect` are permitted inside tests via `clippy.toml`.
- **Zero-comments policy (`build.rs`):** All Rust comments (`//`, `/* */`, `///`, `//!`) are forbidden across `src/`, `tests/`, and `build.rs` and fail compilation during the build script step. String literals (such as `"https://www.youtube.com/..."`), raw strings, and character literals are lexed accurately so URLs and fixtures never cause false positives. Express intent through clear names, small functions, and GitHub issues instead of code comments.
