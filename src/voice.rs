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
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeek, BufReader, ReadBuf,
};
use tokio::process::{Child, ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::core::{
    CancellationToken, PlaybackGeneration, ResolvedTrack, VoiceBackend, VoiceBackendError,
    VoiceLifecycleListener,
};
use crate::state::{GuildId, TrackMetadata, VoiceChannelId, VoiceTarget};
use crate::youtube::YouTubeUrl;

pub const DISCORD_SAMPLE_RATE: u32 = 48_000;
pub const DISCORD_CHANNELS: u32 = 2;
pub const STREAM_RING_BUFFER_BYTES: usize = 128 * 1024;
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_METADATA_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_STDERR_OUTPUT_BYTES: usize = 64 * 1024;
pub const YT_DLP_METADATA_PREFIX: &str = "ZEBOT_META:";
pub const YT_DLP_METADATA_PRINT_TEMPLATE: &str =
    "before_dl:ZEBOT_META:%(.{title,channel,uploader,duration})j";

pub fn build_songbird_config() -> SongbirdConfig {
    SongbirdConfig::default().crypto_mode(CryptoMode::Aes256Gcm)
}

pub fn build_yt_dlp_stream_args(url: &YouTubeUrl) -> Vec<String> {
    vec![
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--no-part".to_string(),
        "--no-cache-dir".to_string(),
        "--no-progress".to_string(),
        "--no-simulate".to_string(),
        "--print".to_string(),
        YT_DLP_METADATA_PRINT_TEMPLATE.to_string(),
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

pub fn parse_yt_dlp_metadata_line(
    url: &YouTubeUrl,
    raw_line: &[u8],
) -> Option<Result<TrackMetadata, VoiceBackendError>> {
    let trimmed = trim_line_endings(raw_line);
    let payload = trimmed.strip_prefix(YT_DLP_METADATA_PREFIX.as_bytes())?;
    Some(parse_yt_dlp_metadata(url, payload))
}

fn trim_line_endings(mut line: &[u8]) -> &[u8] {
    while let Some(&last) = line.last() {
        if last == b'\n' || last == b'\r' {
            line = &line[..line.len() - 1];
        } else {
            break;
        }
    }
    line
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

fn append_bounded_diagnostic_line(buffer: &mut Vec<u8>, line: &[u8], max_bytes: usize) {
    let trimmed = trim_line_endings(line);
    if trimmed.is_empty() {
        return;
    }

    let remaining = max_bytes.saturating_sub(buffer.len());
    if remaining == 0 {
        return;
    }

    let take = trimmed.len().min(remaining);
    buffer.extend_from_slice(&trimmed[..take]);
    if buffer.len() < max_bytes {
        buffer.push(b'\n');
    }
}

enum BoundedLine {
    Eof,
    Line(Vec<u8>),
    TooLong,
}

async fn read_bounded_line<R>(reader: &mut R, max_bytes: usize) -> io::Result<BoundedLine>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            return Ok(BoundedLine::Line(line));
        }

        if let Some(newline_index) = available.iter().position(|&byte| byte == b'\n') {
            let segment = &available[..=newline_index];
            let consumed = newline_index + 1;
            if line.len().saturating_add(segment.len()) > max_bytes {
                reader.consume(consumed);
                return Ok(BoundedLine::TooLong);
            }
            line.extend_from_slice(segment);
            reader.consume(consumed);
            return Ok(BoundedLine::Line(line));
        }

        let chunk_len = available.len();
        if line.len().saturating_add(chunk_len) > max_bytes {
            reader.consume(chunk_len);
            return Ok(BoundedLine::TooLong);
        }
        line.extend_from_slice(available);
        reader.consume(chunk_len);
    }
}

async fn drain_reader_to_eof<R>(mut reader: R)
where
    R: AsyncRead + Unpin,
{
    let mut sink = [0u8; 4096];
    loop {
        match reader.read(&mut sink).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[cfg(windows)]
async fn taskkill_process_tree_async(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(windows)]
fn taskkill_process_tree_blocking(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

async fn terminate_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }

    #[cfg(windows)]
    if let Some(pid) = child.id() {
        taskkill_process_tree_async(pid).await;
    }

    let _ = child.kill().await;
}

fn terminate_child_now(mut child: Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }

    #[cfg(windows)]
    let pid = child.id();

    #[cfg(not(windows))]
    let _ = child.start_kill();

    if let Ok(handle) = Handle::try_current() {
        handle.spawn(async move {
            #[cfg(windows)]
            if let Some(pid) = pid {
                taskkill_process_tree_async(pid).await;
            }
            let _ = child.kill().await;
        });
    } else {
        #[cfg(windows)]
        if let Some(pid) = pid {
            taskkill_process_tree_blocking(pid);
        }
        let _ = child.start_kill();
    }
}

struct PreparedYtDlpInner {
    yt_dlp: Child,
    yt_stdout: ChildStdout,
    stderr_drain: JoinHandle<()>,
}

pub struct PreparedYtDlpStream {
    inner: Option<PreparedYtDlpInner>,
}

impl PreparedYtDlpStream {
    fn new(yt_dlp: Child, yt_stdout: ChildStdout, stderr_drain: JoinHandle<()>) -> Self {
        Self {
            inner: Some(PreparedYtDlpInner {
                yt_dlp,
                yt_stdout,
                stderr_drain,
            }),
        }
    }

    pub fn yt_dlp_pid(&self) -> Option<u32> {
        self.inner.as_ref().and_then(|inner| inner.yt_dlp.id())
    }

    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    pub async fn terminate_async(mut self) {
        if let Some(mut inner) = self.inner.take() {
            inner.stderr_drain.abort();
            drop(inner.yt_stdout);
            terminate_child(&mut inner.yt_dlp).await;
        }
    }

    fn terminate_now(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.stderr_drain.abort();
            drop(inner.yt_stdout);
            terminate_child_now(inner.yt_dlp);
        }
    }
}

impl Drop for PreparedYtDlpStream {
    fn drop(&mut self) {
        self.terminate_now();
    }
}

pub async fn prepare_yt_dlp_stream_with_command(
    program: &str,
    args: &[String],
    url: &YouTubeUrl,
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<ResolvedTrack<PreparedYtDlpStream>, VoiceBackendError> {
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
        drop(stdout);
        terminate_child(&mut child).await;
        return Err(VoiceBackendError::ResolveFailed(
            "yt-dlp stderr pipe was unavailable".to_string(),
        ));
    };

    let mut stderr_reader = BufReader::new(stderr);
    let mut stderr_diagnostics = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    enum LineWaitOutcome {
        Read(io::Result<BoundedLine>),
        Cancelled,
        TimedOut,
    }

    enum ExitWaitOutcome {
        Exited(io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }

    loop {
        let outcome = tokio::select! {
            line = read_bounded_line(&mut stderr_reader, MAX_METADATA_OUTPUT_BYTES) => LineWaitOutcome::Read(line),
            () = cancel.cancelled() => LineWaitOutcome::Cancelled,
            () = tokio::time::sleep_until(deadline) => LineWaitOutcome::TimedOut,
        };

        match outcome {
            LineWaitOutcome::Cancelled => {
                drop(stdout);
                drop(stderr_reader);
                terminate_child(&mut child).await;
                return Err(VoiceBackendError::Cancelled);
            }
            LineWaitOutcome::TimedOut => {
                drop(stdout);
                drop(stderr_reader);
                terminate_child(&mut child).await;
                return Err(VoiceBackendError::ResolveFailed(
                    "yt-dlp timed out while resolving YouTube metadata".to_string(),
                ));
            }
            LineWaitOutcome::Read(Err(error)) => {
                drop(stdout);
                drop(stderr_reader);
                terminate_child(&mut child).await;
                return Err(VoiceBackendError::ResolveFailed(format!(
                    "failed reading yt-dlp stderr: {error}"
                )));
            }
            LineWaitOutcome::Read(Ok(BoundedLine::TooLong)) => {
                drop(stdout);
                drop(stderr_reader);
                terminate_child(&mut child).await;
                return Err(VoiceBackendError::ResolveFailed(
                    "yt-dlp metadata output exceeded maximum allowed size".to_string(),
                ));
            }
            LineWaitOutcome::Read(Ok(BoundedLine::Line(raw_line))) => {
                if let Some(parsed_result) = parse_yt_dlp_metadata_line(url, &raw_line) {
                    let metadata = match parsed_result {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            drop(stdout);
                            drop(stderr_reader);
                            terminate_child(&mut child).await;
                            return Err(error);
                        }
                    };

                    if cancel.is_cancelled() {
                        drop(stdout);
                        drop(stderr_reader);
                        terminate_child(&mut child).await;
                        return Err(VoiceBackendError::Cancelled);
                    }

                    let stderr_drain = tokio::spawn(async move {
                        drain_reader_to_eof(stderr_reader).await;
                    });
                    let prepared = PreparedYtDlpStream::new(child, stdout, stderr_drain);
                    return Ok(ResolvedTrack::new(metadata, prepared));
                }

                append_bounded_diagnostic_line(
                    &mut stderr_diagnostics,
                    &raw_line,
                    MAX_STDERR_OUTPUT_BYTES,
                );
            }
            LineWaitOutcome::Read(Ok(BoundedLine::Eof)) => {
                drop(stdout);
                drop(stderr_reader);

                let exit_outcome = tokio::select! {
                    status = child.wait() => ExitWaitOutcome::Exited(status),
                    () = cancel.cancelled() => ExitWaitOutcome::Cancelled,
                    () = tokio::time::sleep_until(deadline) => ExitWaitOutcome::TimedOut,
                };

                match exit_outcome {
                    ExitWaitOutcome::Cancelled => {
                        terminate_child(&mut child).await;
                        return Err(VoiceBackendError::Cancelled);
                    }
                    ExitWaitOutcome::TimedOut => {
                        terminate_child(&mut child).await;
                        return Err(VoiceBackendError::ResolveFailed(
                            "yt-dlp timed out while resolving YouTube metadata".to_string(),
                        ));
                    }
                    ExitWaitOutcome::Exited(status_result) => {
                        let status = status_result.map_err(|error| {
                            VoiceBackendError::ResolveFailed(format!(
                                "failed while waiting for yt-dlp: {error}"
                            ))
                        })?;

                        if cancel.is_cancelled() {
                            return Err(VoiceBackendError::Cancelled);
                        }

                        if !status.success() {
                            let detail = first_non_empty_line(&stderr_diagnostics)
                                .unwrap_or_else(|| format!("exit status {status}"));
                            return Err(VoiceBackendError::ResolveFailed(detail));
                        }

                        return Err(VoiceBackendError::ResolveFailed(
                            "yt-dlp exited without emitting track metadata".to_string(),
                        ));
                    }
                }
            }
        }
    }
}

struct PipelineChildren {
    yt_dlp: Child,
    ffmpeg: Child,
    stderr_drain: Option<JoinHandle<()>>,
}

pub struct PipelineController {
    children: Mutex<Option<PipelineChildren>>,
}

impl PipelineController {
    fn new(yt_dlp: Child, ffmpeg: Child, stderr_drain: Option<JoinHandle<()>>) -> Arc<Self> {
        Arc::new(Self {
            children: Mutex::new(Some(PipelineChildren {
                yt_dlp,
                ffmpeg,
                stderr_drain,
            })),
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
            if let Some(drain) = children.stderr_drain.take() {
                drain.abort();
            }
            terminate_child(&mut children.ffmpeg).await;
            terminate_child(&mut children.yt_dlp).await;
        }
    }

    pub fn terminate_now(&self) {
        let taken = self.lock_children().take();
        if let Some(mut children) = taken {
            if let Some(drain) = children.stderr_drain.take() {
                drain.abort();
            }
            terminate_child_now(children.ffmpeg);
            terminate_child_now(children.yt_dlp);
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

pub async fn attach_ffmpeg_to_prepared_stream(
    mut prepared: PreparedYtDlpStream,
    ffmpeg_program: &str,
    ffmpeg_args: &[String],
) -> Result<(Input, Arc<PipelineController>), VoiceBackendError> {
    let Some(mut inner) = prepared.inner.take() else {
        return Err(VoiceBackendError::PlaybackFailed(
            "prepared yt-dlp stream was already consumed or terminated".to_string(),
        ));
    };

    let yt_stdout_stdio: Stdio = match inner.yt_stdout.try_into() {
        Ok(stdio) => stdio,
        Err(error) => {
            inner.stderr_drain.abort();
            terminate_child(&mut inner.yt_dlp).await;
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
            inner.stderr_drain.abort();
            terminate_child(&mut inner.yt_dlp).await;
            return Err(VoiceBackendError::PlaybackFailed(format!(
                "failed to start ffmpeg transcoder: {error}"
            )));
        }
    };

    let Some(ffmpeg_stdout) = ffmpeg.stdout.take() else {
        inner.stderr_drain.abort();
        terminate_child(&mut ffmpeg).await;
        terminate_child(&mut inner.yt_dlp).await;
        return Err(VoiceBackendError::PlaybackFailed(
            "ffmpeg stdout pipe was unavailable".to_string(),
        ));
    };

    let controller = PipelineController::new(inner.yt_dlp, ffmpeg, Some(inner.stderr_drain));
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
    type PreparedStream = PreparedYtDlpStream;

    async fn resolve_track(
        &self,
        url: &YouTubeUrl,
        cancel: CancellationToken,
    ) -> Result<ResolvedTrack<Self::PreparedStream>, VoiceBackendError> {
        let args = build_yt_dlp_stream_args(url);
        prepare_yt_dlp_stream_with_command(&self.yt_dlp_path, &args, url, cancel, RESOLVE_TIMEOUT)
            .await
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
        track: ResolvedTrack<Self::PreparedStream>,
        generation: PlaybackGeneration,
        cancel: CancellationToken,
    ) -> Result<(), VoiceBackendError> {
        let prepared = track.prepared;
        if cancel.is_cancelled() {
            prepared.terminate_async().await;
            return Err(VoiceBackendError::Cancelled);
        }

        let Some(sg_guild) = to_songbird_guild_id(target.guild_id) else {
            prepared.terminate_async().await;
            return Err(VoiceBackendError::PlaybackFailed(
                "invalid guild ID".to_string(),
            ));
        };

        self.stop_guild_playback(target.guild_id).await;

        if cancel.is_cancelled() {
            prepared.terminate_async().await;
            return Err(VoiceBackendError::Cancelled);
        }

        let Some(call) = self.songbird.get(sg_guild) else {
            prepared.terminate_async().await;
            return Err(VoiceBackendError::PlaybackFailed(
                "bot is not connected to a Discord voice channel".to_string(),
            ));
        };

        let ffmpeg_args = build_ffmpeg_pcm_args();
        let (input, controller) =
            attach_ffmpeg_to_prepared_stream(prepared, &self.ffmpeg_path, &ffmpeg_args).await?;

        if cancel.is_cancelled() {
            controller.terminate_async().await;
            return Err(VoiceBackendError::Cancelled);
        }

        let track_handle = {
            let mut handler = call.lock().await;
            if cancel.is_cancelled() {
                drop(handler);
                controller.terminate_async().await;
                return Err(VoiceBackendError::Cancelled);
            }
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

    async fn discard_prepared(&self, prepared: Self::PreparedStream) {
        prepared.terminate_async().await;
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
        DISCORD_CHANNELS, DISCORD_SAMPLE_RATE, YT_DLP_METADATA_PREFIX,
        YT_DLP_METADATA_PRINT_TEMPLATE, attach_ffmpeg_to_prepared_stream, build_ffmpeg_pcm_args,
        build_songbird_config, build_yt_dlp_stream_args, parse_yt_dlp_metadata,
        parse_yt_dlp_metadata_line, prepare_yt_dlp_stream_with_command,
    };
    use crate::core::{CancellationToken, VoiceBackendError};
    use crate::youtube::YouTubeUrl;

    #[test]
    fn songbird_config_uses_aead_aes256_gcm_transport_alongside_dave() {
        let config = build_songbird_config();
        assert_eq!(config.crypto_mode, CryptoMode::Aes256Gcm);
    }

    #[test]
    fn argument_builders_use_safe_single_invocation_args_without_disk_downloads() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();

        let stream_args = build_yt_dlp_stream_args(&url);
        assert!(stream_args.contains(&"--no-playlist".to_string()));
        assert!(stream_args.contains(&"--no-warnings".to_string()));
        assert!(stream_args.contains(&"--no-part".to_string()));
        assert!(stream_args.contains(&"--no-cache-dir".to_string()));
        assert!(stream_args.contains(&"--no-progress".to_string()));
        assert!(stream_args.contains(&"--no-simulate".to_string()));
        assert!(!stream_args.contains(&"--skip-download".to_string()));
        assert!(
            stream_args
                .windows(2)
                .any(|pair| pair == ["--print", YT_DLP_METADATA_PRINT_TEMPLATE])
        );
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

    #[test]
    fn parses_prefixed_metadata_line_and_ignores_unrelated_stderr_lines() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();

        assert!(parse_yt_dlp_metadata_line(&url, b"[youtube] Extracting URL\r\n").is_none());
        assert!(parse_yt_dlp_metadata_line(&url, b"ERROR: temporary glitch\n").is_none());

        let valid_line = format!(
            "{YT_DLP_METADATA_PREFIX}{{\"title\":\"Demo Track\",\"channel\":\"Demo Channel\",\"uploader\":\"Fallback\",\"duration\":42}}\r\n"
        );
        let metadata = parse_yt_dlp_metadata_line(&url, valid_line.as_bytes())
            .expect("prefixed line should match")
            .unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Demo Track"));
        assert_eq!(metadata.channel.as_deref(), Some("Demo Channel"));
        assert_eq!(metadata.duration_seconds, Some(42));

        let malformed_line = format!("{YT_DLP_METADATA_PREFIX}{{not-json}}\n");
        let error = parse_yt_dlp_metadata_line(&url, malformed_line.as_bytes())
            .expect("prefixed malformed line should be parsed")
            .unwrap_err();
        assert!(matches!(error, VoiceBackendError::ResolveFailed(_)));
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

        let mut playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("RawAdapter PCM stream should be playable");
        assert!(playable.is_playable());

        let parsed = playable
            .parsed_mut()
            .expect("playable input should expose parsed format and decoder");
        let packet = parsed
            .format
            .next_packet()
            .expect("RawReader should yield a 20ms PCM packet");
        let decoded = parsed
            .decoder
            .decode(&packet)
            .expect("Symphonia PCM decoder should decode RawReader packet");
        assert_eq!(decoded.frames(), DISCORD_SAMPLE_RATE as usize / 50);
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
    fn powershell_command_args(script: &str) -> Vec<String> {
        vec![
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script.to_string(),
        ]
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn single_prepared_stream_yields_metadata_and_attaches_to_ffmpeg_cleanly() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let upstream_args = powershell_command_args(
            "[Console]::Error.WriteLine('diagnostic noise before metadata'); \
             [Console]::Error.WriteLine('ZEBOT_META:{\"title\":\"Single Pass Song\",\"channel\":\"Zebot Channel\",\"uploader\":\"Fallback\",\"duration\":95}'); \
             [Console]::Error.Flush(); \
             Start-Sleep -Seconds 30",
        );
        let downstream_args = powershell_command_args("Start-Sleep -Seconds 30");

        let resolved = prepare_yt_dlp_stream_with_command(
            "powershell",
            &upstream_args,
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        assert_eq!(resolved.metadata.title.as_deref(), Some("Single Pass Song"));
        assert_eq!(resolved.metadata.channel.as_deref(), Some("Zebot Channel"));
        assert_eq!(resolved.metadata.duration_seconds, Some(95));
        assert!(resolved.prepared.is_active());

        let original_yt_pid = resolved
            .prepared
            .yt_dlp_pid()
            .expect("prepared stream should expose upstream PID");
        assert!(is_windows_pid_alive(original_yt_pid));

        let (input, controller) =
            attach_ffmpeg_to_prepared_stream(resolved.prepared, "powershell", &downstream_args)
                .await
                .unwrap();

        let (yt_pid, ff_pid) = controller.child_pids();
        assert_eq!(yt_pid, Some(original_yt_pid));
        let ff_pid = ff_pid.expect("downstream child pid");
        assert!(is_windows_pid_alive(original_yt_pid));
        assert!(is_windows_pid_alive(ff_pid));

        controller.terminate_async().await;
        drop(input);

        assert!(!controller.is_active());
        assert!(!is_windows_pid_alive(original_yt_pid));
        assert!(!is_windows_pid_alive(ff_pid));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_prepared_stream_kills_upstream_process_without_attaching_ffmpeg() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let upstream_args = powershell_command_args(
            "[Console]::Error.WriteLine('ZEBOT_META:{\"title\":\"Aborted Track\",\"channel\":\"Chan\",\"duration\":12}'); \
             [Console]::Error.Flush(); \
             Start-Sleep -Seconds 30",
        );

        let resolved = prepare_yt_dlp_stream_with_command(
            "powershell",
            &upstream_args,
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        let yt_pid = resolved
            .prepared
            .yt_dlp_pid()
            .expect("prepared stream should expose upstream PID");
        assert!(is_windows_pid_alive(yt_pid));

        drop(resolved);

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!is_windows_pid_alive(yt_pid));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_attached_pipeline_kills_both_processes_without_explicit_stop() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let upstream_args = powershell_command_args(
            "[Console]::Error.WriteLine('ZEBOT_META:{\"title\":\"Drop Test\",\"channel\":\"Chan\",\"duration\":30}'); \
             [Console]::Error.Flush(); \
             Start-Sleep -Seconds 30",
        );
        let downstream_args = powershell_command_args("Start-Sleep -Seconds 30");

        let resolved = prepare_yt_dlp_stream_with_command(
            "powershell",
            &upstream_args,
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        let (input, controller) =
            attach_ffmpeg_to_prepared_stream(resolved.prepared, "powershell", &downstream_args)
                .await
                .unwrap();

        let (yt_pid, ff_pid) = controller.child_pids();
        let yt_pid = yt_pid.expect("upstream child pid");
        let ff_pid = ff_pid.expect("downstream child pid");
        assert!(is_windows_pid_alive(yt_pid));
        assert!(is_windows_pid_alive(ff_pid));

        drop(input);
        drop(controller);

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!is_windows_pid_alive(yt_pid));
        assert!(!is_windows_pid_alive(ff_pid));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_stream_preparation_kills_process_tree_immediately() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();

        let pid_file = std::path::PathBuf::from("temp")
            .join(format!("zebot-grandchild-pid-{}.txt", std::process::id()));
        let _ = std::fs::create_dir_all("temp");
        let _ = std::fs::remove_file(&pid_file);

        let watcher_pid_file = pid_file.clone();
        let watcher_task = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
            let mut found_pid = None;
            while tokio::time::Instant::now() < deadline {
                if let Ok(raw) = std::fs::read_to_string(&watcher_pid_file)
                    && let Ok(parsed) = raw.trim().parse::<u32>()
                {
                    found_pid = Some(parsed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            cancel_trigger.cancel();
            found_pid
        });

        let script = format!(
            "$child = Start-Process -FilePath 'powershell' -ArgumentList '-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30' -WindowStyle Hidden -PassThru; \
             [System.IO.File]::WriteAllText('{}', [string]$child.Id); \
             Start-Sleep -Seconds 30",
            pid_file.display()
        );
        let args = powershell_command_args(&script);

        let started = tokio::time::Instant::now();
        let result = prepare_yt_dlp_stream_with_command(
            "powershell",
            &args,
            &url,
            cancel,
            Duration::from_secs(10),
        )
        .await;

        assert!(matches!(result, Err(VoiceBackendError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(9));

        let grandchild_pid = watcher_task
            .await
            .expect("watcher task should complete")
            .expect("grandchild process should have written its PID before cancellation");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!is_windows_pid_alive(grandchild_pid));
        let _ = std::fs::remove_file(&pid_file);
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attached_pipeline_streams_upstream_stdout_bytes_into_playable_songbird_input() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let frame_bytes = (DISCORD_SAMPLE_RATE as usize / 50)
            * DISCORD_CHANNELS as usize
            * std::mem::size_of::<f32>();

        let upstream_script = format!(
            "[Console]::Error.WriteLine('ZEBOT_META:{{\"title\":\"Pipe Audio\",\"channel\":\"Pipe Channel\",\"duration\":1}}'); \
             [Console]::Error.Flush(); \
             $bytes = New-Object byte[] {frame_bytes}; \
             $stdout = [Console]::OpenStandardOutput(); \
             $stdout.Write($bytes, 0, $bytes.Length); \
             $stdout.Flush()"
        );
        let downstream_script = concat!(
            "$stdin = [Console]::OpenStandardInput(); ",
            "$stdout = [Console]::OpenStandardOutput(); ",
            "$stdin.CopyTo($stdout); ",
            "$stdout.Flush()"
        );
        let downstream_args = vec![
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-InputFormat".to_string(),
            "None".to_string(),
            "-Command".to_string(),
            downstream_script.to_string(),
        ];

        let resolved = prepare_yt_dlp_stream_with_command(
            "powershell",
            &powershell_command_args(&upstream_script),
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        assert_eq!(resolved.metadata.title.as_deref(), Some("Pipe Audio"));
        assert_eq!(resolved.metadata.channel.as_deref(), Some("Pipe Channel"));
        assert_eq!(resolved.metadata.duration_seconds, Some(1));

        let (input, controller) =
            attach_ffmpeg_to_prepared_stream(resolved.prepared, "powershell", &downstream_args)
                .await
                .unwrap();

        let mut playable = tokio::time::timeout(
            Duration::from_secs(10),
            input.make_playable_async(get_codec_registry(), get_probe()),
        )
        .await
        .expect("make_playable_async should not time out")
        .expect("attached pipeline PCM output should be playable");
        assert!(playable.is_playable());

        let decoded_frames = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let parsed = playable
                    .parsed_mut()
                    .expect("playable input should expose parsed format and decoder");
                let packet = parsed
                    .format
                    .next_packet()
                    .expect("attached pipeline should yield a 20ms PCM packet");
                let decoded = parsed
                    .decoder
                    .decode(&packet)
                    .expect("Symphonia PCM decoder should decode piped PCM packet");
                decoded.frames()
            }),
        )
        .await
        .expect("decoding piped PCM packet should not time out")
        .expect("blocking decode task should succeed");
        assert_eq!(decoded_frames, DISCORD_SAMPLE_RATE as usize / 50);

        controller.terminate_async().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn stream_preparation_reports_stderr_error_when_process_fails_before_metadata() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let args = powershell_command_args(
            "[Console]::Error.WriteLine('ERROR: [youtube] dQw4w9WgXcQ: Video unavailable'); exit 1",
        );

        let error = match prepare_yt_dlp_stream_with_command(
            "powershell",
            &args,
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        {
            Ok(_) => unreachable!("failing command must not succeed"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            VoiceBackendError::ResolveFailed(
                "ERROR: [youtube] dQw4w9WgXcQ: Video unavailable".to_string()
            )
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn stream_preparation_rejects_clean_exit_without_metadata_and_malformed_json() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();

        let no_meta_error = match prepare_yt_dlp_stream_with_command(
            "powershell",
            &powershell_command_args("exit 0"),
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        {
            Ok(_) => unreachable!("clean exit without metadata must fail"),
            Err(error) => error,
        };
        assert_eq!(
            no_meta_error,
            VoiceBackendError::ResolveFailed(
                "yt-dlp exited without emitting track metadata".to_string()
            )
        );

        let bad_json_args = powershell_command_args(
            "[Console]::Error.WriteLine('ZEBOT_META:not-json'); [Console]::Error.Flush(); Start-Sleep -Seconds 30",
        );
        let started = tokio::time::Instant::now();
        let bad_json_error = match prepare_yt_dlp_stream_with_command(
            "powershell",
            &bad_json_args,
            &url,
            CancellationToken::new(),
            Duration::from_secs(10),
        )
        .await
        {
            Ok(_) => unreachable!("malformed metadata JSON must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            bad_json_error,
            VoiceBackendError::ResolveFailed(_)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn stream_preparation_times_out_and_kills_hanging_process() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let args = powershell_command_args("Start-Sleep -Seconds 30");

        let started = tokio::time::Instant::now();
        let error = match prepare_yt_dlp_stream_with_command(
            "powershell",
            &args,
            &url,
            CancellationToken::new(),
            Duration::from_millis(300),
        )
        .await
        {
            Ok(_) => unreachable!("hanging command must time out"),
            Err(error) => error,
        };

        assert!(matches!(error, VoiceBackendError::ResolveFailed(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
