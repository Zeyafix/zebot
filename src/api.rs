use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::core::{AppCore, CoreError, VoiceBackend};
use crate::state::{
    BotStatus, DiscordConnectionState, GuildId, PlayRequest, PlaybackState, TrackMetadata, UserId,
    VoiceChannelId, VoiceChannelInfo, VoiceChannelKind, VoiceMemberInfo, VoiceTarget,
};
use crate::youtube::YouTubeUrl;

pub const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024;
pub const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

const CONTENT_TYPE_JSON: &str = "application/json; charset=utf-8";
const BEARER_CHALLENGE: &str = "Bearer realm=\"zebot\"";

#[derive(Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub port: u16,
    pub token: String,
}

impl ApiConfig {
    pub fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            port,
            token: token.into(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self {
            port: config.api_port,
            token: config.api_token.clone(),
        }
    }
}

impl fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiConfig")
            .field("port", &self.port)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug)]
pub enum ApiServerError {
    EmptyToken,
    Bind { addr: SocketAddr, source: io::Error },
}

impl fmt::Display for ApiServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyToken => write!(f, "API_TOKEN must not be empty"),
            Self::Bind { addr, source } => {
                write!(f, "failed to bind local API server on {addr}: {source}")
            }
        }
    }
}

impl Error for ApiServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EmptyToken => None,
            Self::Bind { source, .. } => Some(source),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceDirectoryError {
    GuildUnavailable,
    VoiceChannelNotFound(VoiceChannelId),
    UserNotInVoiceChannel(UserId),
    MissingVoicePermissions {
        channel_id: VoiceChannelId,
        missing: &'static str,
    },
}

impl fmt::Display for VoiceDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GuildUnavailable => write!(
                f,
                "Discord guild cache is not ready yet; wait until GET /status reports discord_state \"connected\""
            ),
            Self::VoiceChannelNotFound(channel_id) => write!(
                f,
                "voice channel {channel_id} was not found in the configured Discord server"
            ),
            Self::UserNotInVoiceChannel(user_id) => write!(
                f,
                "user {user_id} is not currently in a voice channel in the configured Discord server"
            ),
            Self::MissingVoicePermissions {
                channel_id,
                missing,
            } => write!(
                f,
                "Zebot is missing the required {missing} permission(s) in voice channel {channel_id}"
            ),
        }
    }
}

impl Error for VoiceDirectoryError {}

pub trait VoiceDirectory: Send + Sync {
    fn guild_id(&self) -> GuildId;

    fn list_voice_channels(
        &self,
    ) -> impl Future<Output = Result<Vec<VoiceChannelInfo>, VoiceDirectoryError>> + Send;

    fn resolve_user_voice_channel(
        &self,
        user_id: UserId,
    ) -> impl Future<Output = Result<VoiceChannelId, VoiceDirectoryError>> + Send;

    fn validate_voice_channel(
        &self,
        channel_id: VoiceChannelId,
    ) -> impl Future<Output = Result<(), VoiceDirectoryError>> + Send;
}

impl<D: VoiceDirectory> VoiceDirectory for Arc<D> {
    fn guild_id(&self) -> GuildId {
        (**self).guild_id()
    }

    fn list_voice_channels(
        &self,
    ) -> impl Future<Output = Result<Vec<VoiceChannelInfo>, VoiceDirectoryError>> + Send {
        (**self).list_voice_channels()
    }

    fn resolve_user_voice_channel(
        &self,
        user_id: UserId,
    ) -> impl Future<Output = Result<VoiceChannelId, VoiceDirectoryError>> + Send {
        (**self).resolve_user_voice_channel(user_id)
    }

    fn validate_voice_channel(
        &self,
        channel_id: VoiceChannelId,
    ) -> impl Future<Output = Result<(), VoiceDirectoryError>> + Send {
        (**self).validate_voice_channel(channel_id)
    }
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff: u8 = u8::from(left.len() != right.len());

    for index in 0..max_len {
        let lhs = left.get(index).copied().unwrap_or(0);
        let rhs = right.get(index).copied().unwrap_or(0);
        diff |= lhs ^ rhs;
    }

    std::hint::black_box(diff) == 0
}

pub fn verify_bearer_authorization(headers: &HeaderMap, expected_token: &str) -> bool {
    if expected_token.is_empty() {
        return false;
    }

    let mut auth_headers = headers.get_all(header::AUTHORIZATION).iter();
    let Some(first_header) = auth_headers.next() else {
        return false;
    };

    if auth_headers.next().is_some() {
        return false;
    }

    let Ok(raw_value) = first_header.to_str() else {
        return false;
    };

    let Some((scheme, provided_token)) = raw_value.split_once(' ') else {
        return false;
    };

    if !scheme.eq_ignore_ascii_case("bearer")
        || provided_token.is_empty()
        || provided_token
            .bytes()
            .any(|byte| byte.is_ascii_whitespace())
    {
        return false;
    }

    constant_time_eq(provided_token.as_bytes(), expected_token.as_bytes())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotStatusResponse {
    pub discord_state: &'static str,
    pub voice_target: Option<VoiceTargetResponse>,
    pub playback_state: &'static str,
    pub active_track: Option<TrackMetadataResponse>,
}

impl From<BotStatus> for BotStatusResponse {
    fn from(status: BotStatus) -> Self {
        Self {
            discord_state: serialize_discord_state(status.discord_state),
            voice_target: status.voice_target.map(VoiceTargetResponse::from),
            playback_state: serialize_playback_state(status.playback_state),
            active_track: status.active_track.map(TrackMetadataResponse::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceTargetResponse {
    pub guild_id: String,
    pub channel_id: String,
}

impl From<VoiceTarget> for VoiceTargetResponse {
    fn from(target: VoiceTarget) -> Self {
        Self {
            guild_id: target.guild_id.to_string(),
            channel_id: target.channel_id.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackMetadataResponse {
    pub url: String,
    pub video_id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub duration_seconds: Option<u64>,
}

impl From<TrackMetadata> for TrackMetadataResponse {
    fn from(track: TrackMetadata) -> Self {
        Self {
            url: track.url.as_str().to_string(),
            video_id: track.url.video_id().to_string(),
            title: track.title,
            channel: track.channel,
            duration_seconds: track.duration_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceChannelsResponse {
    pub guild_id: String,
    pub discord_state: &'static str,
    pub channels: Vec<VoiceChannelResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceChannelResponse {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub members: Vec<VoiceMemberResponse>,
}

impl From<VoiceChannelInfo> for VoiceChannelResponse {
    fn from(channel: VoiceChannelInfo) -> Self {
        Self {
            id: channel.id.to_string(),
            name: channel.name,
            kind: match channel.kind {
                VoiceChannelKind::Voice => "voice",
                VoiceChannelKind::Stage => "stage",
            },
            members: channel
                .members
                .into_iter()
                .map(VoiceMemberResponse::from)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceMemberResponse {
    pub user_id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub is_bot: bool,
}

impl From<VoiceMemberInfo> for VoiceMemberResponse {
    fn from(member: VoiceMemberInfo) -> Self {
        Self {
            user_id: member.user_id.to_string(),
            username: member.username,
            display_name: member.display_name,
            is_bot: member.is_bot,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SnowflakeInput(u64);

impl<'de> Deserialize<'de> for SnowflakeInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SnowflakeVisitor;

        impl Visitor<'_> for SnowflakeVisitor {
            type Value = SnowflakeInput;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a positive Discord snowflake ID as a string or integer")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == 0 {
                    return Err(E::custom("Discord ID must be greater than zero"));
                }
                Ok(SnowflakeInput(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(E::custom(
                        "Discord ID string must contain only ASCII digits",
                    ));
                }
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| E::custom("Discord ID is out of u64 range"))?;
                if parsed == 0 {
                    return Err(E::custom("Discord ID must be greater than zero"));
                }
                Ok(SnowflakeInput(parsed))
            }
        }

        deserializer.deserialize_any(SnowflakeVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayApiRequest {
    url: String,
    voice_channel_id: Option<SnowflakeInput>,
    user_id: Option<SnowflakeInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyObjectRequest {}

fn serialize_discord_state(state: DiscordConnectionState) -> &'static str {
    match state {
        DiscordConnectionState::Disconnected => "disconnected",
        DiscordConnectionState::Connecting => "connecting",
        DiscordConnectionState::Connected => "connected",
        DiscordConnectionState::ShuttingDown => "shutting_down",
    }
}

fn serialize_playback_state(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Idle => "idle",
        PlaybackState::Loading => "loading",
        PlaybackState::Playing => "playing",
    }
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(raw) = value.to_str() else {
        return false;
    };
    let media_type = raw.split(';').next().unwrap_or("").trim();
    media_type.eq_ignore_ascii_case("application/json")
}

fn content_length_exceeds_limit(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_LENGTH) else {
        return false;
    };
    let Ok(raw) = value.to_str() else {
        return false;
    };
    match raw.trim().parse::<u64>() {
        Ok(length) => length > MAX_REQUEST_BODY_BYTES as u64,
        Err(_) => false,
    }
}

enum BodyReadError {
    PayloadTooLarge,
    Timeout,
    ReadFailed,
}

async fn read_bounded_body(headers: &HeaderMap, body: Incoming) -> Result<Bytes, BodyReadError> {
    if content_length_exceeds_limit(headers) {
        return Err(BodyReadError::PayloadTooLarge);
    }

    let limited = Limited::new(body, MAX_REQUEST_BODY_BYTES);
    match tokio::time::timeout(REQUEST_READ_TIMEOUT, limited.collect()).await {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(error)) => {
            if error.downcast_ref::<LengthLimitError>().is_some() {
                Err(BodyReadError::PayloadTooLarge)
            } else {
                Err(BodyReadError::ReadFailed)
            }
        }
        Err(_) => Err(BodyReadError::Timeout),
    }
}

fn body_read_error_response(error: BodyReadError) -> Response<Full<Bytes>> {
    match error {
        BodyReadError::PayloadTooLarge => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("request body exceeds maximum allowed size of {MAX_REQUEST_BODY_BYTES} bytes"),
            None,
        ),
        BodyReadError::Timeout => error_response(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "timed out while reading request body",
            None,
        ),
        BodyReadError::ReadFailed => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "failed to read request body",
            None,
        ),
    }
}

fn build_json_response(
    status: StatusCode,
    payload: Vec<u8>,
    extra_header: Option<(HeaderName, HeaderValue)>,
) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(payload)));
    *response.status_mut() = status;

    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(CONTENT_TYPE_JSON),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );

    if let Some((name, value)) = extra_header {
        headers.insert(name, value);
    }

    response
}

fn json_ok_response<T: Serialize>(value: &T) -> Response<Full<Bytes>> {
    match serde_json::to_vec(value) {
        Ok(bytes) => build_json_response(StatusCode::OK, bytes, None),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "serialization_error",
            format!("failed to serialize response JSON: {error}"),
            None,
        ),
    }
}

fn error_response(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    extra_header: Option<(HeaderName, HeaderValue)>,
) -> Response<Full<Bytes>> {
    let envelope = ErrorEnvelope {
        error: ErrorDetail {
            code,
            message: message.into(),
        },
    };
    let payload = serde_json::to_vec(&envelope).unwrap_or_else(|_| {
        b"{\"error\":{\"code\":\"internal_error\",\"message\":\"failed to serialize error\"}}"
            .to_vec()
    });
    build_json_response(status, payload, extra_header)
}

fn directory_error_response(error: VoiceDirectoryError) -> Response<Full<Bytes>> {
    match error {
        VoiceDirectoryError::GuildUnavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "guild_unavailable",
            error.to_string(),
            None,
        ),
        VoiceDirectoryError::VoiceChannelNotFound(_) => error_response(
            StatusCode::NOT_FOUND,
            "voice_channel_not_found",
            error.to_string(),
            None,
        ),
        VoiceDirectoryError::UserNotInVoiceChannel(_) => error_response(
            StatusCode::CONFLICT,
            "user_not_in_voice_channel",
            error.to_string(),
            None,
        ),
        VoiceDirectoryError::MissingVoicePermissions { .. } => error_response(
            StatusCode::FORBIDDEN,
            "missing_voice_permissions",
            error.to_string(),
            None,
        ),
    }
}

fn core_error_response(error: CoreError) -> Response<Full<Bytes>> {
    match error {
        CoreError::InvalidUrl(url_error) => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_url",
            url_error.to_string(),
            None,
        ),
        CoreError::Cancelled => {
            error_response(StatusCode::CONFLICT, "cancelled", error.to_string(), None)
        }
        CoreError::Backend(backend_error) => error_response(
            StatusCode::BAD_GATEWAY,
            "voice_backend_error",
            backend_error.to_string(),
            None,
        ),
    }
}

struct ApiState<B, D> {
    token: String,
    app_core: Arc<AppCore<B>>,
    directory: Arc<D>,
}

async fn handle_request<B, D>(
    state: Arc<ApiState<B, D>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    B: VoiceBackend + 'static,
    D: VoiceDirectory + 'static,
{
    if !verify_bearer_authorization(request.headers(), &state.token) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid Bearer token",
            Some((
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(BEARER_CHALLENGE),
            )),
        );
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();

    match (method, path.as_str()) {
        (Method::GET, "/status") => handle_get_status(state, request).await,
        (Method::GET, "/voice-channels") => handle_get_voice_channels(state, request).await,
        (Method::POST, "/play") => handle_post_play(state, request).await,
        (Method::POST, "/stop") => handle_post_stop(state, request).await,
        (Method::POST, "/leave") => handle_post_leave(state, request).await,
        (_, "/status" | "/voice-channels") => error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("method not allowed for {path}; expected GET"),
            Some((header::ALLOW, HeaderValue::from_static("GET"))),
        ),
        (_, "/play" | "/stop" | "/leave") => error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("method not allowed for {path}; expected POST"),
            Some((header::ALLOW, HeaderValue::from_static("POST"))),
        ),
        _ => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("unknown endpoint '{path}'"),
            None,
        ),
    }
}

async fn handle_get_status<B, D>(
    state: Arc<ApiState<B, D>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    if let Err(error) = read_bounded_body(&parts.headers, body).await {
        return body_read_error_response(error);
    }

    let response = BotStatusResponse::from(state.app_core.status());
    json_ok_response(&response)
}

async fn handle_get_voice_channels<B, D>(
    state: Arc<ApiState<B, D>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    D: VoiceDirectory + 'static,
{
    let (parts, body) = request.into_parts();
    if let Err(error) = read_bounded_body(&parts.headers, body).await {
        return body_read_error_response(error);
    }

    let status = state.app_core.status();
    match state.directory.list_voice_channels().await {
        Ok(channels) => {
            let payload = VoiceChannelsResponse {
                guild_id: state.directory.guild_id().to_string(),
                discord_state: serialize_discord_state(status.discord_state),
                channels: channels
                    .into_iter()
                    .map(VoiceChannelResponse::from)
                    .collect(),
            };
            json_ok_response(&payload)
        }
        Err(error) => directory_error_response(error),
    }
}

async fn handle_post_play<B, D>(
    state: Arc<ApiState<B, D>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    B: VoiceBackend + 'static,
    D: VoiceDirectory + 'static,
{
    let (parts, body) = request.into_parts();
    if !is_json_content_type(&parts.headers) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Content-Type must be application/json",
            None,
        );
    }

    let raw_body = match read_bounded_body(&parts.headers, body).await {
        Ok(bytes) => bytes,
        Err(error) => return body_read_error_response(error),
    };

    let payload: PlayApiRequest = match serde_json::from_slice(&raw_body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                format!("invalid JSON payload: {error}"),
                None,
            );
        }
    };

    let target = match (payload.voice_channel_id, payload.user_id) {
        (Some(channel), None) => match VoiceChannelId::new(channel.0) {
            Some(channel_id) => PlayApiTarget::Channel(channel_id),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "voice_channel_id must be a positive Discord snowflake ID",
                    None,
                );
            }
        },
        (None, Some(user)) => match UserId::new(user.0) {
            Some(user_id) => PlayApiTarget::User(user_id),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "user_id must be a positive Discord snowflake ID",
                    None,
                );
            }
        },
        (None, None) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "missing_target",
                "specify either voice_channel_id or user_id",
                None,
            );
        }
        (Some(_), Some(_)) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "ambiguous_target",
                "specify either voice_channel_id or user_id, not both",
                None,
            );
        }
    };

    let validated_url = match YouTubeUrl::parse(&payload.url) {
        Ok(url) => url,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_url",
                error.to_string(),
                None,
            );
        }
    };

    let channel_id = match target {
        PlayApiTarget::Channel(channel_id) => {
            if let Err(error) = state.directory.validate_voice_channel(channel_id).await {
                return directory_error_response(error);
            }
            channel_id
        }
        PlayApiTarget::User(user_id) => {
            let resolved = match state.directory.resolve_user_voice_channel(user_id).await {
                Ok(channel_id) => channel_id,
                Err(error) => return directory_error_response(error),
            };
            if let Err(error) = state.directory.validate_voice_channel(resolved).await {
                return directory_error_response(error);
            }
            resolved
        }
    };

    let play_request = PlayRequest::new(
        state.directory.guild_id(),
        channel_id,
        validated_url.as_str(),
    );

    match state.app_core.play(play_request).await {
        Ok(status) => json_ok_response(&BotStatusResponse::from(status)),
        Err(error) => core_error_response(error),
    }
}

enum PlayApiTarget {
    Channel(VoiceChannelId),
    User(UserId),
}

async fn validate_optional_empty_body(
    headers: &HeaderMap,
    body: Incoming,
) -> Option<Response<Full<Bytes>>> {
    let raw_body = match read_bounded_body(headers, body).await {
        Ok(bytes) => bytes,
        Err(error) => return Some(body_read_error_response(error)),
    };

    if raw_body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return None;
    }

    if !is_json_content_type(headers) {
        return Some(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Content-Type must be application/json when sending a request body",
            None,
        ));
    }

    if let Err(error) = serde_json::from_slice::<EmptyObjectRequest>(&raw_body) {
        return Some(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            format!("expected empty body or empty JSON object {{}}: {error}"),
            None,
        ));
    }

    None
}

async fn handle_post_stop<B, D>(
    state: Arc<ApiState<B, D>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    B: VoiceBackend + 'static,
{
    let (parts, body) = request.into_parts();
    if let Some(response) = validate_optional_empty_body(&parts.headers, body).await {
        return response;
    }

    match state.app_core.stop().await {
        Ok(status) => json_ok_response(&BotStatusResponse::from(status)),
        Err(error) => core_error_response(error),
    }
}

async fn handle_post_leave<B, D>(
    state: Arc<ApiState<B, D>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    B: VoiceBackend + 'static,
{
    let (parts, body) = request.into_parts();
    if let Some(response) = validate_optional_empty_body(&parts.headers, body).await {
        return response;
    }

    match state.app_core.leave().await {
        Ok(status) => json_ok_response(&BotStatusResponse::from(status)),
        Err(error) => core_error_response(error),
    }
}

pub struct BoundApiServer<B, D> {
    listener: TcpListener,
    local_addr: SocketAddr,
    state: Arc<ApiState<B, D>>,
}

impl<B, D> BoundApiServer<B, D>
where
    B: VoiceBackend + 'static,
    D: VoiceDirectory + 'static,
{
    pub async fn bind(
        config: ApiConfig,
        app_core: Arc<AppCore<B>>,
        directory: Arc<D>,
    ) -> Result<Self, ApiServerError> {
        let token = config.token.trim().to_string();
        if token.is_empty() {
            return Err(ApiServerError::EmptyToken);
        }

        let bind_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let listener =
            TcpListener::bind(bind_addr)
                .await
                .map_err(|source| ApiServerError::Bind {
                    addr: bind_addr,
                    source,
                })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| ApiServerError::Bind {
                addr: bind_addr,
                source,
            })?;

        Ok(Self {
            listener,
            local_addr,
            state: Arc::new(ApiState {
                token,
                app_core,
                directory,
            }),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn serve<S>(self, shutdown_signal: S) -> Result<(), ApiServerError>
    where
        S: Future<Output = ()> + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut connections = JoinSet::new();
        let mut shutdown_fut = pin!(shutdown_signal);

        loop {
            while connections.try_join_next().is_some() {}

            tokio::select! {
                () = &mut shutdown_fut => {
                    let _ = shutdown_tx.send(true);
                    break;
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _peer_addr)) => {
                            let state = Arc::clone(&self.state);
                            let mut conn_shutdown = shutdown_rx.clone();
                            connections.spawn(async move {
                                let io = TokioIo::new(stream);
                                let service = service_fn(move |req| {
                                    let req_state = Arc::clone(&state);
                                    async move {
                                        Ok::<_, std::convert::Infallible>(
                                            handle_request(req_state, req).await,
                                        )
                                    }
                                });

                                let mut builder = http1::Builder::new();
                                builder
                                    .timer(TokioTimer::new())
                                    .header_read_timeout(REQUEST_READ_TIMEOUT)
                                    .keep_alive(true);

                                let mut conn = pin!(builder.serve_connection(io, service));

                                tokio::select! {
                                    _ = conn.as_mut() => {}
                                    _ = conn_shutdown.changed() => {
                                        conn.as_mut().graceful_shutdown();
                                        let _ = tokio::time::timeout(
                                            SHUTDOWN_DRAIN_TIMEOUT,
                                            conn.as_mut(),
                                        )
                                        .await;
                                    }
                                }
                            });
                        }
                        Err(error) => {
                            eprintln!("warning: local API accept error: {error}");
                        }
                    }
                }
            }
        }

        let _ = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.abort_all();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;
    use http::header::{self, HeaderValue};

    use super::{
        ApiConfig, BotStatusResponse, PlayApiRequest, constant_time_eq, is_json_content_type,
        verify_bearer_authorization,
    };
    use crate::state::{
        BotStatus, DiscordConnectionState, GuildId, PlaybackState, TrackMetadata, VoiceChannelId,
        VoiceTarget,
    };
    use crate::youtube::YouTubeUrl;

    #[test]
    fn constant_time_eq_compares_bytes_accurately() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
        assert!(!constant_time_eq(b"secret", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret"));
    }

    #[test]
    fn verify_bearer_authorization_enforces_single_exact_bearer_token() {
        let mut valid = HeaderMap::new();
        valid.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer top-secret-token"),
        );
        assert!(verify_bearer_authorization(&valid, "top-secret-token"));
        assert!(!verify_bearer_authorization(&valid, "wrong-token"));
        assert!(!verify_bearer_authorization(&valid, ""));

        let mut lowercase_scheme = HeaderMap::new();
        lowercase_scheme.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer top-secret-token"),
        );
        assert!(verify_bearer_authorization(
            &lowercase_scheme,
            "top-secret-token"
        ));

        let mut wrong_scheme = HeaderMap::new();
        wrong_scheme.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic top-secret-token"),
        );
        assert!(!verify_bearer_authorization(
            &wrong_scheme,
            "top-secret-token"
        ));

        let mut extra_whitespace = HeaderMap::new();
        extra_whitespace.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer  top-secret-token"),
        );
        assert!(!verify_bearer_authorization(
            &extra_whitespace,
            "top-secret-token"
        ));

        let mut duplicate = HeaderMap::new();
        duplicate.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer top-secret-token"),
        );
        duplicate.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer top-secret-token"),
        );
        assert!(!verify_bearer_authorization(&duplicate, "top-secret-token"));

        assert!(!verify_bearer_authorization(
            &HeaderMap::new(),
            "top-secret-token"
        ));
    }

    #[test]
    fn api_config_redacts_token_in_debug_output() {
        let config = ApiConfig::new(8080, "super-secret-value");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn content_type_check_accepts_json_with_parameters_only() {
        let mut plain_json = HeaderMap::new();
        plain_json.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(is_json_content_type(&plain_json));

        let mut with_charset = HeaderMap::new();
        with_charset.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Application/JSON; charset=utf-8"),
        );
        assert!(is_json_content_type(&with_charset));

        let mut text_plain = HeaderMap::new();
        text_plain.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!is_json_content_type(&text_plain));

        assert!(!is_json_content_type(&HeaderMap::new()));
    }

    #[test]
    fn play_request_deserializes_string_and_integer_snowflakes_and_rejects_zero() {
        let from_str: PlayApiRequest = serde_json::from_str(
            r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"1234567890123456789"}"#,
        )
        .unwrap();
        assert_eq!(
            from_str.voice_channel_id.unwrap().0,
            1_234_567_890_123_456_789
        );
        assert!(from_str.user_id.is_none());

        let from_num: PlayApiRequest =
            serde_json::from_str(r#"{"url":"https://youtu.be/dQw4w9WgXcQ","user_id":987654321}"#)
                .unwrap();
        assert_eq!(from_num.user_id.unwrap().0, 987_654_321);

        assert!(
            serde_json::from_str::<PlayApiRequest>(
                r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"0"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<PlayApiRequest>(
                r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":0}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<PlayApiRequest>(
                r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"abc"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<PlayApiRequest>(
                r#"{"url":"https://youtu.be/dQw4w9WgXcQ","unknown_field":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn bot_status_response_serializes_snowflakes_as_strings() {
        let status = BotStatus {
            discord_state: DiscordConnectionState::Connected,
            voice_target: Some(VoiceTarget::new(
                GuildId::new(111_222_333_444_555_666).unwrap(),
                VoiceChannelId::new(222_333_444_555_666_777).unwrap(),
            )),
            playback_state: PlaybackState::Playing,
            active_track: Some(TrackMetadata {
                url: YouTubeUrl::parse("https://youtu.be/dQw4w9WgXcQ").unwrap(),
                title: Some("Sample Song".to_string()),
                channel: Some("Sample Artist".to_string()),
                duration_seconds: Some(213),
            }),
        };

        let value = serde_json::to_value(BotStatusResponse::from(status)).unwrap();
        assert_eq!(value["discord_state"], "connected");
        assert_eq!(value["voice_target"]["guild_id"], "111222333444555666");
        assert_eq!(value["voice_target"]["channel_id"], "222333444555666777");
        assert_eq!(value["playback_state"], "playing");
        assert_eq!(
            value["active_track"]["url"],
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(value["active_track"]["video_id"], "dQw4w9WgXcQ");
        assert_eq!(value["active_track"]["title"], "Sample Song");
        assert_eq!(value["active_track"]["channel"], "Sample Artist");
        assert_eq!(value["active_track"]["duration_seconds"], 213);
    }
}
