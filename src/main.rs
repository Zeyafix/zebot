use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Arc;

use zebot::api::{ApiConfig, BoundApiServer};
use zebot::config::Config;
use zebot::core::{AppCore, CancellationToken, VoiceLifecycleListener};
use zebot::dependencies::check_external_tools;
use zebot::discord::prepare_discord_bot;
use zebot::voice::SongbirdVoiceBackend;

fn write_stdout_line(message: &str) {
    let _ = writeln!(io::stdout(), "{message}");
}

fn write_stderr_line(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::windows;

    let mut ctrl_c = match windows::ctrl_c() {
        Ok(stream) => Some(stream),
        Err(error) => {
            write_stderr_line(&format!(
                "warning: failed to listen for Ctrl+C signal: {error}"
            ));
            None
        }
    };
    let mut ctrl_break = match windows::ctrl_break() {
        Ok(stream) => Some(stream),
        Err(error) => {
            write_stderr_line(&format!(
                "warning: failed to listen for Ctrl+Break signal: {error}"
            ));
            None
        }
    };
    let mut ctrl_close = match windows::ctrl_close() {
        Ok(stream) => Some(stream),
        Err(error) => {
            write_stderr_line(&format!(
                "warning: failed to listen for console close signal: {error}"
            ));
            None
        }
    };
    let mut ctrl_shutdown = match windows::ctrl_shutdown() {
        Ok(stream) => Some(stream),
        Err(error) => {
            write_stderr_line(&format!(
                "warning: failed to listen for system shutdown signal: {error}"
            ));
            None
        }
    };

    let recv_c = async {
        match ctrl_c.as_mut() {
            Some(stream) => {
                let _ = stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let recv_break = async {
        match ctrl_break.as_mut() {
            Some(stream) => {
                let _ = stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let recv_close = async {
        match ctrl_close.as_mut() {
            Some(stream) => {
                let _ = stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let recv_shutdown = async {
        match ctrl_shutdown.as_mut() {
            Some(stream) => {
                let _ = stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };

    let reason = tokio::select! {
        () = recv_c => "Ctrl+C",
        () = recv_break => "Ctrl+Break",
        () = recv_close => "console close",
        () = recv_shutdown => "system shutdown",
    };

    write_stdout_line(&format!(
        "shutdown signal received ({reason}); disconnecting from Discord..."
    ));
}

#[cfg(not(windows))]
async fn wait_for_shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => {
            write_stdout_line("shutdown signal received (Ctrl+C); disconnecting from Discord...");
        }
        Err(error) => {
            write_stderr_line(&format!(
                "warning: failed to listen for Ctrl+C signal: {error}"
            ));
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            write_stderr_line(&format!("error: {error}"));
            return ExitCode::FAILURE;
        }
    };

    let tools = match check_external_tools(&config.yt_dlp_path, &config.ffmpeg_path) {
        Ok(tools) => tools,
        Err(error) => {
            write_stderr_line(&format!("error: {error}"));
            return ExitCode::FAILURE;
        }
    };

    write_stdout_line("zebot preflight checks passed");
    write_stdout_line(&format!("  guild_id: {}", config.discord_guild_id));
    write_stdout_line(&format!(
        "  allowed_channel_id: {}",
        config.allowed_channel_id
    ));
    write_stdout_line(&format!("  api_bind: 127.0.0.1:{}", config.api_port));
    write_stdout_line(&format!("  yt-dlp: {}", tools.yt_dlp));
    write_stdout_line(&format!("  ffmpeg: {}", tools.ffmpeg));

    let voice_backend =
        SongbirdVoiceBackend::new(config.yt_dlp_path.clone(), config.ffmpeg_path.clone());
    let songbird = voice_backend.songbird();
    let app_core = Arc::new(AppCore::new(voice_backend));
    let lifecycle_listener: Arc<dyn VoiceLifecycleListener> = app_core.clone();
    app_core
        .backend()
        .attach_listener(Arc::downgrade(&lifecycle_listener));

    let discord_bot = match prepare_discord_bot(&config, Arc::clone(&app_core), songbird).await {
        Ok(bot) => bot,
        Err(error) => {
            write_stderr_line(&format!("error: {error}"));
            return ExitCode::FAILURE;
        }
    };

    let voice_directory = discord_bot.voice_directory();
    let api_server = match BoundApiServer::bind(
        ApiConfig::from_config(&config),
        Arc::clone(&app_core),
        voice_directory,
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            write_stderr_line(&format!("error: {error}"));
            return ExitCode::FAILURE;
        }
    };

    write_stdout_line(&format!(
        "local HTTP API listening on http://{}",
        api_server.local_addr()
    ));

    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    let signal_task = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        signal_shutdown.cancel();
    });

    let api_shutdown = shutdown.clone();
    let api_cancel_on_exit = shutdown.clone();
    let api_future = async move {
        let result = api_server
            .serve(async move {
                api_shutdown.cancelled().await;
            })
            .await;
        api_cancel_on_exit.cancel();
        result
    };

    let discord_shutdown = shutdown.clone();
    let discord_cancel_on_exit = shutdown.clone();
    let discord_future = async move {
        let result = discord_bot
            .run(async move {
                discord_shutdown.cancelled().await;
            })
            .await;
        discord_cancel_on_exit.cancel();
        result
    };

    let (discord_result, api_result) = tokio::join!(discord_future, api_future);
    signal_task.abort();

    let mut had_error = false;
    if let Err(error) = discord_result {
        write_stderr_line(&format!("error: {error}"));
        had_error = true;
    }
    if let Err(error) = api_result {
        write_stderr_line(&format!("error: {error}"));
        had_error = true;
    }

    if had_error {
        ExitCode::FAILURE
    } else {
        write_stdout_line("zebot shut down cleanly");
        ExitCode::SUCCESS
    }
}
