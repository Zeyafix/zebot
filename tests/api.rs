use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use zebot::api::{
    ApiConfig, ApiServerError, BoundApiServer, MAX_REQUEST_BODY_BYTES, VoiceDirectory,
    VoiceDirectoryError,
};
use zebot::core::{
    AppCore, CancellationToken, PlaybackGeneration, VoiceBackend, VoiceBackendError,
};
use zebot::discord::{
    CommandScope, PlayCommandContext, execute_leave_command, execute_play_command,
    execute_stop_command, handle_slash_command,
};
use zebot::state::{
    DiscordConnectionState, GuildId, PlaybackState, TrackMetadata, UserId, VoiceChannelId,
    VoiceChannelInfo, VoiceChannelKind, VoiceMemberInfo, VoiceTarget,
};
use zebot::youtube::YouTubeUrl;

const TEST_TOKEN: &str = "test-api-token-9f8e7d6c5b4a";
const TEST_GUILD_ID: u64 = 111_222_333_444_555_666;
const TEST_ALLOWED_CHANNEL_ID: u64 = 999_888_777_666_555_444;
const VOICE_CHANNEL_A: u64 = 222_333_444_555_666_777;
const VOICE_CHANNEL_B: u64 = 222_333_444_555_666_888;
const VOICE_CHANNEL_NO_PERMS: u64 = 222_333_444_555_666_999;
const USER_ALICE_ID: u64 = 333_444_555_666_777_888;
const USER_BOB_ID: u64 = 333_444_555_666_777_999;
const USER_OFFLINE_ID: u64 = 444_555_666_777_888_999;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

fn non_zero(value: u64) -> Result<NonZeroU64, io::Error> {
    NonZeroU64::new(value)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected non-zero ID"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedBackendAction {
    Resolve(String),
    Join(VoiceTarget),
    Play {
        target: VoiceTarget,
        video_id: String,
    },
    Stop(GuildId),
    Leave(GuildId),
}

#[derive(Default)]
struct FakeBackendState {
    actions: Vec<RecordedBackendAction>,
    fail_resolve: HashMap<String, String>,
    resolve_gates: HashMap<String, (Arc<Notify>, Arc<Notify>)>,
}

#[derive(Default)]
struct FakeVoiceBackend {
    inner: Mutex<FakeBackendState>,
}

impl FakeVoiceBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, FakeBackendState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn actions(&self) -> Vec<RecordedBackendAction> {
        self.lock().actions.clone()
    }

    fn fail_resolve(&self, video_id: &str, message: &str) {
        self.lock()
            .fail_resolve
            .insert(video_id.to_string(), message.to_string());
    }

    fn install_resolve_gate(&self, video_id: &str) -> (Arc<Notify>, Arc<Notify>) {
        let pair = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        self.lock().resolve_gates.insert(
            video_id.to_string(),
            (Arc::clone(&pair.0), Arc::clone(&pair.1)),
        );
        pair
    }
}

impl VoiceBackend for FakeVoiceBackend {
    async fn resolve_track(
        &self,
        url: &YouTubeUrl,
        cancel: CancellationToken,
    ) -> Result<TrackMetadata, VoiceBackendError> {
        let video_id = url.video_id().to_string();
        let (planned_error, gate) = {
            let mut state = self.lock();
            state
                .actions
                .push(RecordedBackendAction::Resolve(video_id.clone()));
            (
                state.fail_resolve.get(&video_id).cloned(),
                state.resolve_gates.get(&video_id).cloned(),
            )
        };

        if let Some((started, release)) = gate {
            started.notify_waiters();
            let proceeded = tokio::select! {
                () = cancel.cancelled() => false,
                () = release.notified() => true,
            };
            if !proceeded {
                return Err(VoiceBackendError::Cancelled);
            }
        }

        if cancel.is_cancelled() {
            return Err(VoiceBackendError::Cancelled);
        }

        if let Some(reason) = planned_error {
            return Err(VoiceBackendError::ResolveFailed(reason));
        }

        Ok(TrackMetadata {
            url: url.clone(),
            title: Some(format!("Track {video_id}")),
            channel: Some("Zebot Test Artist".to_string()),
            duration_seconds: Some(185),
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
        self.lock()
            .actions
            .push(RecordedBackendAction::Join(target));
        Ok(())
    }

    async fn play(
        &self,
        target: VoiceTarget,
        track: &TrackMetadata,
        _generation: PlaybackGeneration,
        cancel: CancellationToken,
    ) -> Result<(), VoiceBackendError> {
        if cancel.is_cancelled() {
            return Err(VoiceBackendError::Cancelled);
        }
        self.lock().actions.push(RecordedBackendAction::Play {
            target,
            video_id: track.url.video_id().to_string(),
        });
        Ok(())
    }

    async fn stop(&self, guild_id: GuildId) -> Result<(), VoiceBackendError> {
        self.lock()
            .actions
            .push(RecordedBackendAction::Stop(guild_id));
        Ok(())
    }

    async fn leave(&self, guild_id: GuildId) -> Result<(), VoiceBackendError> {
        self.lock()
            .actions
            .push(RecordedBackendAction::Leave(guild_id));
        Ok(())
    }
}

struct FakeDirectoryState {
    guild_available: bool,
    channels: Vec<VoiceChannelInfo>,
    missing_permissions: HashMap<VoiceChannelId, &'static str>,
}

struct FakeVoiceDirectory {
    guild_id: GuildId,
    inner: Mutex<FakeDirectoryState>,
}

impl FakeVoiceDirectory {
    fn populated() -> Result<Arc<Self>, io::Error> {
        let guild_id = GuildId::from_non_zero(non_zero(TEST_GUILD_ID)?);
        let channel_a = VoiceChannelId::from_non_zero(non_zero(VOICE_CHANNEL_A)?);
        let channel_b = VoiceChannelId::from_non_zero(non_zero(VOICE_CHANNEL_B)?);
        let channel_restricted = VoiceChannelId::from_non_zero(non_zero(VOICE_CHANNEL_NO_PERMS)?);
        let alice = UserId::from_non_zero(non_zero(USER_ALICE_ID)?);
        let bob = UserId::from_non_zero(non_zero(USER_BOB_ID)?);

        let channels = vec![
            VoiceChannelInfo {
                id: channel_a,
                name: "Lounge".to_string(),
                kind: VoiceChannelKind::Voice,
                members: vec![VoiceMemberInfo {
                    user_id: alice,
                    username: Some("alice".to_string()),
                    display_name: Some("Alice".to_string()),
                    is_bot: false,
                }],
            },
            VoiceChannelInfo {
                id: channel_b,
                name: "Stage Room".to_string(),
                kind: VoiceChannelKind::Stage,
                members: vec![VoiceMemberInfo {
                    user_id: bob,
                    username: Some("bob".to_string()),
                    display_name: None,
                    is_bot: false,
                }],
            },
            VoiceChannelInfo {
                id: channel_restricted,
                name: "Staff Only".to_string(),
                kind: VoiceChannelKind::Voice,
                members: Vec::new(),
            },
        ];

        let mut missing_permissions = HashMap::new();
        missing_permissions.insert(channel_restricted, "Connect and Speak");

        Ok(Arc::new(Self {
            guild_id,
            inner: Mutex::new(FakeDirectoryState {
                guild_available: true,
                channels,
                missing_permissions,
            }),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, FakeDirectoryState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_guild_available(&self, available: bool) {
        self.lock().guild_available = available;
    }
}

impl VoiceDirectory for FakeVoiceDirectory {
    fn guild_id(&self) -> GuildId {
        self.guild_id
    }

    async fn list_voice_channels(&self) -> Result<Vec<VoiceChannelInfo>, VoiceDirectoryError> {
        let state = self.lock();
        if !state.guild_available {
            return Err(VoiceDirectoryError::GuildUnavailable);
        }
        Ok(state.channels.clone())
    }

    async fn resolve_user_voice_channel(
        &self,
        user_id: UserId,
    ) -> Result<VoiceChannelId, VoiceDirectoryError> {
        let state = self.lock();
        if !state.guild_available {
            return Err(VoiceDirectoryError::GuildUnavailable);
        }
        for channel in &state.channels {
            if channel
                .members
                .iter()
                .any(|member| member.user_id == user_id)
            {
                return Ok(channel.id);
            }
        }
        Err(VoiceDirectoryError::UserNotInVoiceChannel(user_id))
    }

    async fn validate_voice_channel(
        &self,
        channel_id: VoiceChannelId,
    ) -> Result<(), VoiceDirectoryError> {
        let state = self.lock();
        if !state.guild_available {
            return Err(VoiceDirectoryError::GuildUnavailable);
        }
        let known_ids: HashSet<VoiceChannelId> = state.channels.iter().map(|ch| ch.id).collect();
        if !known_ids.contains(&channel_id) {
            return Err(VoiceDirectoryError::VoiceChannelNotFound(channel_id));
        }
        if let Some(missing) = state.missing_permissions.get(&channel_id) {
            return Err(VoiceDirectoryError::MissingVoicePermissions {
                channel_id,
                missing,
            });
        }
        Ok(())
    }
}

struct TestHarness {
    addr: SocketAddr,
    app_core: Arc<AppCore<Arc<FakeVoiceBackend>>>,
    backend: Arc<FakeVoiceBackend>,
    directory: Arc<FakeVoiceDirectory>,
    shutdown: CancellationToken,
    server_task: JoinHandle<Result<(), ApiServerError>>,
}

impl TestHarness {
    async fn start() -> TestResult<Self> {
        let backend = FakeVoiceBackend::new();
        let app_core = Arc::new(AppCore::new(Arc::clone(&backend)));
        app_core.set_discord_state(DiscordConnectionState::Connected);
        let directory = FakeVoiceDirectory::populated()?;

        let server = BoundApiServer::bind(
            ApiConfig::new(0, TEST_TOKEN),
            Arc::clone(&app_core),
            Arc::clone(&directory),
        )
        .await?;

        let addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server_task = tokio::spawn(async move {
            server
                .serve(async move {
                    server_shutdown.cancelled().await;
                })
                .await
        });

        Ok(Self {
            addr,
            app_core,
            backend,
            directory,
            shutdown,
            server_task,
        })
    }

    async fn stop(self) -> TestResult {
        self.shutdown.cancel();
        let join_result = tokio::time::timeout(Duration::from_secs(5), self.server_task).await?;
        let serve_result = join_result?;
        serve_result?;
        Ok(())
    }
}

#[derive(Debug)]
struct RawHttpResponse {
    status_code: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl RawHttpResponse {
    fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.body)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn has_any_cors_header(&self) -> bool {
        self.headers
            .keys()
            .any(|key| key.starts_with("access-control-"))
    }
}

async fn send_raw_http(addr: SocketAddr, raw_request: &[u8]) -> TestResult<RawHttpResponse> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(raw_request).await?;
    stream.flush().await?;

    let mut buffer = Vec::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buffer)).await?;
    let _ = read_result?;

    parse_http_response(&buffer)
}

fn parse_http_response(raw: &[u8]) -> TestResult<RawHttpResponse> {
    let text = String::from_utf8(raw.to_vec())?;
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing HTTP header/body separator",
        )
        .into());
    };

    let mut lines = head.split("\r\n");
    let Some(status_line) = lines.next() else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status line").into());
    };
    let Some(status_token) = status_line.split_whitespace().nth(1) else {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status code token").into(),
        );
    };
    let status_code = status_token.parse::<u16>()?;

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    Ok(RawHttpResponse {
        status_code,
        headers,
        body: body.to_string(),
    })
}

fn build_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Vec<u8> {
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(payload) = body {
        raw.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        raw.push_str("\r\n");
        raw.push_str(payload);
    } else {
        raw.push_str("\r\n");
    }
    raw.into_bytes()
}

fn auth_header() -> String {
    format!("Bearer {TEST_TOKEN}")
}

#[tokio::test]
async fn server_binds_only_to_loopback_and_rejects_empty_token() {
    let backend = FakeVoiceBackend::new();
    let app_core = Arc::new(AppCore::new(backend));
    let directory = FakeVoiceDirectory::populated().expect("populated directory");

    let empty_err = BoundApiServer::bind(
        ApiConfig::new(0, "   "),
        Arc::clone(&app_core),
        Arc::clone(&directory),
    )
    .await
    .err()
    .expect("empty token must be rejected");
    assert!(matches!(empty_err, ApiServerError::EmptyToken));

    let harness = TestHarness::start().await.expect("start harness");
    assert_eq!(harness.addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(harness.addr.port() > 0);
    harness.stop().await.expect("stop harness");
}

#[tokio::test]
async fn unauthenticated_and_invalid_token_requests_return_401_without_cors_headers() {
    let harness = TestHarness::start().await.expect("start harness");

    let unauth_cases = vec![
        build_request("GET", "/status", &[], None),
        build_request("GET", "/voice-channels", &[], None),
        build_request(
            "POST",
            "/play",
            &[("Content-Type", "application/json")],
            Some(r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"1"}"#),
        ),
        build_request("POST", "/stop", &[], None),
        build_request("POST", "/leave", &[], None),
        build_request("GET", "/does-not-exist", &[], None),
        build_request(
            "OPTIONS",
            "/status",
            &[("Origin", "https://evil.example")],
            None,
        ),
        build_request("GET", "/status", &[("Authorization", "Bearer wrong")], None),
        build_request(
            "GET",
            "/status",
            &[("Authorization", &format!("Basic {TEST_TOKEN}"))],
            None,
        ),
        build_request("GET", "/status", &[("Authorization", "Bearer ")], None),
        build_request(
            "GET",
            "/status",
            &[
                ("Authorization", &auth_header()),
                ("Authorization", &auth_header()),
            ],
            None,
        ),
    ];

    for raw_request in unauth_cases {
        let response = send_raw_http(harness.addr, &raw_request)
            .await
            .expect("send request");
        assert_eq!(response.status_code, 401);
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer realm=\"zebot\"")
        );
        assert_eq!(response.header("cache-control"), Some("no-store"));
        assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
        assert!(!response.has_any_cors_header());
        assert_eq!(
            response.json().expect("json")["error"]["code"],
            "unauthorized"
        );
    }

    harness.stop().await.expect("stop harness");
}

#[tokio::test]
async fn rejects_oversized_bodies_method_mismatches_wrong_content_type_and_unknown_routes() {
    let harness = TestHarness::start().await.expect("start harness");
    let auth = auth_header();

    let oversized_body = "x".repeat(MAX_REQUEST_BODY_BYTES + 1);
    let oversized_req = build_request(
        "POST",
        "/play",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
        ],
        Some(&oversized_body),
    );
    let oversized_res = send_raw_http(harness.addr, &oversized_req)
        .await
        .expect("oversized request");
    assert_eq!(oversized_res.status_code, 413);
    assert_eq!(
        oversized_res.json().expect("json")["error"]["code"],
        "payload_too_large"
    );

    let chunk_size = MAX_REQUEST_BODY_BYTES + 64;
    let chunk_payload = "a".repeat(chunk_size);
    let chunked_raw = format!(
        "POST /play HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{chunk_size:x}\r\n{chunk_payload}\r\n0\r\n\r\n"
    );
    let chunked_res = send_raw_http(harness.addr, chunked_raw.as_bytes())
        .await
        .expect("chunked oversized request");
    assert_eq!(chunked_res.status_code, 413);
    assert_eq!(
        chunked_res.json().expect("json")["error"]["code"],
        "payload_too_large"
    );

    let wrong_ct = send_raw_http(
        harness.addr,
        &build_request(
            "POST",
            "/play",
            &[("Authorization", &auth), ("Content-Type", "text/plain")],
            Some(r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"1"}"#),
        ),
    )
    .await
    .expect("wrong content type");
    assert_eq!(wrong_ct.status_code, 415);
    assert_eq!(
        wrong_ct.json().expect("json")["error"]["code"],
        "unsupported_media_type"
    );

    let wrong_method_status = send_raw_http(
        harness.addr,
        &build_request("POST", "/status", &[("Authorization", &auth)], None),
    )
    .await
    .expect("wrong method status");
    assert_eq!(wrong_method_status.status_code, 405);
    assert_eq!(wrong_method_status.header("allow"), Some("GET"));

    let wrong_method_play = send_raw_http(
        harness.addr,
        &build_request("GET", "/play", &[("Authorization", &auth)], None),
    )
    .await
    .expect("wrong method play");
    assert_eq!(wrong_method_play.status_code, 405);
    assert_eq!(wrong_method_play.header("allow"), Some("POST"));

    let unknown_route = send_raw_http(
        harness.addr,
        &build_request("GET", "/unknown", &[("Authorization", &auth)], None),
    )
    .await
    .expect("unknown route");
    assert_eq!(unknown_route.status_code, 404);
    assert_eq!(
        unknown_route.json().expect("json")["error"]["code"],
        "not_found"
    );

    harness.stop().await.expect("stop harness");
}

#[tokio::test]
async fn get_status_and_voice_channels_reflect_live_state_and_guild_availability() {
    let harness = TestHarness::start().await.expect("start harness");
    let auth = auth_header();

    let status_res = send_raw_http(
        harness.addr,
        &build_request("GET", "/status", &[("Authorization", &auth)], None),
    )
    .await
    .expect("get status");
    assert_eq!(status_res.status_code, 200);
    assert!(!status_res.has_any_cors_header());
    let status_json = status_res.json().expect("status json");
    assert_eq!(status_json["discord_state"], "connected");
    assert_eq!(status_json["playback_state"], "idle");
    assert!(status_json["voice_target"].is_null());
    assert!(status_json["active_track"].is_null());

    let channels_res = send_raw_http(
        harness.addr,
        &build_request("GET", "/voice-channels", &[("Authorization", &auth)], None),
    )
    .await
    .expect("get voice channels");
    assert_eq!(channels_res.status_code, 200);
    let channels_json = channels_res.json().expect("channels json");
    assert_eq!(channels_json["guild_id"], TEST_GUILD_ID.to_string());
    assert_eq!(channels_json["discord_state"], "connected");
    let channels = channels_json["channels"]
        .as_array()
        .expect("channels array");
    assert_eq!(channels.len(), 3);
    assert_eq!(channels[0]["id"], VOICE_CHANNEL_A.to_string());
    assert_eq!(channels[0]["name"], "Lounge");
    assert_eq!(channels[0]["kind"], "voice");
    assert_eq!(
        channels[0]["members"][0]["user_id"],
        USER_ALICE_ID.to_string()
    );
    assert_eq!(channels[0]["members"][0]["username"], "alice");
    assert_eq!(channels[0]["members"][0]["display_name"], "Alice");
    assert_eq!(channels[0]["members"][0]["is_bot"], false);
    assert_eq!(channels[1]["id"], VOICE_CHANNEL_B.to_string());
    assert_eq!(channels[1]["kind"], "stage");

    harness.directory.set_guild_available(false);
    let unavailable_res = send_raw_http(
        harness.addr,
        &build_request("GET", "/voice-channels", &[("Authorization", &auth)], None),
    )
    .await
    .expect("get unavailable voice channels");
    assert_eq!(unavailable_res.status_code, 503);
    assert_eq!(
        unavailable_res.json().expect("json")["error"]["code"],
        "guild_unavailable"
    );

    harness.stop().await.expect("stop harness");
}

#[tokio::test]
async fn post_play_rejects_invalid_urls_and_invalid_targets_without_mutating_app_core() {
    let harness = TestHarness::start().await.expect("start harness");
    let auth = auth_header();

    let user_offline_payload =
        format!(r#"{{"url":"https://youtu.be/dQw4w9WgXcQ","user_id":"{USER_OFFLINE_ID}"}}"#);
    let invalid_cases = [
        (
            r#"{"url":"https://evil.example/watch?v=dQw4w9WgXcQ","voice_channel_id":"222333444555666777"}"#,
            400,
            "invalid_url",
        ),
        (
            r#"{"url":"file:///C:/Windows/win.ini","voice_channel_id":"222333444555666777"}"#,
            400,
            "invalid_url",
        ),
        (
            r#"{"url":"--exec=calc","voice_channel_id":"222333444555666777"}"#,
            400,
            "invalid_url",
        ),
        (
            r#"{"url":"https://youtu.be/dQw4w9WgXcQ"}"#,
            400,
            "missing_target",
        ),
        (
            r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"222333444555666777","user_id":"333444555666777888"}"#,
            400,
            "ambiguous_target",
        ),
        (
            r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"0"}"#,
            400,
            "invalid_json",
        ),
        (
            r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"777777777777777777"}"#,
            404,
            "voice_channel_not_found",
        ),
        (
            r#"{"url":"https://youtu.be/dQw4w9WgXcQ","voice_channel_id":"222333444555666999"}"#,
            403,
            "missing_voice_permissions",
        ),
        (
            user_offline_payload.as_str(),
            409,
            "user_not_in_voice_channel",
        ),
    ];

    for (payload, expected_status, expected_code) in invalid_cases {
        let response = send_raw_http(
            harness.addr,
            &build_request(
                "POST",
                "/play",
                &[
                    ("Authorization", &auth),
                    ("Content-Type", "application/json"),
                ],
                Some(payload),
            ),
        )
        .await
        .expect("send invalid play request");
        assert_eq!(
            response.status_code, expected_status,
            "payload={payload}, body={}",
            response.body
        );
        assert_eq!(
            response.json().expect("json")["error"]["code"],
            expected_code,
            "payload={payload}, body={}",
            response.body
        );
    }

    assert!(harness.backend.actions().is_empty());
    assert_eq!(
        harness.app_core.status().playback_state,
        PlaybackState::Idle
    );
    assert!(harness.app_core.status().voice_target.is_none());

    harness.stop().await.expect("stop harness");
}

#[tokio::test]
async fn play_replace_stop_and_leave_flow_is_reversible_and_consistent_with_discord_commands() {
    let harness = TestHarness::start().await.expect("start harness");
    let auth = auth_header();
    let scope = CommandScope::new(TEST_GUILD_ID, TEST_ALLOWED_CHANNEL_ID).expect("valid scope");

    let play_by_user = send_raw_http(
        harness.addr,
        &build_request(
            "POST",
            "/play",
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
            ],
            Some(&format!(
                r#"{{"url":"https://youtu.be/dQw4w9WgXcQ","user_id":"{USER_ALICE_ID}"}}"#
            )),
        ),
    )
    .await
    .expect("play by user");
    assert_eq!(play_by_user.status_code, 200);
    let play_json = play_by_user.json().expect("play json");
    assert_eq!(play_json["playback_state"], "playing");
    assert_eq!(
        play_json["voice_target"]["channel_id"],
        VOICE_CHANNEL_A.to_string()
    );
    assert_eq!(play_json["active_track"]["video_id"], "dQw4w9WgXcQ");
    assert_eq!(play_json["active_track"]["title"], "Track dQw4w9WgXcQ");

    let discord_status_reply = handle_slash_command(
        scope,
        &harness.app_core,
        Some(TEST_GUILD_ID),
        TEST_ALLOWED_CHANNEL_ID,
        "status",
    );
    assert!(discord_status_reply.content.contains("Playing"));
    assert!(
        discord_status_reply
            .content
            .contains(&format!("<#{VOICE_CHANNEL_A}>"))
    );
    assert!(discord_status_reply.content.contains("Track dQw4w9WgXcQ"));

    let replace_by_channel = send_raw_http(
        harness.addr,
        &build_request(
            "POST",
            "/play",
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
            ],
            Some(&format!(
                r#"{{"url":"https://www.youtube.com/watch?v=9bZkp7q19f0","voice_channel_id":{VOICE_CHANNEL_B}}}"#
            )),
        ),
    )
    .await
    .expect("replace by channel");
    assert_eq!(replace_by_channel.status_code, 200);
    let replace_json = replace_by_channel.json().expect("replace json");
    assert_eq!(replace_json["playback_state"], "playing");
    assert_eq!(
        replace_json["voice_target"]["channel_id"],
        VOICE_CHANNEL_B.to_string()
    );
    assert_eq!(replace_json["active_track"]["video_id"], "9bZkp7q19f0");

    let stop_res = send_raw_http(
        harness.addr,
        &build_request("POST", "/stop", &[("Authorization", &auth)], None),
    )
    .await
    .expect("post stop");
    assert_eq!(stop_res.status_code, 200);
    let stop_json = stop_res.json().expect("stop json");
    assert_eq!(stop_json["playback_state"], "idle");
    assert_eq!(
        stop_json["voice_target"]["channel_id"],
        VOICE_CHANNEL_B.to_string()
    );
    assert!(stop_json["active_track"].is_null());

    let stop_again = send_raw_http(
        harness.addr,
        &build_request(
            "POST",
            "/stop",
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
            ],
            Some("{}"),
        ),
    )
    .await
    .expect("idempotent stop");
    assert_eq!(stop_again.status_code, 200);
    assert_eq!(stop_again.json().expect("json")["playback_state"], "idle");

    let discord_play = execute_play_command(
        scope,
        &harness.app_core,
        Some(TEST_GUILD_ID),
        TEST_ALLOWED_CHANNEL_ID,
        "https://youtu.be/kJQP7kiw5Fk",
        PlayCommandContext {
            invoking_voice_channel_id: Some(VOICE_CHANNEL_A),
            bot_permissions: None,
        },
    )
    .await;
    assert!(discord_play.content.contains("Now playing"));

    let status_after_discord_play = send_raw_http(
        harness.addr,
        &build_request("GET", "/status", &[("Authorization", &auth)], None),
    )
    .await
    .expect("status after discord play");
    assert_eq!(status_after_discord_play.status_code, 200);
    let after_play_json = status_after_discord_play.json().expect("json");
    assert_eq!(after_play_json["playback_state"], "playing");
    assert_eq!(
        after_play_json["voice_target"]["channel_id"],
        VOICE_CHANNEL_A.to_string()
    );
    assert_eq!(after_play_json["active_track"]["video_id"], "kJQP7kiw5Fk");

    let discord_stop = execute_stop_command(
        scope,
        &harness.app_core,
        Some(TEST_GUILD_ID),
        TEST_ALLOWED_CHANNEL_ID,
    )
    .await;
    assert!(discord_stop.content.contains("Stopped playback"));

    let status_after_discord_stop = send_raw_http(
        harness.addr,
        &build_request("GET", "/status", &[("Authorization", &auth)], None),
    )
    .await
    .expect("status after discord stop");
    assert_eq!(
        status_after_discord_stop.json().expect("json")["playback_state"],
        "idle"
    );

    let leave_res = send_raw_http(
        harness.addr,
        &build_request("POST", "/leave", &[("Authorization", &auth)], None),
    )
    .await
    .expect("post leave");
    assert_eq!(leave_res.status_code, 200);
    let leave_json = leave_res.json().expect("leave json");
    assert_eq!(leave_json["playback_state"], "idle");
    assert!(leave_json["voice_target"].is_null());

    let discord_leave = execute_leave_command(
        scope,
        &harness.app_core,
        Some(TEST_GUILD_ID),
        TEST_ALLOWED_CHANNEL_ID,
    )
    .await;
    assert!(discord_leave.content.contains("Not connected"));

    harness.stop().await.expect("stop harness");
}

#[tokio::test]
async fn post_play_reports_backend_failure_and_cancellation_with_clear_http_statuses() {
    let harness = TestHarness::start().await.expect("start harness");
    let auth = auth_header();

    harness
        .backend
        .fail_resolve("brokenVid01", "yt-dlp exited with status 1");
    let failed_res = send_raw_http(
        harness.addr,
        &build_request(
            "POST",
            "/play",
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
            ],
            Some(&format!(
                r#"{{"url":"https://youtu.be/brokenVid01","voice_channel_id":"{VOICE_CHANNEL_A}"}}"#
            )),
        ),
    )
    .await
    .expect("send failing play");
    assert_eq!(failed_res.status_code, 502);
    assert_eq!(
        failed_res.json().expect("json")["error"]["code"],
        "voice_backend_error"
    );
    assert_eq!(
        harness.app_core.status().playback_state,
        PlaybackState::Idle
    );

    let (started, _release) = harness.backend.install_resolve_gate("slowTrack01");
    let addr = harness.addr;
    let first_auth = auth.clone();
    let first_task = tokio::spawn(async move {
        send_raw_http(
            addr,
            &build_request(
                "POST",
                "/play",
                &[
                    ("Authorization", &first_auth),
                    ("Content-Type", "application/json"),
                ],
                Some(&format!(
                    r#"{{"url":"https://youtu.be/slowTrack01","voice_channel_id":"{VOICE_CHANNEL_A}"}}"#
                )),
            ),
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("first play should reach resolve stage");

    let superseding_stop = send_raw_http(
        harness.addr,
        &build_request("POST", "/stop", &[("Authorization", &auth)], None),
    )
    .await
    .expect("superseding stop");
    assert_eq!(superseding_stop.status_code, 200);

    let cancelled_res = first_task
        .await
        .expect("first task join")
        .expect("first task http");
    assert_eq!(cancelled_res.status_code, 409);
    assert_eq!(
        cancelled_res.json().expect("json")["error"]["code"],
        "cancelled"
    );

    harness.stop().await.expect("stop harness");
}
