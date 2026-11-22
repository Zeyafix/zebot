use std::collections::HashMap;
use std::io::{self, SeekFrom};
use std::num::NonZeroU64;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use serde::Deserialize;
use serenity::async_trait;
use songbird::driver::CryptoMode;
use songbird::error::JoinError;
use songbird::events::{
    CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler, TrackEvent,
};
use songbird::id::{ChannelId as SongbirdChannelId, GuildId as SongbirdGuildId};
use songbird::input::core::io::MediaSource;
use songbird::input::{AsyncAdapterStream, AsyncMediaSource, Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::{Config as SongbirdConfig, Songbird};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, ReadBuf};
use tokio::process::{Child, ChildStdout, Command};
use tokio::runtime::Handle;

use crate::core::{
    CancellationToken, PlaybackGeneration, VoiceBackend, VoiceBackendError, VoiceLifecycleListener,
};
use crate::state::{GuildId, TrackMetadata, VoiceChannelId, VoiceTarget};
use crate::youtube::YouTubeUrl;

pub const DISCORD_SAMPLE_RATE: u32 = 48_000;
pub const DISCORD_CHANNELS: u32 = 2;
pub const STREAM_RING_BUFFER_BYTES: usize = 128 * 1024;
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_METADATA_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_STDERR_OUTPUT_BYTES: usize = 64 * 1024;

pub fn build_songbird_config() -> SongbirdConfig {
    SongbirdConfig::default().crypto_mode(CryptoMode::Aes256Gcm)
}

pub fn build_yt_dlp_metadata_args(url: &YouTubeUrl) -> Vec<String> {
    vec![
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--skip-download".to_string(),
        "--dump-single-json".to_string(),
        "-f".to_string(),
        "bestaudio/best".to_string(),
        "--".to_string(),
        url.as_str().to_string(),
    ]
}

pub fn build_yt_dlp_stream_args(url: &YouTubeUrl) -> Vec<String> {
    vec![
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-part".to_string(),
        "--no-cache-dir".to_string(),
        "-f".to_string(),
        "bestaudio/best".to_string(),
        "-o".to_string(),
        "-".to_string(),
        "--".to_string(),
        url.as_str().to_string(),
    ]
}

pub fn build_ffmpeg_pcm_args() -> Vec<String> {
    vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-vn".to_string(),
        "-sn".to_string(),
        "-dn".to_string(),
        "-f".to_string(),
        "f32le".to_string(),
        "-ar".to_string(),
        DISCORD_SAMPLE_RATE.to_string(),
        "-ac".to_string(),
        DISCORD_CHANNELS.to_string(),
        "pipe:1".to_string(),
    ]
}

#[derive(Debug, Deserialize)]
struct YtDlpMetadataPayload {
    title: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
}

pub fn parse_yt_dlp_metadata(
    url: &YouTubeUrl,
    raw_json: &[u8],
) -> Result<TrackMetadata, VoiceBackendError> {
    let payload: YtDlpMetadataPayload = serde_json::from_slice(raw_json).map_err(|error| {
        VoiceBackendError::ResolveFailed(format!("failed to parse yt-dlp metadata JSON: {error}"))
    })?;

    let title = normalize_optional_text(payload.title);
    let channel = normalize_optional_text(payload.channel)
        .or_else(|| normalize_optional_text(payload.uploader));
    let duration_seconds = payload.duration.and_then(parse_duration_seconds);

    Ok(TrackMetadata {
        url: url.clone(),
        title,
        channel,
        duration_seconds,
    })
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn parse_duration_seconds(raw_seconds: f64) -> Option<u64> {
    if !raw_seconds.is_finite() || raw_seconds < 0.0 {
        return None;
    }

    let rounded = raw_seconds.round();
    if rounded > 31_536_000_000.0 {
        return None;
    }

    format!("{rounded:.0}").parse::<u64>().ok()
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

async fn read_stream_bounded<R>(mut reader: R, max_bytes: usize) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let read_count = reader.read(&mut chunk).await?;
        if read_count == 0 {
            break;
        }

        let remaining = max_bytes.saturating_sub(output.len());
        if remaining > 0 {
            let take = read_count.min(remaining);
            output.extend_from_slice(&chunk[..take]);
        }
    }

    Ok(output)
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
}

pub async fn resolve_track_with_command(
    program: &str,
    args: &[String],
    url: &YouTubeUrl,
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<TrackMetadata, VoiceBackendError> {
    if cancel.is_cancelled() {
        return Err(VoiceBackendError::Cancelled);
    }

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            VoiceBackendError::ResolveFailed(format!("failed to start '{program}': {error}"))
        })?;

    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child).await;
        return Err(VoiceBackendError::ResolveFailed(
            "yt-dlp stdout pipe was unavailable".to_string(),
        ));
    };

    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child).await;
        return Err(VoiceBackendError::ResolveFailed(
            "yt-dlp stderr pipe was unavailable".to_string(),
        ));
    };

    let stdout_task =
        tokio::spawn(async move { read_stream_bounded(stdout, MAX_METADATA_OUTPUT_BYTES).await });
    let stderr_task =
        tokio::spawn(async move { read_stream_bounded(stderr, MAX_STDERR_OUTPUT_BYTES).await });

    enum WaitOutcome {
        Exited(io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }

    let outcome = tokio::select! {
        status = child.wait() => WaitOutcome::Exited(status),
        () = cancel.cancelled() => WaitOutcome::Cancelled,
        () = tokio::time::sleep(timeout) => WaitOutcome::TimedOut,
    };

    match outcome {
        WaitOutcome::Cancelled => {
            terminate_child(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            Err(VoiceBackendError::Cancelled)
        }
        WaitOutcome::TimedOut => {
            terminate_child(&mut child).await;
            stdout_task.abort();
            stderr_task.abort();
            Err(VoiceBackendError::ResolveFailed(
                "yt-dlp timed out while resolving YouTube metadata".to_string(),
            ))
        }
        WaitOutcome::Exited(status_result) => {
            let status = status_result.map_err(|error| {
                stdout_task.abort();
                stderr_task.abort();
                VoiceBackendError::ResolveFailed(format!(
                    "failed while waiting for yt-dlp: {error}"
                ))
            })?;

            let stdout_bytes = stdout_task
                .await
                .map_err(|error| {
                    VoiceBackendError::ResolveFailed(format!(
                        "yt-dlp stdout reader failed: {error}"
                    ))
                })?
                .map_err(|error| {
                    VoiceBackendError::ResolveFailed(format!(
                        "failed reading yt-dlp stdout: {error}"
                    ))
                })?;

            let stderr_bytes = stderr_task
                .await
                .map_err(|error| {
                    VoiceBackendError::ResolveFailed(format!(
                        "yt-dlp stderr reader failed: {error}"
                    ))
                })?
                .map_err(|error| {
                    VoiceBackendError::ResolveFailed(format!(
                        "failed reading yt-dlp stderr: {error}"
                    ))
                })?;

            if cancel.is_cancelled() {
                return Err(VoiceBackendError::Cancelled);
            }

            if !status.success() {
                let detail = first_non_empty_line(&stderr_bytes)
                    .unwrap_or_else(|| format!("exit status {status}"));
                return Err(VoiceBackendError::ResolveFailed(detail));
            }

            parse_yt_dlp_metadata(url, &stdout_bytes)
        }
    }
}

struct PipelineChildren {
    yt_dlp: Child,
    ffmpeg: Child,
}

pub struct PipelineController {
    children: Mutex<Option<PipelineChildren>>,
}

impl PipelineController {
    fn new(yt_dlp: Child, ffmpeg: Child) -> Arc<Self> {
        Arc::new(Self {
            children: Mutex::new(Some(PipelineChildren { yt_dlp, ffmpeg })),
        })
    }

    fn lock_children(&self) -> MutexGuard<'_, Option<PipelineChildren>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn child_pids(&self) -> (Option<u32>, Option<u32>) {
        let guard = self.lock_children();
        match guard.as_ref() {
            Some(children) => (children.yt_dlp.id(), children.ffmpeg.id()),
            None => (None, None),
        }
    }

    pub fn is_active(&self) -> bool {
        self.lock_children().is_some()
    }

    pub async fn terminate_async(&self) {
        let taken = self.lock_children().take();
        if let Some(mut children) = taken {
            terminate_child(&mut children.ffmpeg).await;
            terminate_child(&mut children.yt_dlp).await;
        }
    }

    pub fn terminate_now(&self) {
        let taken = self.lock_children().take();
        if let Some(mut children) = taken {
            let _ = children.ffmpeg.start_kill();
            let _ = children.yt_dlp.start_kill();
            if let Ok(handle) = Handle::try_current() {
                handle.spawn(async move {
                    let _ = children.ffmpeg.wait().await;
                    let _ = children.yt_dlp.wait().await;
                });
            }
        }
    }
}

impl Drop for PipelineController {
    fn drop(&mut self) {
        self.terminate_now();
    }
}

struct ManagedPipeMediaSource {
    reader: ChildStdout,
    controller: Arc<PipelineController>,
}

impl AsyncRead for ManagedPipeMediaSource {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl AsyncSeek for ManagedPipeMediaSource {
    fn start_seek(self: Pin<&mut Self>, _position: SeekFrom) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live audio stream is not seekable",
        ))
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live audio stream is not seekable",
        )))
    }
}

#[async_trait]
impl AsyncMediaSource for ManagedPipeMediaSource {
    fn is_seekable(&self) -> bool {
        false
    }

    async fn byte_len(&self) -> Option<u64> {
        None
    }
}

impl Drop for ManagedPipeMediaSource {
    fn drop(&mut self) {
        self.controller.terminate_now();
    }
}

struct OwnedPipelineStream {
    inner: AsyncAdapterStream,
    controller: Arc<PipelineController>,
}

impl io::Read for OwnedPipelineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Seek for OwnedPipelineStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for OwnedPipelineStream {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

impl Drop for OwnedPipelineStream {
    fn drop(&mut self) {
        self.controller.terminate_now();
    }
}

pub async fn spawn_streaming_pipeline_with_commands(
    yt_dlp_program: &str,
    yt_dlp_args: &[String],
    ffmpeg_program: &str,
    ffmpeg_args: &[String],
) -> Result<(Input, Arc<PipelineController>), VoiceBackendError> {
    let mut yt_dlp = Command::new(yt_dlp_program)
        .args(yt_dlp_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            VoiceBackendError::PlaybackFailed(format!("failed to start yt-dlp stream: {error}"))
        })?;

    let Some(yt_stdout) = yt_dlp.stdout.take() else {
        terminate_child(&mut yt_dlp).await;
        return Err(VoiceBackendError::PlaybackFailed(
            "yt-dlp stdout pipe was unavailable".to_string(),
        ));
    };

    let yt_stdout_stdio: Stdio = match yt_stdout.try_into() {
        Ok(stdio) => stdio,
        Err(error) => {
            terminate_child(&mut yt_dlp).await;
            return Err(VoiceBackendError::PlaybackFailed(format!(
                "failed to attach yt-dlp stdout to ffmpeg stdin: {error}"
            )));
        }
    };

    let mut ffmpeg = match Command::new(ffmpeg_program)
        .args(ffmpeg_args)
        .stdin(yt_stdout_stdio)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            terminate_child(&mut yt_dlp).await;
            return Err(VoiceBackendError::PlaybackFailed(format!(
                "failed to start ffmpeg transcoder: {error}"
            )));
        }
    };

    let Some(ffmpeg_stdout) = ffmpeg.stdout.take() else {
        terminate_child(&mut ffmpeg).await;
        terminate_child(&mut yt_dlp).await;
        return Err(VoiceBackendError::PlaybackFailed(
            "ffmpeg stdout pipe was unavailable".to_string(),
        ));
    };

    let controller = PipelineController::new(yt_dlp, ffmpeg);
    let media_source = ManagedPipeMediaSource {
        reader: ffmpeg_stdout,
        controller: Arc::clone(&controller),
    };
    let async_stream = AsyncAdapterStream::new(Box::new(media_source), STREAM_RING_BUFFER_BYTES);
    let owned_stream = OwnedPipelineStream {
        inner: async_stream,
        controller: Arc::clone(&controller),
    };
    let raw_adapter = RawAdapter::new(owned_stream, DISCORD_SAMPLE_RATE, DISCORD_CHANNELS);

    Ok((Input::from(raw_adapter), controller))
}

struct ActiveGuildVoice {
    session_id: u64,
    track_handle: Option<TrackHandle>,
    pipeline: Option<Arc<PipelineController>>,
}

pub struct SongbirdVoiceBackend {
    songbird: Arc<Songbird>,
    yt_dlp_path: String,
    ffmpeg_path: String,
    sessions: Arc<Mutex<HashMap<GuildId, ActiveGuildVoice>>>,
    next_session_id: AtomicU64,
    listener: Arc<OnceLock<Weak<dyn VoiceLifecycleListener>>>,
}

impl SongbirdVoiceBackend {
    pub fn new(yt_dlp_path: impl Into<String>, ffmpeg_path: impl Into<String>) -> Self {
        let songbird = Songbird::serenity_from_config(build_songbird_config());
        Self::with_songbird(songbird, yt_dlp_path, ffmpeg_path)
    }

    pub fn with_songbird(
        songbird: Arc<Songbird>,
        yt_dlp_path: impl Into<String>,
        ffmpeg_path: impl Into<String>,
    ) -> Self {
        Self {
            songbird,
            yt_dlp_path: yt_dlp_path.into(),
            ffmpeg_path: ffmpeg_path.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: AtomicU64::new(1),
            listener: Arc::new(OnceLock::new()),
        }
    }

    pub fn songbird(&self) -> Arc<Songbird> {
        Arc::clone(&self.songbird)
    }

    pub fn attach_listener(&self, listener: Weak<dyn VoiceLifecycleListener>) {
        let _ = self.listener.set(listener);
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<GuildId, ActiveGuildVoice>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn allocate_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::SeqCst)
    }

    async fn stop_guild_playback(&self, guild_id: GuildId) {
        let (track_handle, pipeline) = {
            let mut sessions = self.lock_sessions();
            match sessions.get_mut(&guild_id) {
                Some(active) => (active.track_handle.take(), active.pipeline.take()),
                None => (None, None),
            }
        };

        if let Some(handle) = track_handle {
            let _ = handle.stop();
        }

        if let Some(sg_guild) = to_songbird_guild_id(guild_id)
            && let Some(call) = self.songbird.get(sg_guild)
        {
            let mut handler = call.lock().await;
            handler.stop();
        }

        if let Some(controller) = pipeline {
            controller.terminate_async().await;
        }
    }
}

impl Drop for SongbirdVoiceBackend {
    fn drop(&mut self) {
        let drained: Vec<ActiveGuildVoice> = {
            let mut sessions = self.lock_sessions();
            sessions.drain().map(|(_, entry)| entry).collect()
        };

        for entry in drained {
            if let Some(handle) = entry.track_handle {
                let _ = handle.stop();
            }
            if let Some(pipeline) = entry.pipeline {
                pipeline.terminate_now();
            }
        }
    }
}

impl VoiceBackend for SongbirdVoiceBackend {
    async fn resolve_track(
        &self,
        url: &YouTubeUrl,
        cancel: CancellationToken,
    ) -> Result<TrackMetadata, VoiceBackendError> {
        let args = build_yt_dlp_metadata_args(url);
        resolve_track_with_command(&self.yt_dlp_path, &args, url, cancel, RESOLVE_TIMEOUT).await
    }

    async fn join(
        &self,
        target: VoiceTarget,
        cancel: CancellationToken,
    ) -> Result<(), VoiceBackendError> {
        if cancel.is_cancelled() {
            return Err(VoiceBackendError::Cancelled);
        }

        let sg_guild = to_songbird_guild_id(target.guild_id)
            .ok_or_else(|| VoiceBackendError::JoinFailed("invalid guild ID".to_string()))?;
        let sg_channel = to_songbird_channel_id(target.channel_id)
            .ok_or_else(|| VoiceBackendError::JoinFailed("invalid voice channel ID".to_string()))?;

        let had_existing_session = self.lock_sessions().contains_key(&target.guild_id);

        let join_result = tokio::select! {
            result = self.songbird.join(sg_guild, sg_channel) => Some(result),
            () = cancel.cancelled() => None,
        };

        let Some(result) = join_result else {
            if !had_existing_session {
                let _ = self.songbird.remove(sg_guild).await;
            }
            return Err(VoiceBackendError::Cancelled);
        };

        let call = result.map_err(|error| VoiceBackendError::JoinFailed(error.to_string()))?;

        if cancel.is_cancelled() {
            if !had_existing_session {
                let _ = self.songbird.remove(sg_guild).await;
            }
            return Err(VoiceBackendError::Cancelled);
        }

        let session_id = {
            let mut sessions = self.lock_sessions();
            match sessions.get(&target.guild_id) {
                Some(existing) => existing.session_id,
                None => {
                    let id = self.allocate_session_id();
                    sessions.insert(
                        target.guild_id,
                        ActiveGuildVoice {
                            session_id: id,
                            track_handle: None,
                            pipeline: None,
                        },
                    );
                    id
                }
            }
        };

        let disconnect_handler = DisconnectEventHandler {
            guild_id: target.guild_id,
            session_id,
            sessions: Arc::clone(&self.sessions),
            listener: Arc::clone(&self.listener),
        };

        {
            let mut handler = call.lock().await;
            handler.remove_all_global_events();
            handler.add_global_event(Event::Core(CoreEvent::DriverDisconnect), disconnect_handler);
        }

        Ok(())
    }

    async fn play(
        &self,
        target: VoiceTarget,
        track: &TrackMetadata,
        generation: PlaybackGeneration,
        cancel: CancellationToken,
    ) -> Result<(), VoiceBackendError> {
        if cancel.is_cancelled() {
            return Err(VoiceBackendError::Cancelled);
        }

        let sg_guild = to_songbird_guild_id(target.guild_id)
            .ok_or_else(|| VoiceBackendError::PlaybackFailed("invalid guild ID".to_string()))?;

        self.stop_guild_playback(target.guild_id).await;

        let call = self.songbird.get(sg_guild).ok_or_else(|| {
            VoiceBackendError::PlaybackFailed(
                "bot is not connected to a Discord voice channel".to_string(),
            )
        })?;

        let yt_args = build_yt_dlp_stream_args(&track.url);
        let ffmpeg_args = build_ffmpeg_pcm_args();
        let (input, controller) = spawn_streaming_pipeline_with_commands(
            &self.yt_dlp_path,
            &yt_args,
            &self.ffmpeg_path,
            &ffmpeg_args,
        )
        .await?;

        if cancel.is_cancelled() {
            controller.terminate_async().await;
            return Err(VoiceBackendError::Cancelled);
        }

        let track_handle = {
            let mut handler = call.lock().await;
            handler.play_only_input(input)
        };

        let end_handler = TrackEndEventHandler {
            guild_id: target.guild_id,
            generation,
            controller: Arc::clone(&controller),
            sessions: Arc::clone(&self.sessions),
            listener: Arc::clone(&self.listener),
        };

        let _ = track_handle.add_event(Event::Track(TrackEvent::End), end_handler.clone());
        let _ = track_handle.add_event(Event::Track(TrackEvent::Error), end_handler);

        let fallback_session_id = self.allocate_session_id();
        {
            let mut sessions = self.lock_sessions();
            let entry = sessions
                .entry(target.guild_id)
                .or_insert_with(|| ActiveGuildVoice {
                    session_id: fallback_session_id,
                    track_handle: None,
                    pipeline: None,
                });
            entry.track_handle = Some(track_handle);
            entry.pipeline = Some(controller);
        }

        Ok(())
    }

    async fn stop(&self, guild_id: GuildId) -> Result<(), VoiceBackendError> {
        self.stop_guild_playback(guild_id).await;
        Ok(())
    }

    async fn leave(&self, guild_id: GuildId) -> Result<(), VoiceBackendError> {
        let removed = self.lock_sessions().remove(&guild_id);

        if let Some(entry) = removed {
            if let Some(handle) = entry.track_handle {
                let _ = handle.stop();
            }
            if let Some(pipeline) = entry.pipeline {
                pipeline.terminate_async().await;
            }
        }

        let Some(sg_guild) = to_songbird_guild_id(guild_id) else {
            return Ok(());
        };

        match self.songbird.remove(sg_guild).await {
            Ok(()) | Err(JoinError::NoCall) => Ok(()),
            Err(error) => Err(VoiceBackendError::LeaveFailed(error.to_string())),
        }
    }
}

#[derive(Clone)]
struct TrackEndEventHandler {
    guild_id: GuildId,
    generation: PlaybackGeneration,
    controller: Arc<PipelineController>,
    sessions: Arc<Mutex<HashMap<GuildId, ActiveGuildVoice>>>,
    listener: Arc<OnceLock<Weak<dyn VoiceLifecycleListener>>>,
}

#[async_trait]
impl SongbirdEventHandler for TrackEndEventHandler {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        self.controller.terminate_async().await;

        {
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(active) = sessions.get_mut(&self.guild_id)
                && active
                    .pipeline
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &self.controller))
            {
                active.pipeline = None;
                active.track_handle = None;
            }
        }

        if let Some(listener) = self.listener.get().and_then(Weak::upgrade) {
            listener.on_track_ended(self.generation);
        }

        Some(Event::Cancel)
    }
}

struct DisconnectEventHandler {
    guild_id: GuildId,
    session_id: u64,
    sessions: Arc<Mutex<HashMap<GuildId, ActiveGuildVoice>>>,
    listener: Arc<OnceLock<Weak<dyn VoiceLifecycleListener>>>,
}

#[async_trait]
impl SongbirdEventHandler for DisconnectEventHandler {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        let removed = {
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let is_matching = sessions
                .get(&self.guild_id)
                .is_some_and(|entry| entry.session_id == self.session_id);
            if is_matching {
                sessions.remove(&self.guild_id)
            } else {
                None
            }
        };

        let Some(entry) = removed else {
            return Some(Event::Cancel);
        };

        if let Some(handle) = entry.track_handle {
            let _ = handle.stop();
        }
        if let Some(pipeline) = entry.pipeline {
            pipeline.terminate_async().await;
        }

        if let Some(listener) = self.listener.get().and_then(Weak::upgrade) {
            listener.on_voice_disconnected(self.guild_id);
        }

        Some(Event::Cancel)
    }
}

fn to_songbird_guild_id(guild_id: GuildId) -> Option<SongbirdGuildId> {
    NonZeroU64::new(guild_id.get()).map(SongbirdGuildId)
}

fn to_songbird_channel_id(channel_id: VoiceChannelId) -> Option<SongbirdChannelId> {
    NonZeroU64::new(channel_id.get()).map(SongbirdChannelId)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::Command as StdCommand;
    use std::time::Duration;

    use songbird::driver::CryptoMode;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use songbird::input::{Input, RawAdapter};

    use super::{
        DISCORD_CHANNELS, DISCORD_SAMPLE_RATE, build_ffmpeg_pcm_args, build_songbird_config,
        build_yt_dlp_metadata_args, build_yt_dlp_stream_args, parse_yt_dlp_metadata,
        resolve_track_with_command, spawn_streaming_pipeline_with_commands,
    };
    use crate::core::{CancellationToken, VoiceBackendError};
    use crate::youtube::YouTubeUrl;

    #[test]
    fn songbird_config_uses_aead_aes256_gcm_transport_alongside_dave() {
        let config = build_songbird_config();
        assert_eq!(config.crypto_mode, CryptoMode::Aes256Gcm);
    }

    #[test]
    fn argument_builders_use_safe_argument_arrays_without_disk_downloads() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();

        let meta_args = build_yt_dlp_metadata_args(&url);
        assert!(meta_args.contains(&"--no-playlist".to_string()));
        assert!(meta_args.contains(&"--skip-download".to_string()));
        assert!(meta_args.contains(&"--dump-single-json".to_string()));
        assert_eq!(meta_args[meta_args.len() - 2], "--");
        assert_eq!(meta_args[meta_args.len() - 1], url.as_str());

        let stream_args = build_yt_dlp_stream_args(&url);
        assert!(stream_args.contains(&"--no-part".to_string()));
        assert!(stream_args.contains(&"--no-cache-dir".to_string()));
        assert!(stream_args.windows(2).any(|pair| pair == ["-o", "-"]));
        assert_eq!(stream_args[stream_args.len() - 2], "--");
        assert_eq!(stream_args[stream_args.len() - 1], url.as_str());

        let ffmpeg_args = build_ffmpeg_pcm_args();
        assert!(ffmpeg_args.windows(2).any(|pair| pair == ["-i", "pipe:0"]));
        assert!(ffmpeg_args.windows(2).any(|pair| pair == ["-f", "f32le"]));
        assert!(ffmpeg_args.windows(2).any(|pair| pair == ["-ar", "48000"]));
        assert!(ffmpeg_args.windows(2).any(|pair| pair == ["-ac", "2"]));
        assert_eq!(ffmpeg_args.last().map(String::as_str), Some("pipe:1"));
    }

    #[test]
    fn parses_yt_dlp_metadata_json_with_channel_fallback_and_duration() {
        let url = YouTubeUrl::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();

        let primary = br#"{"title":"  Never Gonna Give You Up ","channel":" Rick Astley ","uploader":"Fallback","duration":212.6}"#;
        let parsed = parse_yt_dlp_metadata(&url, primary).unwrap();
        assert_eq!(parsed.url, url);
        assert_eq!(parsed.title.as_deref(), Some("Never Gonna Give You Up"));
        assert_eq!(parsed.channel.as_deref(), Some("Rick Astley"));
        assert_eq!(parsed.duration_seconds, Some(213));

        let fallback =
            br#"{"title":"Song","channel":"   ","uploader":"Uploader Name","duration":null}"#;
        let parsed_fallback = parse_yt_dlp_metadata(&url, fallback).unwrap();
        assert_eq!(parsed_fallback.channel.as_deref(), Some("Uploader Name"));
        assert_eq!(parsed_fallback.duration_seconds, None);

        let invalid = parse_yt_dlp_metadata(&url, b"not-json").unwrap_err();
        assert!(matches!(invalid, VoiceBackendError::ResolveFailed(_)));
    }

    #[tokio::test]
    async fn raw_adapter_pcm_f32le_stream_is_playable_with_songbird_codecs() {
        let sample_count = (DISCORD_SAMPLE_RATE as usize / 50) * DISCORD_CHANNELS as usize;
        let pcm_bytes = vec![0u8; sample_count * std::mem::size_of::<f32>()];
        let adapter = RawAdapter::new(
            Cursor::new(pcm_bytes),
            DISCORD_SAMPLE_RATE,
            DISCORD_CHANNELS,
        );
        let input = Input::from(adapter);

        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("RawAdapter PCM stream should be playable");
        assert!(playable.is_playable());
    }

    #[cfg(windows)]
    fn is_windows_pid_alive(pid: u32) -> bool {
        let output = StdCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("query tasklist for child PID");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.contains(&format!("\"{pid}\""))
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn streaming_pipeline_explicit_termination_leaves_no_orphaned_processes() {
        let sleep_args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 30".to_string(),
        ];

        let (input, controller) = spawn_streaming_pipeline_with_commands(
            "powershell",
            &sleep_args,
            "powershell",
            &sleep_args,
        )
        .await
        .unwrap();

        let (yt_pid, ff_pid) = controller.child_pids();
        let yt_pid = yt_pid.expect("upstream child pid");
        let ff_pid = ff_pid.expect("downstream child pid");
        assert!(is_windows_pid_alive(yt_pid));
        assert!(is_windows_pid_alive(ff_pid));

        controller.terminate_async().await;
        drop(input);

        assert!(!controller.is_active());
        assert!(!is_windows_pid_alive(yt_pid));
        assert!(!is_windows_pid_alive(ff_pid));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_pipeline_kills_child_processes_without_explicit_stop() {
        let sleep_args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 30".to_string(),
        ];

        let (input, controller) = spawn_streaming_pipeline_with_commands(
            "powershell",
            &sleep_args,
            "powershell",
            &sleep_args,
        )
        .await
        .unwrap();

        let (yt_pid, ff_pid) = controller.child_pids();
        let yt_pid = yt_pid.expect("upstream child pid");
        let ff_pid = ff_pid.expect("downstream child pid");
        assert!(is_windows_pid_alive(yt_pid));
        assert!(is_windows_pid_alive(ff_pid));

        drop(input);
        drop(controller);

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!is_windows_pid_alive(yt_pid));
        assert!(!is_windows_pid_alive(ff_pid));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_metadata_resolution_kills_child_process_immediately() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel_trigger.cancel();
        });

        let args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 30".to_string(),
        ];

        let started = tokio::time::Instant::now();
        let result =
            resolve_track_with_command("powershell", &args, &url, cancel, Duration::from_secs(10))
                .await;

        assert_eq!(result, Err(VoiceBackendError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
