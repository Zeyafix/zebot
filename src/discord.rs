use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serenity::async_trait;
use serenity::builder::{
    CreateCommand, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use serenity::client::{Client, Context, EventHandler};
use serenity::gateway::{ConnectionStage, ShardStageUpdateEvent};
use serenity::model::application::Interaction;
use serenity::model::event::ResumedEvent;
use serenity::model::gateway::{GatewayIntents, Ready};
use serenity::model::id::GuildId as SerenityGuildId;

use crate::config::Config;
use crate::core::{AppCore, VoiceBackend};
use crate::state::{BotStatus, DiscordConnectionState, PlaybackState, TrackMetadata};

pub const STATUS_COMMAND_NAME: &str = "status";
pub const STATUS_COMMAND_DESCRIPTION: &str =
    "Show Zebot readiness, active voice channel, and playback state";

#[derive(Debug)]
pub enum DiscordError {
    InvalidGuildId,
    InvalidAllowedChannelId,
    ClientBuild(Box<serenity::Error>),
    ClientRun(Box<serenity::Error>),
}

impl fmt::Display for DiscordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGuildId => write!(f, "DISCORD_GUILD_ID must be a non-zero Discord ID"),
            Self::InvalidAllowedChannelId => {
                write!(f, "ALLOWED_CHANNEL_ID must be a non-zero Discord ID")
            }
            Self::ClientBuild(source) => write!(f, "failed to initialize Discord client: {source}"),
            Self::ClientRun(source) => write!(f, "Discord client terminated with error: {source}"),
        }
    }
}

impl Error for DiscordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidGuildId | Self::InvalidAllowedChannelId => None,
            Self::ClientBuild(source) | Self::ClientRun(source) => Some(source),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandScope {
    guild_id: NonZeroU64,
    allowed_channel_id: NonZeroU64,
}

impl CommandScope {
    pub fn new(guild_id: u64, allowed_channel_id: u64) -> Result<Self, DiscordError> {
        let guild_id = NonZeroU64::new(guild_id).ok_or(DiscordError::InvalidGuildId)?;
        let allowed_channel_id =
            NonZeroU64::new(allowed_channel_id).ok_or(DiscordError::InvalidAllowedChannelId)?;
        Ok(Self {
            guild_id,
            allowed_channel_id,
        })
    }

    pub fn from_config(config: &Config) -> Result<Self, DiscordError> {
        Self::new(config.discord_guild_id, config.allowed_channel_id)
    }

    pub fn guild_id(self) -> SerenityGuildId {
        SerenityGuildId::from(self.guild_id)
    }

    pub fn guild_id_raw(self) -> u64 {
        self.guild_id.get()
    }

    pub fn allowed_channel_id_raw(self) -> u64 {
        self.allowed_channel_id.get()
    }

    pub fn evaluate_access(self, guild_id: Option<u64>, channel_id: u64) -> CommandAccessDecision {
        if guild_id != Some(self.guild_id.get()) {
            return CommandAccessDecision::RejectedWrongGuild;
        }

        if channel_id != self.allowed_channel_id.get() {
            return CommandAccessDecision::RejectedWrongChannel {
                allowed_channel_id: self.allowed_channel_id.get(),
            };
        }

        CommandAccessDecision::Allowed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAccessDecision {
    Allowed,
    RejectedWrongGuild,
    RejectedWrongChannel { allowed_channel_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReply {
    pub content: String,
    pub ephemeral: bool,
}

impl CommandReply {
    pub fn public(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ephemeral: false,
        }
    }

    pub fn ephemeral(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ephemeral: true,
        }
    }
}

pub fn required_gateway_intents() -> GatewayIntents {
    GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES
}

pub fn build_slash_commands() -> Vec<CreateCommand> {
    vec![CreateCommand::new(STATUS_COMMAND_NAME).description(STATUS_COMMAND_DESCRIPTION)]
}

pub fn handle_slash_command<B>(
    scope: CommandScope,
    app_core: &AppCore<B>,
    guild_id: Option<u64>,
    channel_id: u64,
    command_name: &str,
) -> CommandReply {
    match scope.evaluate_access(guild_id, channel_id) {
        CommandAccessDecision::RejectedWrongGuild => {
            CommandReply::ephemeral("Zebot only accepts commands in its configured Discord server.")
        }
        CommandAccessDecision::RejectedWrongChannel { allowed_channel_id } => {
            CommandReply::ephemeral(format!(
                "Please use Zebot commands in <#{allowed_channel_id}>."
            ))
        }
        CommandAccessDecision::Allowed => match command_name {
            STATUS_COMMAND_NAME => {
                let status = app_core.status();
                CommandReply::public(format_status_message(&status))
            }
            other => CommandReply::ephemeral(format!("Unknown command '/{other}'.")),
        },
    }
}

pub fn format_status_message(status: &BotStatus) -> String {
    let readiness = format_discord_state(status.discord_state);
    let voice_channel = match status.voice_target {
        Some(target) => format!("<#{}>", target.channel_id),
        None => "Not connected".to_string(),
    };
    let playback = format_playback_state(status.playback_state);
    let track = match status.active_track.as_ref() {
        Some(metadata) => format_track_metadata(metadata),
        None => "None".to_string(),
    };

    format!(
        "**Zebot Status**\n- Readiness: {readiness}\n- Voice channel: {voice_channel}\n- Playback: {playback}\n- Track: {track}"
    )
}

fn format_discord_state(state: DiscordConnectionState) -> &'static str {
    match state {
        DiscordConnectionState::Disconnected => "Disconnected",
        DiscordConnectionState::Connecting => "Connecting",
        DiscordConnectionState::Connected => "Connected (Ready)",
        DiscordConnectionState::ShuttingDown => "Shutting down",
    }
}

fn format_playback_state(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Idle => "Idle",
        PlaybackState::Loading => "Loading",
        PlaybackState::Playing => "Playing",
    }
}

fn format_track_metadata(track: &TrackMetadata) -> String {
    let mut parts = Vec::new();

    if let Some(title) = track.title.as_deref()
        && !title.trim().is_empty()
    {
        parts.push(title.trim().to_string());
    }

    if let Some(channel) = track.channel.as_deref()
        && !channel.trim().is_empty()
    {
        if parts.is_empty() {
            parts.push(channel.trim().to_string());
        } else {
            parts.push(format!("by {}", channel.trim()));
        }
    }

    if let Some(duration) = track.duration_seconds {
        parts.push(format!("({})", format_duration(duration)));
    }

    parts.push(format!("<{}>", track.url.as_str()));
    parts.join(" ")
}

fn format_duration(total_seconds: u64) -> String {
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub fn map_connection_stage(stage: ConnectionStage) -> DiscordConnectionState {
    match stage {
        ConnectionStage::Connected => DiscordConnectionState::Connected,
        ConnectionStage::Disconnected => DiscordConnectionState::Disconnected,
        ConnectionStage::Connecting
        | ConnectionStage::Handshake
        | ConnectionStage::Identifying
        | ConnectionStage::Resuming => DiscordConnectionState::Connecting,
        _ => DiscordConnectionState::Connecting,
    }
}

fn update_gateway_state<B>(
    app_core: &AppCore<B>,
    shutting_down: &AtomicBool,
    next_state: DiscordConnectionState,
) {
    if !shutting_down.load(Ordering::SeqCst) {
        app_core.set_discord_state(next_state);
    }
}

struct DiscordHandler<B> {
    scope: CommandScope,
    app_core: Arc<AppCore<B>>,
    shutting_down: Arc<AtomicBool>,
}

#[async_trait]
impl<B: VoiceBackend + 'static> EventHandler for DiscordHandler<B> {
    async fn ready(&self, ctx: Context, ready: Ready) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }

        let guild_id = self.scope.guild_id();
        match guild_id
            .set_commands(&ctx.http, build_slash_commands())
            .await
        {
            Ok(commands) => {
                let command_names: Vec<&str> = commands
                    .iter()
                    .map(|command| command.name.as_str())
                    .collect();
                println!(
                    "Discord connected as {} (guild_id: {}, allowed_channel_id: {}, commands: [{}])",
                    ready.user.name,
                    self.scope.guild_id_raw(),
                    self.scope.allowed_channel_id_raw(),
                    command_names.join(", ")
                );
            }
            Err(error) => {
                eprintln!(
                    "error: failed to register guild slash commands for guild {}: {error}",
                    self.scope.guild_id_raw()
                );
            }
        }

        update_gateway_state(
            &self.app_core,
            &self.shutting_down,
            DiscordConnectionState::Connected,
        );
    }

    async fn resume(&self, _ctx: Context, _resume: ResumedEvent) {
        update_gateway_state(
            &self.app_core,
            &self.shutting_down,
            DiscordConnectionState::Connected,
        );
    }

    async fn shard_stage_update(&self, _ctx: Context, event: ShardStageUpdateEvent) {
        update_gateway_state(
            &self.app_core,
            &self.shutting_down,
            map_connection_stage(event.new),
        );
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };

        let reply = handle_slash_command(
            self.scope,
            &self.app_core,
            command.guild_id.map(SerenityGuildId::get),
            command.channel_id.get(),
            command.data.name.as_str(),
        );

        let message = CreateInteractionResponseMessage::new()
            .content(reply.content)
            .ephemeral(reply.ephemeral);
        let response = CreateInteractionResponse::Message(message);

        if let Err(error) = command.create_response(&ctx.http, response).await {
            eprintln!(
                "error: failed to respond to slash command '/{}': {error}",
                command.data.name
            );
        }
    }
}

pub async fn run_discord_bot<B, S>(
    config: &Config,
    app_core: Arc<AppCore<B>>,
    shutdown_signal: S,
) -> Result<(), DiscordError>
where
    B: VoiceBackend + 'static,
    S: Future<Output = ()> + Send + 'static,
{
    let scope = CommandScope::from_config(config)?;
    let shutting_down = Arc::new(AtomicBool::new(false));
    let handler = DiscordHandler {
        scope,
        app_core: Arc::clone(&app_core),
        shutting_down: Arc::clone(&shutting_down),
    };

    let mut client = Client::builder(&config.discord_token, required_gateway_intents())
        .event_handler(handler)
        .await
        .map_err(|error| DiscordError::ClientBuild(Box::new(error)))?;

    let shard_manager = Arc::clone(&client.shard_manager);

    run_with_shutdown(
        app_core,
        shutting_down,
        async move {
            client
                .start()
                .await
                .map_err(|error| DiscordError::ClientRun(Box::new(error)))
        },
        shutdown_signal,
        move || async move {
            shard_manager.shutdown_all().await;
        },
    )
    .await
}

async fn run_with_shutdown<B, S, RunFut, ShutdownFut, ShutdownFn>(
    app_core: Arc<AppCore<B>>,
    shutting_down: Arc<AtomicBool>,
    run_client: RunFut,
    shutdown_signal: S,
    shutdown_shards: ShutdownFn,
) -> Result<(), DiscordError>
where
    B: VoiceBackend + 'static,
    S: Future<Output = ()> + Send + 'static,
    RunFut: Future<Output = Result<(), DiscordError>>,
    ShutdownFut: Future<Output = ()> + Send + 'static,
    ShutdownFn: FnOnce() -> ShutdownFut + Send + 'static,
{
    app_core.set_discord_state(DiscordConnectionState::Connecting);

    let shutdown_core = Arc::clone(&app_core);
    let shutdown_flag = Arc::clone(&shutting_down);
    let shutdown_task = tokio::spawn(async move {
        shutdown_signal.await;
        shutdown_flag.store(true, Ordering::SeqCst);
        shutdown_core.set_discord_state(DiscordConnectionState::ShuttingDown);
        if let Err(error) = shutdown_core.leave().await {
            eprintln!("warning: failed to leave voice channel during shutdown: {error}");
        }
        shutdown_shards().await;
    });

    let run_result = run_client.await;

    if shutting_down.load(Ordering::SeqCst) {
        let _ = shutdown_task.await;
    } else {
        shutdown_task.abort();
    }

    app_core.set_discord_state(DiscordConnectionState::Disconnected);
    run_result
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use serenity::gateway::ConnectionStage;
    use serenity::model::gateway::GatewayIntents;
    use tokio::sync::Notify;

    use super::{
        CommandAccessDecision, CommandReply, CommandScope, DiscordError, STATUS_COMMAND_NAME,
        build_slash_commands, format_status_message, handle_slash_command, map_connection_stage,
        required_gateway_intents, run_with_shutdown, update_gateway_state,
    };
    use crate::core::{
        AppCore, CancellationToken, PlaybackGeneration, UnavailableVoiceBackend, VoiceBackend,
        VoiceBackendError,
    };
    use crate::state::{
        BotStatus, DiscordConnectionState, GuildId, PlayRequest, PlaybackState, TrackMetadata,
        VoiceChannelId, VoiceTarget,
    };
    use crate::youtube::YouTubeUrl;

    #[derive(Default)]
    struct RecordingBackend {
        leave_calls: AtomicUsize,
        stop_calls: AtomicUsize,
    }

    impl VoiceBackend for RecordingBackend {
        async fn resolve_track(
            &self,
            url: &YouTubeUrl,
            _cancel: CancellationToken,
        ) -> Result<TrackMetadata, VoiceBackendError> {
            Ok(TrackMetadata {
                url: url.clone(),
                title: Some("Sample Song".to_string()),
                channel: Some("Sample Artist".to_string()),
                duration_seconds: Some(213),
            })
        }

        async fn join(
            &self,
            _target: VoiceTarget,
            _cancel: CancellationToken,
        ) -> Result<(), VoiceBackendError> {
            Ok(())
        }

        async fn play(
            &self,
            _target: VoiceTarget,
            _track: &TrackMetadata,
            _generation: PlaybackGeneration,
            _cancel: CancellationToken,
        ) -> Result<(), VoiceBackendError> {
            Ok(())
        }

        async fn stop(&self, _guild_id: GuildId) -> Result<(), VoiceBackendError> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn leave(&self, _guild_id: GuildId) -> Result<(), VoiceBackendError> {
            self.leave_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn required_intents_include_guilds_and_voice_states_without_privileged_intents() {
        let intents = required_gateway_intents();
        assert!(intents.contains(GatewayIntents::GUILDS));
        assert!(intents.contains(GatewayIntents::GUILD_VOICE_STATES));
        assert!(!intents.contains(GatewayIntents::MESSAGE_CONTENT));
        assert!(!intents.contains(GatewayIntents::GUILD_MEMBERS));
        assert!(!intents.contains(GatewayIntents::GUILD_PRESENCES));
    }

    #[test]
    fn registers_status_slash_command() {
        let commands = build_slash_commands();
        assert_eq!(commands.len(), 1);
        let debug_repr = format!("{commands:?}");
        assert!(debug_repr.contains("\"status\""));
        assert!(debug_repr.contains(super::STATUS_COMMAND_DESCRIPTION));
    }

    #[tokio::test]
    async fn client_builder_initializes_with_rustls_and_minimal_features() {
        let client = serenity::client::Client::builder(
            "test-token-not-used-for-network",
            required_gateway_intents(),
        )
        .await;
        assert!(client.is_ok());
    }

    #[test]
    fn command_scope_rejects_zero_ids() {
        assert!(matches!(
            CommandScope::new(0, 10),
            Err(DiscordError::InvalidGuildId)
        ));
        assert!(matches!(
            CommandScope::new(10, 0),
            Err(DiscordError::InvalidAllowedChannelId)
        ));
    }

    #[test]
    fn command_scope_enforces_guild_and_allowed_channel() {
        let scope = CommandScope::new(100, 200).unwrap();

        assert_eq!(
            scope.evaluate_access(Some(100), 200),
            CommandAccessDecision::Allowed
        );
        assert_eq!(
            scope.evaluate_access(Some(100), 999),
            CommandAccessDecision::RejectedWrongChannel {
                allowed_channel_id: 200,
            }
        );
        assert_eq!(
            scope.evaluate_access(Some(555), 200),
            CommandAccessDecision::RejectedWrongGuild
        );
        assert_eq!(
            scope.evaluate_access(None, 200),
            CommandAccessDecision::RejectedWrongGuild
        );
    }

    #[test]
    fn status_command_in_allowed_channel_reads_live_app_core_state() {
        let scope = CommandScope::new(100, 200).unwrap();
        let core = AppCore::new(UnavailableVoiceBackend);
        core.set_discord_state(DiscordConnectionState::Connected);

        let reply = handle_slash_command(scope, &core, Some(100), 200, STATUS_COMMAND_NAME);

        assert!(!reply.ephemeral);
        assert!(reply.content.contains("Readiness: Connected (Ready)"));
        assert!(reply.content.contains("Voice channel: Not connected"));
        assert!(reply.content.contains("Playback: Idle"));
        assert!(reply.content.contains("Track: None"));
    }

    #[test]
    fn command_outside_allowed_channel_returns_ephemeral_hint_without_status() {
        let scope = CommandScope::new(100, 200).unwrap();
        let core = AppCore::new(UnavailableVoiceBackend);
        core.set_discord_state(DiscordConnectionState::Connected);

        let reply = handle_slash_command(scope, &core, Some(100), 300, STATUS_COMMAND_NAME);

        assert_eq!(
            reply,
            CommandReply::ephemeral("Please use Zebot commands in <#200>.")
        );
    }

    #[test]
    fn command_in_other_guild_or_dm_returns_ephemeral_rejection() {
        let scope = CommandScope::new(100, 200).unwrap();
        let core = AppCore::new(UnavailableVoiceBackend);

        let dm_reply = handle_slash_command(scope, &core, None, 200, STATUS_COMMAND_NAME);
        assert!(dm_reply.ephemeral);
        assert!(!dm_reply.content.contains("<#200>"));

        let other_guild_reply =
            handle_slash_command(scope, &core, Some(999), 200, STATUS_COMMAND_NAME);
        assert!(other_guild_reply.ephemeral);
        assert!(!other_guild_reply.content.contains("<#200>"));
    }

    #[test]
    fn unknown_command_in_allowed_channel_returns_ephemeral_reply() {
        let scope = CommandScope::new(100, 200).unwrap();
        let core = AppCore::new(UnavailableVoiceBackend);

        let reply = handle_slash_command(scope, &core, Some(100), 200, "nonexistent");
        assert_eq!(
            reply,
            CommandReply::ephemeral("Unknown command '/nonexistent'.")
        );
    }

    #[test]
    fn formats_playing_and_loading_status_with_track_details() {
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();
        let playing = BotStatus {
            discord_state: DiscordConnectionState::Connected,
            voice_target: Some(VoiceTarget::new(
                GuildId::new(10).unwrap(),
                VoiceChannelId::new(20).unwrap(),
            )),
            playback_state: PlaybackState::Playing,
            active_track: Some(TrackMetadata {
                url: url.clone(),
                title: Some("Never Gonna Give You Up".to_string()),
                channel: Some("Rick Astley".to_string()),
                duration_seconds: Some(3665),
            }),
        };

        let rendered_playing = format_status_message(&playing);
        assert!(rendered_playing.contains("Voice channel: <#20>"));
        assert!(rendered_playing.contains("Playback: Playing"));
        assert!(rendered_playing.contains(
            "Track: Never Gonna Give You Up by Rick Astley (1:01:05) <https://www.youtube.com/watch?v=dQw4w9WgXcQ>"
        ));

        let loading = BotStatus {
            discord_state: DiscordConnectionState::Connecting,
            voice_target: None,
            playback_state: PlaybackState::Loading,
            active_track: Some(TrackMetadata::from_url(url)),
        };

        let rendered_loading = format_status_message(&loading);
        assert!(rendered_loading.contains("Readiness: Connecting"));
        assert!(rendered_loading.contains("Playback: Loading"));
        assert!(rendered_loading.contains("Track: <https://www.youtube.com/watch?v=dQw4w9WgXcQ>"));
    }

    #[test]
    fn maps_connection_stages_and_preserves_shutting_down_state() {
        assert_eq!(
            map_connection_stage(ConnectionStage::Connected),
            DiscordConnectionState::Connected
        );
        assert_eq!(
            map_connection_stage(ConnectionStage::Disconnected),
            DiscordConnectionState::Disconnected
        );
        assert_eq!(
            map_connection_stage(ConnectionStage::Resuming),
            DiscordConnectionState::Connecting
        );

        let core = AppCore::new(UnavailableVoiceBackend);
        let shutting_down = AtomicBool::new(false);

        update_gateway_state(&core, &shutting_down, DiscordConnectionState::Connected);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Connected
        );

        shutting_down.store(true, Ordering::SeqCst);
        core.set_discord_state(DiscordConnectionState::ShuttingDown);
        update_gateway_state(&core, &shutting_down, DiscordConnectionState::Connected);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::ShuttingDown
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_transitions_state_leaves_voice_and_closes_shards() {
        let backend = Arc::new(RecordingBackend::default());
        let core = Arc::new(AppCore::new(Arc::clone(&backend)));

        let guild_id = GuildId::new(100).unwrap();
        let channel_id = VoiceChannelId::new(200).unwrap();
        core.play(PlayRequest::new(
            guild_id,
            channel_id,
            "https://youtu.be/dQw4w9WgXcQ",
        ))
        .await
        .unwrap();

        let shutting_down = Arc::new(AtomicBool::new(false));
        let trigger_shutdown = Arc::new(Notify::new());
        let client_started = Arc::new(Notify::new());
        let shards_closed = Arc::new(Notify::new());
        let observed_states = Arc::new(Mutex::new(Vec::new()));

        let client_core = Arc::clone(&core);
        let client_started_tx = Arc::clone(&client_started);
        let shards_closed_rx = Arc::clone(&shards_closed);
        let run_client = async move {
            client_core.set_discord_state(DiscordConnectionState::Connected);
            client_started_tx.notify_one();
            shards_closed_rx.notified().await;
            Ok(())
        };

        let trigger_rx = Arc::clone(&trigger_shutdown);
        let shutdown_signal = async move {
            trigger_rx.notified().await;
        };

        let states_for_shards = Arc::clone(&observed_states);
        let core_for_shards = Arc::clone(&core);
        let shards_closed_tx = Arc::clone(&shards_closed);
        let shutdown_shards = move || async move {
            states_for_shards
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(core_for_shards.status().discord_state);
            shards_closed_tx.notify_one();
        };

        let runner_core = Arc::clone(&core);
        let runner_handle = tokio::spawn(async move {
            run_with_shutdown(
                runner_core,
                shutting_down,
                run_client,
                shutdown_signal,
                shutdown_shards,
            )
            .await
        });

        client_started.notified().await;
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Connected
        );

        trigger_shutdown.notify_one();
        runner_handle.await.unwrap().unwrap();

        let recorded = observed_states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(recorded, vec![DiscordConnectionState::ShuttingDown]);
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Disconnected
        );
        assert_eq!(core.status().voice_target, None);
        assert_eq!(backend.stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.leave_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn early_client_failure_resets_state_to_disconnected_and_aborts_shutdown_waiter() {
        let core = Arc::new(AppCore::new(UnavailableVoiceBackend));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let never_fires = Arc::new(Notify::new());
        let shards_shutdown_called = Arc::new(AtomicBool::new(false));

        let shards_flag = Arc::clone(&shards_shutdown_called);
        let result = run_with_shutdown(
            Arc::clone(&core),
            shutting_down,
            async { Err(DiscordError::InvalidGuildId) },
            async move {
                never_fires.notified().await;
            },
            move || async move {
                shards_flag.store(true, Ordering::SeqCst);
            },
        )
        .await;

        assert!(matches!(result, Err(DiscordError::InvalidGuildId)));
        assert_eq!(
            core.status().discord_state,
            DiscordConnectionState::Disconnected
        );
        assert!(!shards_shutdown_called.load(Ordering::SeqCst));
    }
}
