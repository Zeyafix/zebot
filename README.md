# Zebot

A locally hosted, AI-friendly Discord bot written in Rust that streams YouTube audio into Discord voice channels and exposes a unified local API so both the host and AI agents operate the bot the same way.

## Getting Started

See [Quick Start for Devs](docs/quick-start.md) to install prerequisites, configure a Discord application, and run local verification checks.

## Project Principles

- **Local-first:** Runs on the host's machine with local configuration and no external database.
- **YouTube audio streaming:** Streams audio directly through `yt-dlp` and `ffmpeg` into Discord voice channels without downloading full files to disk.
- **AI-friendly:** Every capability exposed to humans is available to AI agents through the same underlying core.
- **Strict safety guardrails:** `unsafe` code and Rust source comments are forbidden at compile time, and strict Clippy lints are enforced across the repository.

## License

Licensed under [CC BY-NC-SA 4.0](LICENSE).
