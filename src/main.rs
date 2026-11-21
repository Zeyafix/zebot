use std::process::ExitCode;
use std::sync::Arc;

use zebot::config::Config;
use zebot::core::{AppCore, UnavailableVoiceBackend};
use zebot::dependencies::check_external_tools;
use zebot::discord::run_discord_bot;

#[tokio::main]
async fn main() -> ExitCode {
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

    let app_core = Arc::new(AppCore::new(UnavailableVoiceBackend));
    let shutdown_signal = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                println!("shutdown signal received (Ctrl+C); disconnecting from Discord...");
            }
            Err(error) => {
                eprintln!("warning: failed to listen for Ctrl+C signal: {error}");
                std::future::pending::<()>().await;
            }
        }
    };

    match run_discord_bot(&config, app_core, shutdown_signal).await {
        Ok(()) => {
            println!("zebot shut down cleanly");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
