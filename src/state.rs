use std::fmt;
use std::num::NonZeroU64;

use crate::youtube::YouTubeUrl;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GuildId(u64);

impl GuildId {
    pub fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub fn from_non_zero(value: NonZeroU64) -> Self {
        Self(value.get())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for GuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VoiceChannelId(u64);

impl VoiceChannelId {
    pub fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub fn from_non_zero(value: NonZeroU64) -> Self {
        Self(value.get())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for VoiceChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(u64);

impl UserId {
    pub fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub fn from_non_zero(value: NonZeroU64) -> Self {
        Self(value.get())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VoiceChannelKind {
    Voice,
    Stage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceMemberInfo {
    pub user_id: UserId,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub is_bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceChannelInfo {
    pub id: VoiceChannelId,
    pub name: String,
    pub kind: VoiceChannelKind,
    pub members: Vec<VoiceMemberInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoiceTarget {
    pub guild_id: GuildId,
    pub channel_id: VoiceChannelId,
}

impl VoiceTarget {
    pub fn new(guild_id: GuildId, channel_id: VoiceChannelId) -> Self {
        Self {
            guild_id,
            channel_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DiscordConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PlaybackState {
    #[default]
    Idle,
    Loading,
    Playing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    pub url: YouTubeUrl,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub duration_seconds: Option<u64>,
}

impl TrackMetadata {
    pub fn from_url(url: YouTubeUrl) -> Self {
        Self {
            url,
            title: None,
            channel: None,
            duration_seconds: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BotStatus {
    pub discord_state: DiscordConnectionState,
    pub voice_target: Option<VoiceTarget>,
    pub playback_state: PlaybackState,
    pub active_track: Option<TrackMetadata>,
}

impl BotStatus {
    pub fn guild_id(&self) -> Option<GuildId> {
        self.voice_target.map(|target| target.guild_id)
    }

    pub fn voice_channel_id(&self) -> Option<VoiceChannelId> {
        self.voice_target.map(|target| target.channel_id)
    }

    pub fn is_connected_to_voice(&self) -> bool {
        self.voice_target.is_some()
    }

    pub fn is_playing(&self) -> bool {
        self.playback_state == PlaybackState::Playing
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRequest {
    pub target: VoiceTarget,
    pub url: String,
}

impl PlayRequest {
    pub fn new(
        guild_id: GuildId,
        voice_channel_id: VoiceChannelId,
        url: impl Into<String>,
    ) -> Self {
        Self {
            target: VoiceTarget::new(guild_id, voice_channel_id),
            url: url.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BotStatus, DiscordConnectionState, GuildId, PlaybackState, TrackMetadata, UserId,
        VoiceChannelId, VoiceTarget,
    };
    use crate::youtube::YouTubeUrl;

    #[test]
    fn rejects_zero_snowflakes_and_accepts_positive_ids() {
        assert_eq!(GuildId::new(0), None);
        assert_eq!(VoiceChannelId::new(0), None);
        assert_eq!(UserId::new(0), None);

        let guild_id = GuildId::new(100).unwrap();
        let channel_id = VoiceChannelId::new(200).unwrap();
        let user_id = UserId::new(300).unwrap();

        assert_eq!(guild_id.get(), 100);
        assert_eq!(channel_id.get(), 200);
        assert_eq!(user_id.get(), 300);
        assert_eq!(guild_id.to_string(), "100");
        assert_eq!(channel_id.to_string(), "200");
        assert_eq!(user_id.to_string(), "300");
    }

    #[test]
    fn bot_status_helpers_reflect_voice_and_playback_state() {
        let initial = BotStatus::default();
        assert_eq!(initial.discord_state, DiscordConnectionState::Disconnected);
        assert_eq!(initial.guild_id(), None);
        assert_eq!(initial.voice_channel_id(), None);
        assert!(!initial.is_connected_to_voice());
        assert!(!initial.is_playing());

        let guild_id = GuildId::new(1).unwrap();
        let channel_id = VoiceChannelId::new(2).unwrap();
        let url = YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap();

        let playing = BotStatus {
            discord_state: DiscordConnectionState::Connected,
            voice_target: Some(VoiceTarget::new(guild_id, channel_id)),
            playback_state: PlaybackState::Playing,
            active_track: Some(TrackMetadata::from_url(url)),
        };

        assert_eq!(playing.guild_id(), Some(guild_id));
        assert_eq!(playing.voice_channel_id(), Some(channel_id));
        assert!(playing.is_connected_to_voice());
        assert!(playing.is_playing());
    }
}
