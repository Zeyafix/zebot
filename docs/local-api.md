# Local HTTP/JSON API

Zebot exposes a local HTTP/JSON API inside the running bot process backed directly by `AppCore` (the exact same single source of truth used by Discord slash commands). Both the host and AI agents inspect and control Zebot through the same endpoints and receive identical state.

## Security Defaults

- **Loopback only:** The server binds strictly to `127.0.0.1:<API_PORT>` (default port `8080`). It never binds to `0.0.0.0` or external network interfaces.
- **Mandatory Bearer authentication:** Every request to every path must include `Authorization: Bearer <API_TOKEN>` (validated with constant-time comparison). Missing, duplicate, malformed, or invalid tokens return `401 Unauthorized` with `WWW-Authenticate: Bearer realm="zebot"`.
- **No permissive CORS:** The API never sends `Access-Control-Allow-*` headers and sets `Cache-Control: no-store` and `X-Content-Type-Options: nosniff` on all responses.
- **Strict body size limit:** Request bodies larger than **16 KiB (`16384` bytes)** are rejected with `413 Payload Too Large` (for both `Content-Length` and chunked transfer encoding).
- **Strict input validation:** `POST /play` requires `Content-Type: application/json`, rejects unknown JSON fields, and validates URLs through the exact same `YouTubeUrl` allowlist as Discord `/play`.
- **Lossless Discord IDs:** All Discord snowflake IDs in JSON responses are serialized as strings (`"123456789012345678"`). Request payloads accept Discord IDs as either strings (recommended) or positive integers.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/status` | Return the current `BotStatus` (Discord connection state, active voice channel, playback state, active track). |
| `GET` | `/voice-channels` | List voice and stage channels in `DISCORD_GUILD_ID` with their current members. |
| `POST` | `/play` | Validate a YouTube URL, resolve the target voice channel, join/move if needed, replace any active track, and start playback. |
| `POST` | `/stop` | Stop active playback or cancel an in-flight track load while staying connected to the voice channel. |
| `POST` | `/leave` | Stop playback and disconnect from the current voice channel. |

### `GET /status`

Returns `200 OK`:

```json
{
  "discord_state": "connected",
  "voice_target": {
    "guild_id": "111222333444555666",
    "channel_id": "222333444555666777"
  },
  "playback_state": "playing",
  "active_track": {
    "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    "video_id": "dQw4w9WgXcQ",
    "title": "Sample Song",
    "channel": "Sample Artist",
    "duration_seconds": 213
  }
}
```

Field values:

- `discord_state`: `"disconnected"`, `"connecting"`, `"connected"`, or `"shutting_down"`
- `playback_state`: `"idle"`, `"loading"`, or `"playing"`
- `voice_target`: `null` when disconnected from voice, or `{ "guild_id", "channel_id" }`
- `active_track`: `null` when idle, or track metadata with canonical YouTube `url` and `video_id`

### `GET /voice-channels`

Returns `200 OK` once the Discord guild cache is ready (or `503 Service Unavailable` with `"code": "guild_unavailable"` while Discord is still connecting):

```json
{
  "guild_id": "111222333444555666",
  "discord_state": "connected",
  "channels": [
    {
      "id": "222333444555666777",
      "name": "General",
      "kind": "voice",
      "members": [
        {
          "user_id": "333444555666777888",
          "username": "alice",
          "display_name": "Alice",
          "is_bot": false
        }
      ]
    }
  ]
}
```

### `POST /play`

Requires `Content-Type: application/json` and a JSON object containing `url` and **exactly one** of `voice_channel_id` or `user_id`:

Target a specific voice channel:

```json
{
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "voice_channel_id": "222333444555666777"
}
```

Or target whichever voice channel a Discord member is currently in:

```json
{
  "url": "https://youtu.be/dQw4w9WgXcQ",
  "user_id": "333444555666777888"
}
```

Returns `200 OK` with the updated `BotStatus` JSON once playback starts.

### `POST /stop`

Accepts an empty body or `{}` (no `Content-Type` header is needed when the body is empty). Idempotent when already idle. Returns `200 OK` with the updated `BotStatus` JSON.

### `POST /leave`

Accepts an empty body or `{}` (no `Content-Type` header is needed when the body is empty). Idempotent when already disconnected from voice. Returns `200 OK` with the updated `BotStatus` JSON.

### Error Format

All error responses use the same JSON envelope:

```json
{
  "error": {
    "code": "invalid_url",
    "message": "host 'example.com' is not in the YouTube allowlist"
  }
}
```

Common status codes:

| Status | Error code(s) | Meaning |
| --- | --- | --- |
| `400` | `invalid_json`, `invalid_request`, `missing_target`, `ambiguous_target`, `invalid_url`, `invalid_body` | Malformed request, missing/both targets, or disallowed YouTube URL. |
| `401` | `unauthorized` | Missing or invalid `Authorization: Bearer <API_TOKEN>` header. |
| `403` | `missing_voice_permissions` | Zebot lacks `Connect` or `Speak` permission in the target voice channel. |
| `404` | `not_found`, `voice_channel_not_found` | Unknown route or target voice channel does not exist in `DISCORD_GUILD_ID`. |
| `405` | `method_not_allowed` | Wrong HTTP method for an existing endpoint (includes `Allow` header). |
| `409` | `user_not_in_voice_channel`, `cancelled` | Target user is not currently in a voice channel, or playback was superseded by a newer command. |
| `413` | `payload_too_large` | Request body exceeded 16 KiB. |
| `415` | `unsupported_media_type` | Missing or non-`application/json` `Content-Type` on a JSON request. |
| `502` | `voice_backend_error` | `yt-dlp`, `ffmpeg`, or Discord voice backend failed. |
| `503` | `guild_unavailable` | Discord guild cache is not ready yet; wait until `GET /status` reports `"connected"`. |

## Examples for Hosts and AI Agents

### PowerShell (Windows)

```powershell
$ApiBase = "http://127.0.0.1:8080"
$Headers = @{ Authorization = "Bearer $env:API_TOKEN" }

# 1. Check bot readiness and current playback state
Invoke-RestMethod -Method Get -Uri "$ApiBase/status" -Headers $Headers

# 2. Discover voice channels and active members
Invoke-RestMethod -Method Get -Uri "$ApiBase/voice-channels" -Headers $Headers

# 3. Play a YouTube URL into a specific voice channel
$Body = @{
    url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    voice_channel_id = "222333444555666777"
} | ConvertTo-Json
Invoke-RestMethod -Method Post -Uri "$ApiBase/play" -Headers $Headers -ContentType "application/json" -Body $Body

# 4. Stop playback (stays in voice channel)
Invoke-RestMethod -Method Post -Uri "$ApiBase/stop" -Headers $Headers

# 5. Leave the voice channel
Invoke-RestMethod -Method Post -Uri "$ApiBase/leave" -Headers $Headers
```

### `curl` (macOS / Linux / Git Bash / Windows `curl.exe`)

```bash
export ZEBOT_URL="http://127.0.0.1:8080"
export ZEBOT_TOKEN="replace_with_your_api_token"

# 1. Check bot status
curl -sS -H "Authorization: Bearer $ZEBOT_TOKEN" \
  "$ZEBOT_URL/status"

# 2. List voice channels and members
curl -sS -H "Authorization: Bearer $ZEBOT_TOKEN" \
  "$ZEBOT_URL/voice-channels"

# 3. Play a track in the channel where a specific user is connected
curl -sS -X POST \
  -H "Authorization: Bearer $ZEBOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://youtu.be/dQw4w9WgXcQ","user_id":"333444555666777888"}' \
  "$ZEBOT_URL/play"

# 4. Stop playback (stays in voice channel)
curl -sS -X POST -H "Authorization: Bearer $ZEBOT_TOKEN" \
  "$ZEBOT_URL/stop"

# 5. Stop playback and leave voice channel
curl -sS -X POST -H "Authorization: Bearer $ZEBOT_TOKEN" \
  "$ZEBOT_URL/leave"
```

### Typical AI Agent Workflow

1. Ensure Zebot is running (for example `.\launch.bat status` / `.\launch.bat start` on Windows) and verify `GET /status` returns `"discord_state": "connected"`.
2. Call `GET /voice-channels` to find the target `voice_channel_id` (or the `user_id` of the member who asked for music).
3. Call `POST /play` with the YouTube URL and target.
4. Reverse or adjust state at any time using `POST /play` (replace track / move channel), `POST /stop` (silence audio while staying connected), or `POST /leave` (disconnect cleanly).
