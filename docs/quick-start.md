# Quick Start for Devs

This guide covers installing prerequisites, configuring a Discord bot application, and running the standard verification checks for Zebot.

## 1. Required Tools

Install the following tools before building or running Zebot:

- **Rust toolchain (`rustup`):** Install from [rustup.rs](https://rustup.rs/) with the `rustfmt` and `clippy` components.
- **Git:** Required for version control and repository workflows.
- **C/C++ Build Tools + `cmake` (Windows):** Required for compiling native Discord voice/Opus dependencies (`songbird` / `audiopus`).
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

## 5. Standard Verification Commands

Run the full check suite before opening a PR:

```powershell
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

To start Zebot locally (runs `.env` and external tool preflight checks, connects to Discord, registers `/status` in `DISCORD_GUILD_ID`, and shuts down cleanly on `Ctrl+C`):

```powershell
cargo run
```

## 6. Code Guardrails

Zebot enforces strict compile-time and lint-time guardrails for both human developers and AI agents:

- **No `unsafe` code (`unsafe_code = "forbid"`):** Any `unsafe` block anywhere in `src/`, `tests/`, or `build.rs` fails compilation.
- **No ignored results (`unused_must_use = "deny"`):** Values marked `#[must_use]` (such as `Result`) must be handled explicitly.
- **No panicking shortcuts in production code:** Clippy denies `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, and `dbg_macro` in production code. `unwrap` and `expect` are permitted inside tests via `clippy.toml`.
- **Zero-comments policy (`build.rs`):** All Rust comments (`//`, `/* */`, `///`, `//!`) are forbidden across `src/`, `tests/`, and `build.rs` and fail compilation during the build script step. String literals (such as `"https://www.youtube.com/..."`), raw strings, and character literals are lexed accurately so URLs and fixtures never cause false positives. Express intent through clear names, small functions, and GitHub issues instead of code comments.
