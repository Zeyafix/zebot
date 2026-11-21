use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::state::{
    BotStatus, DiscordConnectionState, GuildId, PlayRequest, PlaybackState, TrackMetadata,
    VoiceTarget,
};
use crate::youtube::{YouTubeUrl, YouTubeUrlError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaybackGeneration(u64);

impl PlaybackGeneration {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PlaybackGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}

#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        let mut notified = pin!(self.inner.notify.notified());
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceBackendError {
    Cancelled,
    ResolveFailed(String),
    JoinFailed(String),
    PlaybackFailed(String),
    StopFailed(String),
    LeaveFailed(String),
}

impl fmt::Display for VoiceBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "voice operation was cancelled"),
            Self::ResolveFailed(reason) => write!(f, "failed to resolve track: {reason}"),
            Self::JoinFailed(reason) => write!(f, "failed to join voice channel: {reason}"),
            Self::PlaybackFailed(reason) => write!(f, "failed to start playback: {reason}"),
            Self::StopFailed(reason) => write!(f, "failed to stop playback: {reason}"),
            Self::LeaveFailed(reason) => write!(f, "failed to leave voice channel: {reason}"),
        }
    }
}

impl Error for VoiceBackendError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    InvalidUrl(YouTubeUrlError),
    Cancelled,
    Backend(VoiceBackendError),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(f, "invalid YouTube URL: {error}"),
            Self::Cancelled => write!(f, "operation was cancelled by a newer request"),
            Self::Backend(error) => write!(f, "voice backend error: {error}"),
        }
    }
}

impl Error for CoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::Cancelled => None,
            Self::Backend(error) => Some(error),
        }
    }
}

impl From<YouTubeUrlError> for CoreError {
    fn from(value: YouTubeUrlError) -> Self {
        Self::InvalidUrl(value)
    }
}

pub trait VoiceBackend: Send + Sync {
    fn resolve_track(
        &self,
        url: &YouTubeUrl,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<TrackMetadata, VoiceBackendError>> + Send;

    fn join(
        &self,
        target: VoiceTarget,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send;

    fn play(
        &self,
        target: VoiceTarget,
        track: &TrackMetadata,
        generation: PlaybackGeneration,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send;

    fn stop(&self, guild_id: GuildId)
    -> impl Future<Output = Result<(), VoiceBackendError>> + Send;

    fn leave(
        &self,
        guild_id: GuildId,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send;
}

impl<B: VoiceBackend> VoiceBackend for Arc<B> {
    fn resolve_track(
        &self,
        url: &YouTubeUrl,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<TrackMetadata, VoiceBackendError>> + Send {
        (**self).resolve_track(url, cancel)
    }

    fn join(
        &self,
        target: VoiceTarget,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send {
        (**self).join(target, cancel)
    }

    fn play(
        &self,
        target: VoiceTarget,
        track: &TrackMetadata,
        generation: PlaybackGeneration,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send {
        (**self).play(target, track, generation, cancel)
    }

    fn stop(
        &self,
        guild_id: GuildId,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send {
        (**self).stop(guild_id)
    }

    fn leave(
        &self,
        guild_id: GuildId,
    ) -> impl Future<Output = Result<(), VoiceBackendError>> + Send {
        (**self).leave(guild_id)
    }
}

#[derive(Debug)]
struct CoreState {
    discord_state: DiscordConnectionState,
    voice_target: Option<VoiceTarget>,
    playback_state: PlaybackState,
    active_track: Option<TrackMetadata>,
    backend_playing: bool,
    generation: u64,
    active_cancel: Option<CancellationToken>,
}

impl Default for CoreState {
    fn default() -> Self {
        Self {
            discord_state: DiscordConnectionState::Disconnected,
            voice_target: None,
            playback_state: PlaybackState::Idle,
            active_track: None,
            backend_playing: false,
            generation: 0,
            active_cancel: None,
        }
    }
}

impl CoreState {
    fn snapshot(&self) -> BotStatus {
        BotStatus {
            discord_state: self.discord_state,
            voice_target: self.voice_target,
            playback_state: self.playback_state,
            active_track: self.active_track.clone(),
        }
    }

    fn advance_generation(&mut self, next_cancel: Option<CancellationToken>) -> PlaybackGeneration {
        if let Some(previous) = self.active_cancel.take() {
            previous.cancel();
        }
        self.generation = self.generation.wrapping_add(1);
        self.active_cancel = next_cancel;
        PlaybackGeneration::new(self.generation)
    }

    fn is_current(&self, generation: PlaybackGeneration) -> bool {
        self.generation == generation.get()
    }
}

pub struct AppCore<B> {
    backend: B,
    state: Mutex<CoreState>,
    voice_op_lock: AsyncMutex<()>,
}

impl<B> AppCore<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: Mutex::new(CoreState::default()),
            voice_op_lock: AsyncMutex::new(()),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn status(&self) -> BotStatus {
        self.lock_state().snapshot()
    }

    pub fn set_discord_state(&self, discord_state: DiscordConnectionState) {
        self.lock_state().discord_state = discord_state;
    }

    pub fn notify_track_ended(&self, generation: PlaybackGeneration) {
        let mut state = self.lock_state();
        if state.is_current(generation) && state.playback_state == PlaybackState::Playing {
            state.playback_state = PlaybackState::Idle;
            state.active_track = None;
            state.backend_playing = false;
        }
    }

    pub fn notify_voice_disconnected(&self, guild_id: GuildId) {
        let mut state = self.lock_state();
        if state.voice_target.map(|target| target.guild_id) == Some(guild_id) {
            let _ = state.advance_generation(None);
            state.voice_target = None;
            state.playback_state = PlaybackState::Idle;
            state.active_track = None;
            state.backend_playing = false;
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CoreState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_current_generation(&self, generation: PlaybackGeneration) -> bool {
        self.lock_state().is_current(generation)
    }

    fn clear_loading_if_current(&self, generation: PlaybackGeneration) {
        let mut state = self.lock_state();
        if state.is_current(generation) {
            state.playback_state = PlaybackState::Idle;
            state.active_track = None;
        }
    }
}

impl<B: VoiceBackend> AppCore<B> {
    pub async fn play(&self, request: PlayRequest) -> Result<BotStatus, CoreError> {
        let validated_url = YouTubeUrl::parse(&request.url)?;
        let cancel = CancellationToken::new();

        let (op_gen, should_stop_active) = {
            let mut state = self.lock_state();
            let op_gen = state.advance_generation(Some(cancel.clone()));
            state.playback_state = PlaybackState::Loading;
            state.active_track = Some(TrackMetadata::from_url(validated_url.clone()));
            (op_gen, state.backend_playing)
        };

        if should_stop_active {
            self.stop_active_before_resolve(op_gen).await?;
        }

        let resolved_track = match self
            .backend
            .resolve_track(&validated_url, cancel.clone())
            .await
        {
            Ok(track) => track,
            Err(error) => {
                if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
                    self.clear_loading_if_current(op_gen);
                    return Err(CoreError::Cancelled);
                }
                self.clear_loading_if_current(op_gen);
                return Err(CoreError::Backend(error));
            }
        };

        if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
            self.clear_loading_if_current(op_gen);
            return Err(CoreError::Cancelled);
        }

        let _voice_guard = self.voice_op_lock.lock().await;

        if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
            self.clear_loading_if_current(op_gen);
            return Err(CoreError::Cancelled);
        }

        let (current_voice, backend_playing) = {
            let state = self.lock_state();
            (state.voice_target, state.backend_playing)
        };

        if backend_playing && let Some(active_voice) = current_voice {
            if let Err(error) = self.backend.stop(active_voice.guild_id).await {
                self.clear_loading_if_current(op_gen);
                return Err(CoreError::Backend(error));
            }
            self.lock_state().backend_playing = false;
        }

        if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
            self.clear_loading_if_current(op_gen);
            return Err(CoreError::Cancelled);
        }

        if current_voice != Some(request.target) {
            if let Some(previous_voice) = current_voice
                && previous_voice.guild_id != request.target.guild_id
            {
                if let Err(error) = self.backend.leave(previous_voice.guild_id).await {
                    self.clear_loading_if_current(op_gen);
                    return Err(CoreError::Backend(error));
                }
                self.lock_state().voice_target = None;
            }

            if let Err(error) = self.backend.join(request.target, cancel.clone()).await {
                if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
                    self.clear_loading_if_current(op_gen);
                    return Err(CoreError::Cancelled);
                }
                self.clear_loading_if_current(op_gen);
                return Err(CoreError::Backend(error));
            }

            self.lock_state().voice_target = Some(request.target);

            if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
                self.clear_loading_if_current(op_gen);
                return Err(CoreError::Cancelled);
            }
        }

        match self
            .backend
            .play(request.target, &resolved_track, op_gen, cancel.clone())
            .await
        {
            Ok(()) => {
                let committed_status = {
                    let mut state = self.lock_state();
                    if state.is_current(op_gen) && !cancel.is_cancelled() {
                        state.backend_playing = true;
                        state.playback_state = PlaybackState::Playing;
                        state.active_track = Some(resolved_track);
                        Some(state.snapshot())
                    } else {
                        state.backend_playing = true;
                        None
                    }
                };

                match committed_status {
                    Some(status) => Ok(status),
                    None => {
                        if self.backend.stop(request.target.guild_id).await.is_ok() {
                            self.lock_state().backend_playing = false;
                        }
                        Err(CoreError::Cancelled)
                    }
                }
            }
            Err(error) => {
                if !self.is_current_generation(op_gen) || cancel.is_cancelled() {
                    self.clear_loading_if_current(op_gen);
                    return Err(CoreError::Cancelled);
                }
                self.clear_loading_if_current(op_gen);
                Err(CoreError::Backend(error))
            }
        }
    }

    pub async fn stop(&self) -> Result<BotStatus, CoreError> {
        let op_gen = {
            let mut state = self.lock_state();
            let op_gen = state.advance_generation(None);
            if state.playback_state == PlaybackState::Loading {
                state.playback_state = PlaybackState::Idle;
                state.active_track = None;
            }
            op_gen
        };

        let _voice_guard = self.voice_op_lock.lock().await;

        let guild_to_stop = {
            let state = self.lock_state();
            if !state.is_current(op_gen) {
                return Ok(state.snapshot());
            }
            if state.backend_playing {
                state.voice_target.map(|target| target.guild_id)
            } else {
                None
            }
        };

        if let Some(guild_id) = guild_to_stop {
            self.backend
                .stop(guild_id)
                .await
                .map_err(CoreError::Backend)?;
            self.lock_state().backend_playing = false;
        }

        let mut state = self.lock_state();
        if state.is_current(op_gen) {
            state.playback_state = PlaybackState::Idle;
            state.active_track = None;
        }
        Ok(state.snapshot())
    }

    pub async fn leave(&self) -> Result<BotStatus, CoreError> {
        let op_gen = {
            let mut state = self.lock_state();
            let op_gen = state.advance_generation(None);
            if state.playback_state == PlaybackState::Loading {
                state.playback_state = PlaybackState::Idle;
                state.active_track = None;
            }
            op_gen
        };

        let _voice_guard = self.voice_op_lock.lock().await;

        let (guild_to_leave, should_stop) = {
            let state = self.lock_state();
            if !state.is_current(op_gen) {
                return Ok(state.snapshot());
            }
            (
                state.voice_target.map(|target| target.guild_id),
                state.backend_playing,
            )
        };

        if let Some(guild_id) = guild_to_leave {
            let mut stop_error = None;
            if should_stop {
                match self.backend.stop(guild_id).await {
                    Ok(()) => {
                        self.lock_state().backend_playing = false;
                    }
                    Err(error) => {
                        stop_error = Some(error);
                    }
                }
            }

            if let Err(leave_error) = self.backend.leave(guild_id).await {
                return Err(CoreError::Backend(leave_error));
            }

            let mut state = self.lock_state();
            state.voice_target = None;
            state.backend_playing = false;
            if state.is_current(op_gen) {
                state.playback_state = PlaybackState::Idle;
                state.active_track = None;
            }

            if let Some(error) = stop_error {
                return Err(CoreError::Backend(error));
            }

            return Ok(state.snapshot());
        }

        let mut state = self.lock_state();
        if state.is_current(op_gen) {
            state.playback_state = PlaybackState::Idle;
            state.active_track = None;
        }
        Ok(state.snapshot())
    }

    async fn stop_active_before_resolve(
        &self,
        op_gen: PlaybackGeneration,
    ) -> Result<(), CoreError> {
        let _voice_guard = self.voice_op_lock.lock().await;

        let guild_to_stop = {
            let state = self.lock_state();
            if !state.is_current(op_gen) {
                return Err(CoreError::Cancelled);
            }
            if state.backend_playing {
                state.voice_target.map(|target| target.guild_id)
            } else {
                None
            }
        };

        if let Some(guild_id) = guild_to_stop {
            if let Err(error) = self.backend.stop(guild_id).await {
                self.clear_loading_if_current(op_gen);
                return Err(CoreError::Backend(error));
            }
            self.lock_state().backend_playing = false;
        }

        if !self.is_current_generation(op_gen) {
            return Err(CoreError::Cancelled);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::{
        AppCore, CancellationToken, CoreError, PlaybackGeneration, VoiceBackend, VoiceBackendError,
    };
    use crate::state::{
        DiscordConnectionState, GuildId, PlayRequest, PlaybackState, TrackMetadata, VoiceChannelId,
        VoiceTarget,
    };
    use crate::youtube::{YouTubeUrl, YouTubeUrlError};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BackendEvent {
        ResolveStart(String),
        ResolveCancelled(String),
        Join(VoiceTarget),
        Play {
            target: VoiceTarget,
            video_id: String,
            generation: PlaybackGeneration,
        },
        Stop(GuildId),
        Leave(GuildId),
    }

    #[derive(Clone)]
    struct AsyncGate {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl AsyncGate {
        fn new() -> Self {
            Self {
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
            }
        }
    }

    #[derive(Default)]
    struct MockBackendState {
        events: Vec<BackendEvent>,
        resolve_gates: HashMap<String, AsyncGate>,
        play_gates: HashMap<String, AsyncGate>,
        resolve_errors: HashMap<String, String>,
        join_errors: HashMap<VoiceChannelId, String>,
    }

    #[derive(Default)]
    struct MockVoiceBackend {
        inner: Mutex<MockBackendState>,
    }

    impl MockVoiceBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn lock(&self) -> MutexGuard<'_, MockBackendState> {
            self.inner.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn events(&self) -> Vec<BackendEvent> {
            self.lock().events.clone()
        }

        fn install_resolve_gate(&self, video_id: &str) -> AsyncGate {
            let gate = AsyncGate::new();
            self.lock()
                .resolve_gates
                .insert(video_id.to_string(), gate.clone());
            gate
        }

        fn install_play_gate(&self, video_id: &str) -> AsyncGate {
            let gate = AsyncGate::new();
            self.lock()
                .play_gates
                .insert(video_id.to_string(), gate.clone());
            gate
        }

        fn fail_resolve(&self, video_id: &str, message: &str) {
            self.lock()
                .resolve_errors
                .insert(video_id.to_string(), message.to_string());
        }

        fn fail_join(&self, channel_id: VoiceChannelId, message: &str) {
            self.lock()
                .join_errors
                .insert(channel_id, message.to_string());
        }
    }

    impl VoiceBackend for MockVoiceBackend {
        async fn resolve_track(
            &self,
            url: &YouTubeUrl,
            cancel: CancellationToken,
        ) -> Result<TrackMetadata, VoiceBackendError> {
            let video_id = url.video_id().to_string();
            let (gate, planned_error) = {
                let mut state = self.lock();
                state
                    .events
                    .push(BackendEvent::ResolveStart(video_id.clone()));
                (
                    state.resolve_gates.get(&video_id).cloned(),
                    state.resolve_errors.get(&video_id).cloned(),
                )
            };

            if let Some(gate) = gate {
                gate.started.notify_waiters();
                let released = tokio::select! {
                    () = cancel.cancelled() => false,
                    () = gate.release.notified() => true,
                };
                if !released {
                    self.lock()
                        .events
                        .push(BackendEvent::ResolveCancelled(video_id));
                    return Err(VoiceBackendError::Cancelled);
                }
            }

            if cancel.is_cancelled() {
                self.lock()
                    .events
                    .push(BackendEvent::ResolveCancelled(video_id));
                return Err(VoiceBackendError::Cancelled);
            }

            if let Some(reason) = planned_error {
                return Err(VoiceBackendError::ResolveFailed(reason));
            }

            Ok(TrackMetadata {
                url: url.clone(),
                title: Some(format!("Title for {video_id}")),
                channel: Some("Test Channel".to_string()),
                duration_seconds: Some(180),
            })
        }

        async fn join(
            &self,
            target: VoiceTarget,
            cancel: CancellationToken,
        ) -> Result<(), VoiceBackendError> {
            if cancel.is_cancelled() {
                return Err(VoiceBackendError::Cancelled);
            }

            let planned_error = {
                let mut state = self.lock();
                state.events.push(BackendEvent::Join(target));
                state.join_errors.get(&target.channel_id).cloned()
            };

            if let Some(reason) = planned_error {
                return Err(VoiceBackendError::JoinFailed(reason));
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

            let video_id = track.url.video_id().to_string();
            let gate = {
                let mut state = self.lock();
                state.events.push(BackendEvent::Play {
                    target,
                    video_id: video_id.clone(),
                    generation,
                });
                state.play_gates.get(&video_id).cloned()
            };

            if let Some(gate) = gate {
                gate.started.notify_waiters();
                gate.release.notified().await;
            }

            Ok(())
        }

        async fn stop(&self, guild_id: GuildId) -> Result<(), VoiceBackendError> {
            self.lock().events.push(BackendEvent::Stop(guild_id));
            Ok(())
        }

        async fn leave(&self, guild_id: GuildId) -> Result<(), VoiceBackendError> {
            self.lock().events.push(BackendEvent::Leave(guild_id));
            Ok(())
        }
    }

    fn guild(id: u64) -> GuildId {
        GuildId::new(id).unwrap()
    }

    fn channel(id: u64) -> VoiceChannelId {
        VoiceChannelId::new(id).unwrap()
    }

    #[tokio::test]
    async fn status_reports_initial_and_discord_connection_transitions() {
        let backend = MockVoiceBackend::new();
        let core = AppCore::new(backend);

        let initial = core.status();
        assert_eq!(initial.discord_state, DiscordConnectionState::Disconnected);
        assert_eq!(initial.voice_target, None);
        assert_eq!(initial.playback_state, PlaybackState::Idle);
        assert_eq!(initial.active_track, None);

        core.set_discord_state(DiscordConnectionState::Connecting);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Connecting
        );

        core.set_discord_state(DiscordConnectionState::Connected);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Connected
        );

        core.set_discord_state(DiscordConnectionState::ShuttingDown);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::ShuttingDown
        );

        core.set_discord_state(DiscordConnectionState::Disconnected);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Disconnected
        );
    }

    #[tokio::test]
    async fn play_replaces_active_track_in_same_channel_without_rejoining() {
        let backend = MockVoiceBackend::new();
        let core = AppCore::new(Arc::clone(&backend));
        let target = VoiceTarget::new(guild(1), channel(10));

        let first = core
            .play(PlayRequest::new(
                target.guild_id,
                target.channel_id,
                "https://www.youtube.com/watch?v=firstTrack1",
            ))
            .await
            .unwrap();

        assert_eq!(first.voice_target, Some(target));
        assert_eq!(first.playback_state, PlaybackState::Playing);
        assert_eq!(
            first.active_track.as_ref().map(|t| t.url.video_id()),
            Some("firstTrack1")
        );
        assert_eq!(
            first.active_track.as_ref().and_then(|t| t.title.as_deref()),
            Some("Title for firstTrack1")
        );

        let second = core
            .play(PlayRequest::new(
                target.guild_id,
                target.channel_id,
                "https://youtu.be/secondTrack2",
            ))
            .await
            .unwrap();

        assert_eq!(second.voice_target, Some(target));
        assert_eq!(second.playback_state, PlaybackState::Playing);
        assert_eq!(
            second.active_track.as_ref().map(|t| t.url.video_id()),
            Some("secondTrack2")
        );
        assert_eq!(core.status(), second);

        assert_eq!(
            backend.events(),
            vec![
                BackendEvent::ResolveStart("firstTrack1".to_string()),
                BackendEvent::Join(target),
                BackendEvent::Play {
                    target,
                    video_id: "firstTrack1".to_string(),
                    generation: PlaybackGeneration::new(1),
                },
                BackendEvent::Stop(target.guild_id),
                BackendEvent::ResolveStart("secondTrack2".to_string()),
                BackendEvent::Play {
                    target,
                    video_id: "secondTrack2".to_string(),
                    generation: PlaybackGeneration::new(2),
                },
            ]
        );
    }

    #[tokio::test]
    async fn play_moves_from_voice_channel_a_to_voice_channel_b() {
        let backend = MockVoiceBackend::new();
        let core = AppCore::new(Arc::clone(&backend));
        let guild_id = guild(42);
        let channel_a = VoiceTarget::new(guild_id, channel(100));
        let channel_b = VoiceTarget::new(guild_id, channel(200));

        let status_a = core
            .play(PlayRequest::new(
                guild_id,
                channel_a.channel_id,
                "https://youtube.com/watch?v=trackInChanA",
            ))
            .await
            .unwrap();
        assert_eq!(status_a.voice_target, Some(channel_a));

        let status_b = core
            .play(PlayRequest::new(
                guild_id,
                channel_b.channel_id,
                "https://music.youtube.com/watch?v=trackInChanB",
            ))
            .await
            .unwrap();

        assert_eq!(status_b.voice_target, Some(channel_b));
        assert_eq!(status_b.playback_state, PlaybackState::Playing);
        assert_eq!(
            status_b.active_track.as_ref().map(|t| t.url.video_id()),
            Some("trackInChanB")
        );

        assert_eq!(
            backend.events(),
            vec![
                BackendEvent::ResolveStart("trackInChanA".to_string()),
                BackendEvent::Join(channel_a),
                BackendEvent::Play {
                    target: channel_a,
                    video_id: "trackInChanA".to_string(),
                    generation: PlaybackGeneration::new(1),
                },
                BackendEvent::Stop(guild_id),
                BackendEvent::ResolveStart("trackInChanB".to_string()),
                BackendEvent::Join(channel_b),
                BackendEvent::Play {
                    target: channel_b,
                    video_id: "trackInChanB".to_string(),
                    generation: PlaybackGeneration::new(2),
                },
            ]
        );
    }

    #[tokio::test]
    async fn concurrent_play_cancels_older_in_flight_resolution_cleanly() {
        let backend = MockVoiceBackend::new();
        let gate_first = backend.install_resolve_gate("slowTrack01");
        let core = Arc::new(AppCore::new(Arc::clone(&backend)));
        let guild_id = guild(7);
        let channel_a = channel(70);
        let channel_b = channel(71);

        let started_waiter = gate_first.started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();

        let core_first = Arc::clone(&core);
        let first_task = tokio::spawn(async move {
            core_first
                .play(PlayRequest::new(
                    guild_id,
                    channel_a,
                    "https://www.youtube.com/watch?v=slowTrack01",
                ))
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), started_waiter)
            .await
            .unwrap();

        assert_eq!(core.status().playback_state, PlaybackState::Loading);

        let second_status = core
            .play(PlayRequest::new(
                guild_id,
                channel_b,
                "https://www.youtube.com/watch?v=fastTrack02",
            ))
            .await
            .unwrap();

        let first_result = tokio::time::timeout(Duration::from_secs(2), first_task)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first_result, Err(CoreError::Cancelled));
        assert_eq!(
            second_status.voice_target,
            Some(VoiceTarget::new(guild_id, channel_b))
        );
        assert_eq!(second_status.playback_state, PlaybackState::Playing);
        assert_eq!(
            second_status
                .active_track
                .as_ref()
                .map(|t| t.url.video_id()),
            Some("fastTrack02")
        );
        assert_eq!(core.status(), second_status);

        let events = backend.events();
        assert!(events.contains(&BackendEvent::ResolveCancelled("slowTrack01".to_string())));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::Play { video_id, .. } if video_id == "slowTrack01"))
        );
    }

    #[tokio::test]
    async fn stop_and_leave_are_reversible_and_idempotent() {
        let backend = MockVoiceBackend::new();
        let core = AppCore::new(Arc::clone(&backend));
        let guild_id = guild(9);
        let voice_channel = channel(99);
        let target = VoiceTarget::new(guild_id, voice_channel);

        let idle_stop = core.stop().await.unwrap();
        assert_eq!(idle_stop.playback_state, PlaybackState::Idle);
        assert_eq!(idle_stop.voice_target, None);

        let idle_leave = core.leave().await.unwrap();
        assert_eq!(idle_leave.playback_state, PlaybackState::Idle);
        assert_eq!(idle_leave.voice_target, None);
        assert!(backend.events().is_empty());

        let playing = core
            .play(PlayRequest::new(
                guild_id,
                voice_channel,
                "https://www.youtube.com/watch?v=playThenStop",
            ))
            .await
            .unwrap();
        assert_eq!(playing.playback_state, PlaybackState::Playing);
        assert_eq!(playing.voice_target, Some(target));

        let stopped = core.stop().await.unwrap();
        assert_eq!(stopped.playback_state, PlaybackState::Idle);
        assert_eq!(stopped.active_track, None);
        assert_eq!(stopped.voice_target, Some(target));

        let resumed = core
            .play(PlayRequest::new(
                guild_id,
                voice_channel,
                "https://youtu.be/playThenLeave",
            ))
            .await
            .unwrap();
        assert_eq!(resumed.playback_state, PlaybackState::Playing);
        assert_eq!(resumed.voice_target, Some(target));

        let left = core.leave().await.unwrap();
        assert_eq!(left.playback_state, PlaybackState::Idle);
        assert_eq!(left.active_track, None);
        assert_eq!(left.voice_target, None);
        assert_eq!(core.status(), left);

        assert_eq!(
            backend.events(),
            vec![
                BackendEvent::ResolveStart("playThenStop".to_string()),
                BackendEvent::Join(target),
                BackendEvent::Play {
                    target,
                    video_id: "playThenStop".to_string(),
                    generation: PlaybackGeneration::new(3),
                },
                BackendEvent::Stop(guild_id),
                BackendEvent::ResolveStart("playThenLeave".to_string()),
                BackendEvent::Play {
                    target,
                    video_id: "playThenLeave".to_string(),
                    generation: PlaybackGeneration::new(5),
                },
                BackendEvent::Stop(guild_id),
                BackendEvent::Leave(guild_id),
            ]
        );
    }

    #[tokio::test]
    async fn stop_and_leave_cancel_in_flight_play_resolution() {
        let backend = MockVoiceBackend::new();
        let gate = backend.install_resolve_gate("blockedTrack");
        let core = Arc::new(AppCore::new(Arc::clone(&backend)));

        let started_waiter = gate.started.notified();
        tokio::pin!(started_waiter);
        started_waiter.as_mut().enable();

        let core_play = Arc::clone(&core);
        let play_task = tokio::spawn(async move {
            core_play
                .play(PlayRequest::new(
                    guild(3),
                    channel(30),
                    "https://www.youtube.com/watch?v=blockedTrack",
                ))
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), started_waiter)
            .await
            .unwrap();

        let stopped = core.stop().await.unwrap();
        assert_eq!(stopped.playback_state, PlaybackState::Idle);
        assert_eq!(stopped.active_track, None);
        assert_eq!(stopped.voice_target, None);

        let play_result = tokio::time::timeout(Duration::from_secs(2), play_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(play_result, Err(CoreError::Cancelled));

        let second_gate = backend.install_resolve_gate("blockedBeforeLeave");
        let second_started = second_gate.started.notified();
        tokio::pin!(second_started);
        second_started.as_mut().enable();

        let core_play_leave = Arc::clone(&core);
        let leave_play_task = tokio::spawn(async move {
            core_play_leave
                .play(PlayRequest::new(
                    guild(3),
                    channel(30),
                    "https://www.youtube.com/watch?v=blockedBeforeLeave",
                ))
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), second_started)
            .await
            .unwrap();

        let left = core.leave().await.unwrap();
        assert_eq!(left.playback_state, PlaybackState::Idle);
        assert_eq!(left.active_track, None);
        assert_eq!(left.voice_target, None);

        let leave_play_result = tokio::time::timeout(Duration::from_secs(2), leave_play_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leave_play_result, Err(CoreError::Cancelled));
    }

    #[tokio::test]
    async fn invalid_url_does_not_interrupt_currently_playing_track() {
        let backend = MockVoiceBackend::new();
        let core = AppCore::new(Arc::clone(&backend));
        let target = VoiceTarget::new(guild(5), channel(50));

        let active = core
            .play(PlayRequest::new(
                target.guild_id,
                target.channel_id,
                "https://www.youtube.com/watch?v=activeTrack",
            ))
            .await
            .unwrap();

        let error = core
            .play(PlayRequest::new(
                target.guild_id,
                target.channel_id,
                "file:///etc/passwd",
            ))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            CoreError::InvalidUrl(YouTubeUrlError::UnsupportedScheme {
                scheme: "file".to_string(),
            })
        );
        assert_eq!(core.status(), active);
    }

    #[tokio::test]
    async fn resolve_and_join_failures_leave_consistent_state() {
        let backend = MockVoiceBackend::new();
        backend.fail_resolve("brokenTrack", "video unavailable");
        backend.fail_join(channel(61), "missing connect permission");

        let core = AppCore::new(Arc::clone(&backend));
        let guild_id = guild(6);

        let resolve_error = core
            .play(PlayRequest::new(
                guild_id,
                channel(60),
                "https://www.youtube.com/watch?v=brokenTrack",
            ))
            .await
            .unwrap_err();
        assert_eq!(
            resolve_error,
            CoreError::Backend(VoiceBackendError::ResolveFailed(
                "video unavailable".to_string()
            ))
        );
        assert_eq!(core.status().playback_state, PlaybackState::Idle);
        assert_eq!(core.status().active_track, None);
        assert_eq!(core.status().voice_target, None);

        let ok_status = core
            .play(PlayRequest::new(
                guild_id,
                channel(60),
                "https://www.youtube.com/watch?v=goodTrack01",
            ))
            .await
            .unwrap();
        assert_eq!(
            ok_status.voice_target,
            Some(VoiceTarget::new(guild_id, channel(60)))
        );

        let join_error = core
            .play(PlayRequest::new(
                guild_id,
                channel(61),
                "https://www.youtube.com/watch?v=goodTrack02",
            ))
            .await
            .unwrap_err();
        assert_eq!(
            join_error,
            CoreError::Backend(VoiceBackendError::JoinFailed(
                "missing connect permission".to_string()
            ))
        );
        assert_eq!(core.status().playback_state, PlaybackState::Idle);
        assert_eq!(core.status().active_track, None);
        assert_eq!(
            core.status().voice_target,
            Some(VoiceTarget::new(guild_id, channel(60)))
        );
    }

    #[tokio::test]
    async fn track_end_and_disconnect_notifications_respect_generation_and_guild() {
        let backend = MockVoiceBackend::new();
        let core = AppCore::new(Arc::clone(&backend));
        let guild_id = guild(8);
        let channel_id = channel(80);

        let _ = core
            .play(PlayRequest::new(
                guild_id,
                channel_id,
                "https://www.youtube.com/watch?v=trackOne001",
            ))
            .await
            .unwrap();

        let _ = core
            .play(PlayRequest::new(
                guild_id,
                channel_id,
                "https://www.youtube.com/watch?v=trackTwo002",
            ))
            .await
            .unwrap();

        core.notify_track_ended(PlaybackGeneration::new(1));
        assert_eq!(core.status().playback_state, PlaybackState::Playing);

        core.notify_track_ended(PlaybackGeneration::new(2));
        assert_eq!(core.status().playback_state, PlaybackState::Idle);
        assert_eq!(core.status().active_track, None);
        assert_eq!(
            core.status().voice_target,
            Some(VoiceTarget::new(guild_id, channel_id))
        );

        core.notify_voice_disconnected(guild(999));
        assert!(core.status().is_connected_to_voice());

        core.notify_voice_disconnected(guild_id);
        assert!(!core.status().is_connected_to_voice());
        assert_eq!(core.status().playback_state, PlaybackState::Idle);
    }

    #[tokio::test]
    async fn play_superseded_during_backend_start_stops_stale_playback_and_preserves_winner() {
        let backend = MockVoiceBackend::new();
        let play_gate = backend.install_play_gate("lateTrack01");
        let second_resolve_gate = backend.install_resolve_gate("winnerTrack2");
        let core = Arc::new(AppCore::new(Arc::clone(&backend)));
        let guild_id = guild(11);
        let channel_id = channel(110);

        let first_play_started = play_gate.started.notified();
        tokio::pin!(first_play_started);
        first_play_started.as_mut().enable();

        let core_first = Arc::clone(&core);
        let first_task = tokio::spawn(async move {
            core_first
                .play(PlayRequest::new(
                    guild_id,
                    channel_id,
                    "https://www.youtube.com/watch?v=lateTrack01",
                ))
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), first_play_started)
            .await
            .unwrap();

        let second_resolve_started = second_resolve_gate.started.notified();
        tokio::pin!(second_resolve_started);
        second_resolve_started.as_mut().enable();

        let core_second = Arc::clone(&core);
        let second_task = tokio::spawn(async move {
            core_second
                .play(PlayRequest::new(
                    guild_id,
                    channel_id,
                    "https://www.youtube.com/watch?v=winnerTrack2",
                ))
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), second_resolve_started)
            .await
            .unwrap();

        play_gate.release.notify_waiters();
        second_resolve_gate.release.notify_waiters();

        let first_result = tokio::time::timeout(Duration::from_secs(2), first_task)
            .await
            .unwrap()
            .unwrap();
        let second_status = tokio::time::timeout(Duration::from_secs(2), second_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert_eq!(first_result, Err(CoreError::Cancelled));
        assert_eq!(second_status.playback_state, PlaybackState::Playing);
        assert_eq!(
            second_status
                .active_track
                .as_ref()
                .map(|t| t.url.video_id()),
            Some("winnerTrack2")
        );
        assert_eq!(core.status(), second_status);

        let events = backend.events();
        assert!(events.contains(&BackendEvent::Stop(guild_id)));
    }
}
