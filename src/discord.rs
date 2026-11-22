use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serenity::async_trait;
use serenity::builder::{
    CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse,
};
use serenity::client::{Client, Context, EventHandler};
use serenity::gateway::{ConnectionStage, ShardStageUpdateEvent};
use serenity::model::application::{
    CommandInteraction, CommandOptionType, Interaction, ResolvedValue,
};
use serenity::model::event::ResumedEvent;
use serenity::model::gateway::{GatewayIntents, Ready};
use serenity::model::guild::Guild;
use serenity::model::id::{ChannelId as SerenityChannelId, GuildId as SerenityGuildId};
use serenity::model::permissions::Permissions;
use serenity::model::voice::VoiceState;
use songbird::{SerenityInit, Songbird};

use crate::config::Config;
use crate::core::{AppCore, CoreError, VoiceBackend};
use crate::state::{
    BotStatus, DiscordConnectionState, GuildId, PlayRequest, PlaybackState, TrackMetadata,
    VoiceChannelId, VoiceTarget,
};
use crate::youtube::YouTubeUrl;

pub const STATUS_COMMAND_NAME: &str = "status";
pub const STATUS_COMMAND_DESCRIPTION: &str =
    "Show Zebot readiness, active voice channel, and playback state";

pub const PLAY_COMMAND_NAME: &str = "play";
pub const PLAY_COMMAND_DESCRIPTION: &str =
    "Stream audio from a YouTube URL into your current voice channel";
pub const PLAY_URL_OPTION_NAME: &str = "url";
pub const PLAY_URL_OPTION_DESCRIPTION: &str = "YouTube video URL to play";

pub const STOP_COMMAND_NAME: &str = "stop";
pub const STOP_COMMAND_DESCRIPTION: &str =
    "Stop current audio playback while remaining in the voice channel";

pub const LEAVE_COMMAND_NAME: &str = "leave";
pub const LEAVE_COMMAND_DESCRIPTION: &str =
    "Stop audio playback and disconnect from the voice channel";

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

    pub fn core_guild_id(self) -> Option<GuildId> {
        GuildId::new(self.guild_id.get())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingVoicePermissions {
    pub connect: bool,
    pub speak: bool,
}

impl MissingVoicePermissions {
    pub fn describe(self) -> &'static str {
        match (self.connect, self.speak) {
            (true, true) => "Connect and Speak",
            (true, false) => "Connect",
            (false, true) => "Speak",
            (false, false) => "none",
        }
    }
}

pub fn check_voice_permissions(permissions: Permissions) -> Result<(), MissingVoicePermissions> {
    let missing_connect = !permissions.connect();
    let missing_speak = !permissions.speak();

    if missing_connect || missing_speak {
        Err(MissingVoicePermissions {
            connect: missing_connect,
            speak: missing_speak,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayCommandContext {
    pub invoking_voice_channel_id: Option<u64>,
    pub bot_permissions: Option<Permissions>,
}

pub fn validate_play_preflight(
    url: &str,
    context: PlayCommandContext,
) -> Result<(VoiceChannelId, YouTubeUrl), CommandReply> {
    let Some(raw_channel_id) = context.invoking_voice_channel_id else {
        return Err(CommandReply::ephemeral(
            "You need to join a voice channel before using `/play`. Join a voice channel in this server and run `/play <youtube-url>` again.",
        ));
    };

    let Some(voice_channel_id) = VoiceChannelId::new(raw_channel_id) else {
        return Err(CommandReply::ephemeral(
            "Unable to resolve your voice channel. Rejoin the voice channel and try `/play` again.",
        ));
    };

    if let Some(permissions) = context.bot_permissions
        && let Err(missing) = check_voice_permissions(permissions)
    {
        return Err(CommandReply::ephemeral(format!(
            "Zebot is missing the required {} permission(s) in <#{}>. Grant Connect and Speak permissions for that voice channel and retry.",
            missing.describe(),
            voice_channel_id
        )));
    }

    let validated_url = YouTubeUrl::parse(url).map_err(|error| {
        CommandReply::ephemeral(format!(
            "Invalid YouTube URL: {error}. Provide an `https://` YouTube video link (for example `https://www.youtube.com/watch?v=...` or `https://youtu.be/...`)."
        ))
    })?;

    Ok((voice_channel_id, validated_url))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceChannelMember {
    pub user_id: u64,
    pub is_bot: bool,
}

pub fn count_non_bot_members(members: &[VoiceChannelMember], bot_user_id: u64) -> usize {
    members
        .iter()
        .filter(|member| member.user_id != bot_user_id && !member.is_bot)
        .count()
}

pub fn should_auto_leave_voice_channel(
    status: &BotStatus,
    guild_id: GuildId,
    members: &[VoiceChannelMember],
    bot_user_id: u64,
) -> bool {
    let Some(target) = status.voice_target else {
        return false;
    };

    if target.guild_id != guild_id {
        return false;
    }

    count_non_bot_members(members, bot_user_id) == 0
}

pub fn required_gateway_intents() -> GatewayIntents {
    GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES
}

pub fn build_slash_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new(STATUS_COMMAND_NAME).description(STATUS_COMMAND_DESCRIPTION),
        CreateCommand::new(PLAY_COMMAND_NAME)
            .description(PLAY_COMMAND_DESCRIPTION)
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    PLAY_URL_OPTION_NAME,
                    PLAY_URL_OPTION_DESCRIPTION,
                )
                .required(true),
            ),
        CreateCommand::new(STOP_COMMAND_NAME).description(STOP_COMMAND_DESCRIPTION),
        CreateCommand::new(LEAVE_COMMAND_NAME).description(LEAVE_COMMAND_DESCRIPTION),
    ]
}

pub fn access_rejection_reply(decision: CommandAccessDecision) -> Option<CommandReply> {
    match decision {
        CommandAccessDecision::Allowed => None,
        CommandAccessDecision::RejectedWrongGuild => Some(CommandReply::ephemeral(
            "Zebot only accepts commands in its configured Discord server.",
        )),
        CommandAccessDecision::RejectedWrongChannel { allowed_channel_id } => {
            Some(CommandReply::ephemeral(format!(
                "Please use Zebot commands in <#{allowed_channel_id}>."
            )))
        }
    }
}

pub fn handle_slash_command<B>(
    scope: CommandScope,
    app_core: &AppCore<B>,
    guild_id: Option<u64>,
    channel_id: u64,
    command_name: &str,
) -> CommandReply {
    if let Some(rejection) = access_rejection_reply(scope.evaluate_access(guild_id, channel_id)) {
        return rejection;
    }

    match command_name {
        STATUS_COMMAND_NAME => {
            let status = app_core.status();
            CommandReply::public(format_status_message(&status))
        }
        other => CommandReply::ephemeral(format!("Unknown command '/{other}'.")),
    }
}

pub async fn execute_play_command<B: VoiceBackend>(
    scope: CommandScope,
    app_core: &AppCore<B>,
    guild_id: Option<u64>,
    channel_id: u64,
    url: &str,
    play_context: PlayCommandContext,
) -> CommandReply {
    if let Some(rejection) = access_rejection_reply(scope.evaluate_access(guild_id, channel_id)) {
        return rejection;
    }

    let (voice_channel_id, validated_url) = match validate_play_preflight(url, play_context) {
        Ok(valid) => valid,
        Err(reply) => return reply,
    };

    let Some(core_guild_id) = scope.core_guild_id() else {
        return CommandReply::ephemeral("Zebot guild configuration is invalid.");
    };

    let request = PlayRequest::new(core_guild_id, voice_channel_id, validated_url.as_str());
    format_play_outcome(app_core.play(request).await, voice_channel_id)
}

pub async fn execute_stop_command<B: VoiceBackend>(
    scope: CommandScope,
    app_core: &AppCore<B>,
    guild_id: Option<u64>,
    channel_id: u64,
) -> CommandReply {
    if let Some(rejection) = access_rejection_reply(scope.evaluate_access(guild_id, channel_id)) {
        return rejection;
    }

    let was_active = app_core.status().playback_state != PlaybackState::Idle;
    format_stop_outcome(app_core.stop().await, was_active)
}

pub async fn execute_leave_command<B: VoiceBackend>(
    scope: CommandScope,
    app_core: &AppCore<B>,
    guild_id: Option<u64>,
    channel_id: u64,
) -> CommandReply {
    if let Some(rejection) = access_rejection_reply(scope.evaluate_access(guild_id, channel_id)) {
        return rejection;
    }

    let status_before = app_core.status();
    let was_connected_or_active = status_before.is_connected_to_voice()
        || status_before.playback_state != PlaybackState::Idle;
    format_leave_outcome(app_core.leave().await, was_connected_or_active)
}

pub fn format_play_outcome(
    result: Result<BotStatus, CoreError>,
    voice_channel_id: VoiceChannelId,
) -> CommandReply {
    match result {
        Ok(status) => {
            let channel_mention = status
                .voice_channel_id()
                .map_or_else(|| format!("<#{voice_channel_id}>"), |id| format!("<#{id}>"));
            let track_label = status
                .active_track
                .as_ref()
                .map_or_else(|| "YouTube audio".to_string(), format_track_metadata);
            CommandReply::public(format!("Now playing {track_label} in {channel_mention}."))
        }
        Err(CoreError::InvalidUrl(error)) => {
            CommandReply::ephemeral(format!("Invalid YouTube URL: {error}."))
        }
        Err(CoreError::Cancelled) => CommandReply::public(
            "Playback request was superseded by a newer command before playback started.",
        ),
        Err(CoreError::Backend(error)) => {
            CommandReply::public(format!("Failed to start playback: {error}"))
        }
    }
}

pub fn format_stop_outcome(result: Result<BotStatus, CoreError>, was_active: bool) -> CommandReply {
    match result {
        Ok(_) if was_active => CommandReply::public("Stopped playback."),
        Ok(_) => CommandReply::public("Nothing is currently playing."),
        Err(error) => CommandReply::public(format!("Failed to stop playback: {error}")),
    }
}

pub fn format_leave_outcome(
    result: Result<BotStatus, CoreError>,
    was_connected_or_active: bool,
) -> CommandReply {
    match result {
        Ok(_) if was_connected_or_active => {
            CommandReply::public("Stopped playback and left the voice channel.")
        }
        Ok(_) => CommandReply::public("Not connected to a voice channel."),
        Err(error) => CommandReply::public(format!("Failed to leave voice channel: {error}")),
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

fn extract_play_url_option(command: &CommandInteraction) -> Option<String> {
    command
        .data
        .options()
        .into_iter()
        .find(|option| option.name == PLAY_URL_OPTION_NAME)
        .and_then(|option| match option.value {
            ResolvedValue::String(value) => Some(value.to_string()),
            _ => None,
        })
}

async fn send_immediate_reply(ctx: &Context, command: &CommandInteraction, reply: CommandReply) {
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

async fn edit_deferred_reply(ctx: &Context, command: &CommandInteraction, reply: CommandReply) {
    let edit = EditInteractionResponse::new().content(reply.content);
    if let Err(error) = command.edit_response(&ctx.http, edit).await {
        eprintln!(
            "error: failed to send deferred reply for slash command '/{}': {error}",
            command.data.name
        );
    }
}

struct DiscordHandler<B> {
    scope: CommandScope,
    app_core: Arc<AppCore<B>>,
    shutting_down: Arc<AtomicBool>,
    known_bots: Arc<Mutex<HashMap<u64, bool>>>,
}

impl<B> DiscordHandler<B> {
    fn lock_known_bots(&self) -> MutexGuard<'_, HashMap<u64, bool>> {
        self.known_bots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn record_bot_flag(&self, user_id: u64, is_bot: bool) {
        self.lock_known_bots().insert(user_id, is_bot);
    }

    async fn resolve_play_context(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
    ) -> PlayCommandContext {
        let guild_id = self.scope.guild_id();
        let bot_user_id = ctx.cache.current_user().id;

        let cached_voice_and_perms = ctx.cache.guild(guild_id).map(|guild| {
            let invoking_channel = guild
                .voice_states
                .get(&command.user.id)
                .and_then(|state| state.channel_id);

            let permissions = invoking_channel.and_then(|channel_id| {
                let channel = guild.channels.get(&channel_id)?;
                let bot_member = guild.members.get(&bot_user_id)?;
                Some(guild.user_permissions_in(channel, bot_member))
            });

            (invoking_channel, permissions)
        });

        let Some((invoking_channel, cached_permissions)) = cached_voice_and_perms else {
            return PlayCommandContext {
                invoking_voice_channel_id: None,
                bot_permissions: None,
            };
        };

        let Some(channel_id) = invoking_channel else {
            return PlayCommandContext {
                invoking_voice_channel_id: None,
                bot_permissions: None,
            };
        };

        if cached_permissions.is_some() {
            return PlayCommandContext {
                invoking_voice_channel_id: Some(channel_id.get()),
                bot_permissions: cached_permissions,
            };
        }

        let fetched_permissions = match guild_id.member(&ctx.http, bot_user_id).await {
            Ok(bot_member) => ctx.cache.guild(guild_id).and_then(|guild| {
                let channel = guild.channels.get(&channel_id)?;
                Some(guild.user_permissions_in(channel, &bot_member))
            }),
            Err(_) => None,
        };

        PlayCommandContext {
            invoking_voice_channel_id: Some(channel_id.get()),
            bot_permissions: fetched_permissions,
        }
    }

    fn collect_voice_channel_members(
        &self,
        ctx: &Context,
        guild_id: SerenityGuildId,
        channel_id: VoiceChannelId,
    ) -> Option<Vec<VoiceChannelMember>> {
        let target_channel = SerenityChannelId::new(channel_id.get());
        let known_bots = self.lock_known_bots();

        ctx.cache.guild(guild_id).map(|guild| {
            guild
                .voice_states
                .values()
                .filter(|state| state.channel_id == Some(target_channel))
                .map(|state| {
                    let user_id = state.user_id.get();
                    let is_bot = state
                        .member
                        .as_ref()
                        .map(|member| member.user.bot)
                        .or_else(|| {
                            guild
                                .members
                                .get(&state.user_id)
                                .map(|member| member.user.bot)
                        })
                        .or_else(|| known_bots.get(&user_id).copied())
                        .unwrap_or(false);
                    VoiceChannelMember { user_id, is_bot }
                })
                .collect()
        })
    }
}

#[async_trait]
impl<B: VoiceBackend + 'static> EventHandler for DiscordHandler<B> {
    async fn ready(&self, ctx: Context, ready: Ready) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }

        self.record_bot_flag(ready.user.id.get(), true);

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

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        if guild.id != self.scope.guild_id() {
            return;
        }

        let mut known_bots = self.lock_known_bots();
        for (user_id, member) in &guild.members {
            known_bots.insert(user_id.get(), member.user.bot);
        }
        for (user_id, state) in &guild.voice_states {
            if let Some(member) = state.member.as_ref() {
                known_bots.insert(user_id.get(), member.user.bot);
            }
        }
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }

        if let Some(member) = new.member.as_ref() {
            self.record_bot_flag(new.user_id.get(), member.user.bot);
        }

        if new.guild_id != Some(self.scope.guild_id()) {
            return;
        }

        let Some(core_guild_id) = self.scope.core_guild_id() else {
            return;
        };

        let bot_user_id = ctx.cache.current_user().id;
        if new.user_id == bot_user_id {
            match new.channel_id {
                Some(moved_channel) => {
                    if let Some(core_channel) = VoiceChannelId::new(moved_channel.get()) {
                        self.app_core.notify_voice_channel_moved(VoiceTarget::new(
                            core_guild_id,
                            core_channel,
                        ));
                    }
                }
                None => {
                    let old_channel_id = old
                        .and_then(|state| state.channel_id)
                        .map(SerenityChannelId::get);
                    let current_channel_id = self
                        .app_core
                        .status()
                        .voice_channel_id()
                        .map(VoiceChannelId::get);
                    if old_channel_id.is_some()
                        && old_channel_id == current_channel_id
                        && let Err(error) = self.app_core.leave().await
                    {
                        eprintln!(
                            "warning: failed to clean up voice state after disconnect: {error}"
                        );
                    }
                    return;
                }
            }
        }

        let status = self.app_core.status();
        let Some(target) = status.voice_target else {
            return;
        };

        if target.guild_id != core_guild_id {
            return;
        }

        let Some(members) =
            self.collect_voice_channel_members(&ctx, self.scope.guild_id(), target.channel_id)
        else {
            return;
        };

        if should_auto_leave_voice_channel(&status, core_guild_id, &members, bot_user_id.get()) {
            println!(
                "Voice channel {} has no non-bot members remaining; leaving automatically",
                target.channel_id
            );
            if let Err(error) = self.app_core.leave().await {
                eprintln!("warning: failed to auto-leave empty voice channel: {error}");
            }
        }
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

        self.record_bot_flag(command.user.id.get(), command.user.bot);

        let guild_id = command.guild_id.map(SerenityGuildId::get);
        let channel_id = command.channel_id.get();

        if let Some(rejection) =
            access_rejection_reply(self.scope.evaluate_access(guild_id, channel_id))
        {
            send_immediate_reply(&ctx, &command, rejection).await;
            return;
        }

        match command.data.name.as_str() {
            STATUS_COMMAND_NAME => {
                let reply = handle_slash_command(
                    self.scope,
                    &self.app_core,
                    guild_id,
                    channel_id,
                    STATUS_COMMAND_NAME,
                );
                send_immediate_reply(&ctx, &command, reply).await;
            }
            PLAY_COMMAND_NAME => {
                let Some(raw_url) = extract_play_url_option(&command) else {
                    send_immediate_reply(
                        &ctx,
                        &command,
                        CommandReply::ephemeral("Missing required `url` option for `/play`."),
                    )
                    .await;
                    return;
                };

                let play_context = self.resolve_play_context(&ctx, &command).await;
                let (voice_channel_id, validated_url) =
                    match validate_play_preflight(&raw_url, play_context) {
                        Ok(valid) => valid,
                        Err(reply) => {
                            send_immediate_reply(&ctx, &command, reply).await;
                            return;
                        }
                    };

                if let Err(error) = command.defer(&ctx.http).await {
                    eprintln!("error: failed to defer '/play' interaction: {error}");
                    return;
                }

                let Some(core_guild_id) = self.scope.core_guild_id() else {
                    edit_deferred_reply(
                        &ctx,
                        &command,
                        CommandReply::public("Zebot guild configuration is invalid."),
                    )
                    .await;
                    return;
                };

                let request =
                    PlayRequest::new(core_guild_id, voice_channel_id, validated_url.as_str());
                let play_result = self.app_core.play(request).await;
                let reply = match play_result {
                    Ok(status) => {
                        let bot_user_id = ctx.cache.current_user().id.get();
                        let became_empty = status.voice_target.is_some_and(|target| {
                            self.collect_voice_channel_members(
                                &ctx,
                                self.scope.guild_id(),
                                target.channel_id,
                            )
                            .is_some_and(|members| {
                                should_auto_leave_voice_channel(
                                    &status,
                                    core_guild_id,
                                    &members,
                                    bot_user_id,
                                )
                            })
                        });

                        if became_empty {
                            if let Err(error) = self.app_core.leave().await {
                                eprintln!(
                                    "warning: failed to leave voice channel that emptied during play: {error}"
                                );
                            }
                            CommandReply::public(format!(
                                "Everyone left <#{voice_channel_id}> while loading the track, so Zebot disconnected."
                            ))
                        } else {
                            format_play_outcome(Ok(status), voice_channel_id)
                        }
                    }
                    Err(error) => format_play_outcome(Err(error), voice_channel_id),
                };
                edit_deferred_reply(&ctx, &command, reply).await;
            }
            STOP_COMMAND_NAME => {
                if let Err(error) = command.defer(&ctx.http).await {
                    eprintln!("error: failed to defer '/stop' interaction: {error}");
                    return;
                }
                let reply =
                    execute_stop_command(self.scope, &self.app_core, guild_id, channel_id).await;
                edit_deferred_reply(&ctx, &command, reply).await;
            }
            LEAVE_COMMAND_NAME => {
                if let Err(error) = command.defer(&ctx.http).await {
                    eprintln!("error: failed to defer '/leave' interaction: {error}");
                    return;
                }
                let reply =
                    execute_leave_command(self.scope, &self.app_core, guild_id, channel_id).await;
                edit_deferred_reply(&ctx, &command, reply).await;
            }
            other => {
                send_immediate_reply(
                    &ctx,
                    &command,
                    CommandReply::ephemeral(format!("Unknown command '/{other}'.")),
                )
                .await;
            }
        }
    }
}

pub async fn run_discord_bot<B, S>(
    config: &Config,
    app_core: Arc<AppCore<B>>,
    songbird: Arc<Songbird>,
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
        known_bots: Arc::new(Mutex::new(HashMap::new())),
    };

    let mut client = Client::builder(&config.discord_token, required_gateway_intents())
        .event_handler(handler)
        .register_songbird_with(songbird)
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
    use serenity::model::permissions::Permissions;
    use songbird::{SerenityInit, Songbird};
    use tokio::sync::Notify;

    use super::{
        CommandAccessDecision, CommandReply, CommandScope, DiscordError, LEAVE_COMMAND_NAME,
        MissingVoicePermissions, PLAY_COMMAND_NAME, PLAY_URL_OPTION_NAME, PlayCommandContext,
        STATUS_COMMAND_NAME, STOP_COMMAND_NAME, VoiceChannelMember, build_slash_commands,
        check_voice_permissions, count_non_bot_members, execute_leave_command,
        execute_play_command, execute_stop_command, format_status_message, handle_slash_command,
        map_connection_stage, required_gateway_intents, run_with_shutdown,
        should_auto_leave_voice_channel, update_gateway_state, validate_play_preflight,
    };
    use crate::core::{
        AppCore, CancellationToken, PlaybackGeneration, UnavailableVoiceBackend, VoiceBackend,
        VoiceBackendError,
    };
    use crate::state::{
        BotStatus, DiscordConnectionState, GuildId, PlayRequest, PlaybackState, TrackMetadata,
        VoiceChannelId, VoiceTarget,
    };
    use crate::voice::build_songbird_config;
    use crate::youtube::YouTubeUrl;

    #[derive(Default)]
    struct RecordingBackend {
        join_targets: Mutex<Vec<VoiceTarget>>,
        play_video_ids: Mutex<Vec<String>>,
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
            target: VoiceTarget,
            _cancel: CancellationToken,
        ) -> Result<(), VoiceBackendError> {
            self.join_targets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(target);
            Ok(())
        }

        async fn play(
            &self,
            _target: VoiceTarget,
            track: &TrackMetadata,
            _generation: PlaybackGeneration,
            _cancel: CancellationToken,
        ) -> Result<(), VoiceBackendError> {
            self.play_video_ids
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(track.url.video_id().to_string());
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
    fn registers_status_play_stop_and_leave_slash_commands() {
        let commands = build_slash_commands();
        assert_eq!(commands.len(), 4);
        let debug_repr = format!("{commands:?}");
        assert!(debug_repr.contains(&format!("\"{STATUS_COMMAND_NAME}\"")));
        assert!(debug_repr.contains(&format!("\"{PLAY_COMMAND_NAME}\"")));
        assert!(debug_repr.contains(&format!("\"{PLAY_URL_OPTION_NAME}\"")));
        assert!(debug_repr.contains(&format!("\"{STOP_COMMAND_NAME}\"")));
        assert!(debug_repr.contains(&format!("\"{LEAVE_COMMAND_NAME}\"")));
    }

    #[tokio::test]
    async fn client_builder_initializes_with_rustls_and_songbird() {
        let songbird = Songbird::serenity_from_config(build_songbird_config());
        let client = serenity::client::Client::builder(
            "test-token-not-used-for-network",
            required_gateway_intents(),
        )
        .register_songbird_with(songbird)
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
    fn play_preflight_requires_voice_channel_permissions_and_valid_url() {
        let not_in_voice = validate_play_preflight(
            "https://youtu.be/dQw4w9WgXcQ",
            PlayCommandContext {
                invoking_voice_channel_id: None,
                bot_permissions: None,
            },
        )
        .unwrap_err();
        assert!(not_in_voice.ephemeral);
        assert!(not_in_voice.content.contains("join a voice channel"));

        let missing_speak = validate_play_preflight(
            "https://youtu.be/dQw4w9WgXcQ",
            PlayCommandContext {
                invoking_voice_channel_id: Some(777),
                bot_permissions: Some(Permissions::CONNECT),
            },
        )
        .unwrap_err();
        assert!(missing_speak.ephemeral);
        assert!(missing_speak.content.contains("Speak"));
        assert!(missing_speak.content.contains("<#777>"));

        let missing_both = validate_play_preflight(
            "https://youtu.be/dQw4w9WgXcQ",
            PlayCommandContext {
                invoking_voice_channel_id: Some(777),
                bot_permissions: Some(Permissions::VIEW_CHANNEL),
            },
        )
        .unwrap_err();
        assert!(missing_both.ephemeral);
        assert!(missing_both.content.contains("Connect and Speak"));

        let invalid_url = validate_play_preflight(
            "file:///etc/passwd",
            PlayCommandContext {
                invoking_voice_channel_id: Some(777),
                bot_permissions: Some(Permissions::CONNECT | Permissions::SPEAK),
            },
        )
        .unwrap_err();
        assert!(invalid_url.ephemeral);
        assert!(invalid_url.content.contains("Invalid YouTube URL"));

        let (channel_id, url) = validate_play_preflight(
            "https://youtu.be/dQw4w9WgXcQ",
            PlayCommandContext {
                invoking_voice_channel_id: Some(777),
                bot_permissions: Some(Permissions::CONNECT | Permissions::SPEAK),
            },
        )
        .unwrap();
        assert_eq!(channel_id.get(), 777);
        assert_eq!(url.video_id(), "dQw4w9WgXcQ");
    }

    #[test]
    fn voice_permission_checker_identifies_missing_connect_and_speak() {
        assert_eq!(
            check_voice_permissions(Permissions::CONNECT | Permissions::SPEAK),
            Ok(())
        );
        assert_eq!(
            check_voice_permissions(Permissions::SPEAK),
            Err(MissingVoicePermissions {
                connect: true,
                speak: false,
            })
        );
        assert_eq!(
            check_voice_permissions(Permissions::CONNECT),
            Err(MissingVoicePermissions {
                connect: false,
                speak: true,
            })
        );
        assert_eq!(
            check_voice_permissions(Permissions::empty()),
            Err(MissingVoicePermissions {
                connect: true,
                speak: true,
            })
        );
    }

    #[test]
    fn empty_voice_channel_detection_ignores_bots_and_only_triggers_when_connected() {
        let guild_id = GuildId::new(100).unwrap();
        let channel_id = VoiceChannelId::new(300).unwrap();
        let connected = BotStatus {
            discord_state: DiscordConnectionState::Connected,
            voice_target: Some(VoiceTarget::new(guild_id, channel_id)),
            playback_state: PlaybackState::Playing,
            active_track: None,
        };

        let only_bot = vec![VoiceChannelMember {
            user_id: 1,
            is_bot: true,
        }];
        let with_other_bot = vec![
            VoiceChannelMember {
                user_id: 1,
                is_bot: true,
            },
            VoiceChannelMember {
                user_id: 2,
                is_bot: true,
            },
        ];
        let with_human = vec![
            VoiceChannelMember {
                user_id: 1,
                is_bot: true,
            },
            VoiceChannelMember {
                user_id: 42,
                is_bot: false,
            },
        ];

        assert_eq!(count_non_bot_members(&only_bot, 1), 0);
        assert_eq!(count_non_bot_members(&with_other_bot, 1), 0);
        assert_eq!(count_non_bot_members(&with_human, 1), 1);

        assert!(should_auto_leave_voice_channel(
            &connected, guild_id, &only_bot, 1
        ));
        assert!(should_auto_leave_voice_channel(
            &connected,
            guild_id,
            &with_other_bot,
            1
        ));
        assert!(!should_auto_leave_voice_channel(
            &connected,
            guild_id,
            &with_human,
            1
        ));
        assert!(!should_auto_leave_voice_channel(
            &connected,
            GuildId::new(999).unwrap(),
            &only_bot,
            1
        ));
        assert!(!should_auto_leave_voice_channel(
            &BotStatus::default(),
            guild_id,
            &only_bot,
            1
        ));
    }

    #[tokio::test]
    async fn play_stop_and_leave_commands_delegate_to_app_core_and_move_channels() {
        let scope = CommandScope::new(100, 200).unwrap();
        let backend = Arc::new(RecordingBackend::default());
        let core = AppCore::new(Arc::clone(&backend));
        let perms = Some(Permissions::CONNECT | Permissions::SPEAK);

        let first_reply = execute_play_command(
            scope,
            &core,
            Some(100),
            200,
            "https://youtu.be/firstTrack01",
            PlayCommandContext {
                invoking_voice_channel_id: Some(501),
                bot_permissions: perms,
            },
        )
        .await;
        assert!(!first_reply.ephemeral);
        assert!(first_reply.content.contains("Sample Song"));
        assert!(first_reply.content.contains("<#501>"));
        assert_eq!(
            core.status().voice_channel_id().map(VoiceChannelId::get),
            Some(501)
        );
        assert_eq!(core.status().playback_state, PlaybackState::Playing);

        let moved_reply = execute_play_command(
            scope,
            &core,
            Some(100),
            200,
            "https://youtu.be/secondTrack2",
            PlayCommandContext {
                invoking_voice_channel_id: Some(502),
                bot_permissions: perms,
            },
        )
        .await;
        assert!(!moved_reply.ephemeral);
        assert!(moved_reply.content.contains("<#502>"));
        assert_eq!(
            core.status().voice_channel_id().map(VoiceChannelId::get),
            Some(502)
        );

        let stop_reply = execute_stop_command(scope, &core, Some(100), 200).await;
        assert_eq!(stop_reply, CommandReply::public("Stopped playback."));
        assert_eq!(core.status().playback_state, PlaybackState::Idle);
        assert_eq!(
            core.status().voice_channel_id().map(VoiceChannelId::get),
            Some(502)
        );

        let stop_again = execute_stop_command(scope, &core, Some(100), 200).await;
        assert_eq!(
            stop_again,
            CommandReply::public("Nothing is currently playing.")
        );

        let leave_reply = execute_leave_command(scope, &core, Some(100), 200).await;
        assert_eq!(
            leave_reply,
            CommandReply::public("Stopped playback and left the voice channel.")
        );
        assert_eq!(core.status().voice_target, None);

        let leave_again = execute_leave_command(scope, &core, Some(100), 200).await;
        assert_eq!(
            leave_again,
            CommandReply::public("Not connected to a voice channel.")
        );

        let joined: Vec<u64> = backend
            .join_targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|target| target.channel_id.get())
            .collect();
        assert_eq!(joined, vec![501, 502]);
        assert_eq!(backend.leave_calls.load(Ordering::SeqCst), 1);
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
