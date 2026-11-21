use std::process::ExitCode;

use zebot::config::Config;
use zebot::dependencies::check_external_tools;

fn main() -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let tools = match check_external_tools(&config.yt_dlp_path, &config.ffmpeg_path) {
        Ok(tools) => tools,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("zebot preflight checks passed");
    println!("  guild_id: {}", config.discord_guild_id);
    println!("  allowed_channel_id: {}", config.allowed_channel_id);
    println!("  api_bind: 127.0.0.1:{}", config.api_port);
    println!("  yt-dlp: {}", tools.yt_dlp);
    println!("  ffmpeg: {}", tools.ffmpeg);

    ExitCode::SUCCESS
}
