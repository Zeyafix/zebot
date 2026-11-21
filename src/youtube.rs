use std::error::Error;
use std::fmt;
use std::str::FromStr;

pub const MAX_YOUTUBE_URL_LENGTH: usize = 2048;
pub const MAX_VIDEO_ID_LENGTH: usize = 64;

pub const ALLOWED_YOUTUBE_HOSTS: [&str; 5] = [
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "youtu.be",
    "music.youtube.com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum YouTubeHost {
    YoutubeCom,
    WwwYoutubeCom,
    MYoutubeCom,
    YoutuBe,
    MusicYoutubeCom,
}

impl YouTubeHost {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::YoutubeCom => "youtube.com",
            Self::WwwYoutubeCom => "www.youtube.com",
            Self::MYoutubeCom => "m.youtube.com",
            Self::YoutuBe => "youtu.be",
            Self::MusicYoutubeCom => "music.youtube.com",
        }
    }

    fn parse_allowed(host: &str) -> Option<Self> {
        match host {
            "youtube.com" => Some(Self::YoutubeCom),
            "www.youtube.com" => Some(Self::WwwYoutubeCom),
            "m.youtube.com" => Some(Self::MYoutubeCom),
            "youtu.be" => Some(Self::YoutuBe),
            "music.youtube.com" => Some(Self::MusicYoutubeCom),
            _ => None,
        }
    }
}

impl fmt::Display for YouTubeHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct YouTubeUrl {
    host: YouTubeHost,
    video_id: String,
    canonical: String,
}

impl YouTubeUrl {
    pub fn parse(raw: &str) -> Result<Self, YouTubeUrlError> {
        if raw.trim().is_empty() {
            return Err(YouTubeUrlError::Empty);
        }

        if raw.len() > MAX_YOUTUBE_URL_LENGTH {
            return Err(YouTubeUrlError::TooLong {
                max_length: MAX_YOUTUBE_URL_LENGTH,
                actual_length: raw.len(),
            });
        }

        if raw.starts_with('-') || raw.trim_start().starts_with('-') {
            return Err(YouTubeUrlError::ArgumentInjection);
        }

        if looks_like_local_path(raw) {
            return Err(YouTubeUrlError::LocalPath);
        }

        if raw.bytes().any(is_disallowed_url_byte) {
            return Err(YouTubeUrlError::InvalidCharacters);
        }

        let rest = parse_https_scheme(raw)?;
        let (authority, suffix) = split_authority(rest);

        if authority.is_empty() {
            return Err(YouTubeUrlError::HostNotAllowed {
                host: String::new(),
            });
        }

        if authority.contains('@') {
            return Err(YouTubeUrlError::UserInfoNotAllowed);
        }

        if authority.contains(':') || authority.contains('[') || authority.contains(']') {
            return Err(YouTubeUrlError::PortNotAllowed);
        }

        let host_lower = authority.to_ascii_lowercase();
        let Some(host) = YouTubeHost::parse_allowed(&host_lower) else {
            return Err(YouTubeUrlError::HostNotAllowed { host: host_lower });
        };

        let without_fragment = suffix.split_once('#').map_or(suffix, |(before, _)| before);
        let (path, query) = match without_fragment.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (without_fragment, None),
        };

        validate_path_safety(path)?;

        let video_id = match host {
            YouTubeHost::YoutuBe => extract_short_link_video_id(path)?,
            YouTubeHost::YoutubeCom
            | YouTubeHost::WwwYoutubeCom
            | YouTubeHost::MYoutubeCom
            | YouTubeHost::MusicYoutubeCom => extract_standard_video_id(path, query)?,
        };

        validate_video_id(video_id)?;

        let canonical = format!("https://www.youtube.com/watch?v={video_id}");
        Ok(Self {
            host,
            video_id: video_id.to_string(),
            canonical,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    pub fn host(&self) -> YouTubeHost {
        self.host
    }

    pub fn video_id(&self) -> &str {
        &self.video_id
    }
}

impl fmt::Display for YouTubeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl FromStr for YouTubeUrl {
    type Err = YouTubeUrlError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for YouTubeUrl {
    type Error = YouTubeUrlError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouTubeUrlError {
    Empty,
    TooLong {
        max_length: usize,
        actual_length: usize,
    },
    ArgumentInjection,
    LocalPath,
    InvalidCharacters,
    MissingScheme,
    UnsupportedScheme {
        scheme: String,
    },
    UserInfoNotAllowed,
    PortNotAllowed,
    HostNotAllowed {
        host: String,
    },
    InvalidPath,
    MissingVideoId,
    MultipleVideoIds,
    InvalidVideoId,
}

impl fmt::Display for YouTubeUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "YouTube URL must not be empty"),
            Self::TooLong {
                max_length,
                actual_length,
            } => write!(
                f,
                "YouTube URL exceeds maximum length of {max_length} bytes (got {actual_length})"
            ),
            Self::ArgumentInjection => {
                write!(f, "URL must not start with '-' (argument injection)")
            }
            Self::LocalPath => write!(f, "local file paths are not allowed"),
            Self::InvalidCharacters => {
                write!(f, "URL contains whitespace, control, or unsafe characters")
            }
            Self::MissingScheme => write!(f, "URL must start with https://"),
            Self::UnsupportedScheme { scheme } => {
                write!(
                    f,
                    "unsupported URL scheme '{scheme}'; only https:// is allowed"
                )
            }
            Self::UserInfoNotAllowed => write!(f, "URL must not contain userinfo ('@')"),
            Self::PortNotAllowed => {
                write!(f, "URL must not specify an explicit port or IP literal")
            }
            Self::HostNotAllowed { host } => {
                write!(f, "host '{host}' is not in the YouTube allowlist")
            }
            Self::InvalidPath => {
                write!(
                    f,
                    "URL path is not a supported YouTube video path (/watch, /shorts, /live, /embed, or youtu.be/<id>)"
                )
            }
            Self::MissingVideoId => write!(f, "YouTube URL is missing a video identifier"),
            Self::MultipleVideoIds => {
                write!(f, "YouTube URL must not contain multiple 'v' parameters")
            }
            Self::InvalidVideoId => {
                write!(f, "YouTube video identifier contains invalid characters")
            }
        }
    }
}

impl Error for YouTubeUrlError {}

fn looks_like_local_path(raw: &str) -> bool {
    if raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with(".\\")
        || raw.starts_with("..\\")
        || raw.starts_with("~/")
    {
        return true;
    }

    let bytes = raw.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }

    false
}

fn is_disallowed_url_byte(byte: u8) -> bool {
    if !(0x21..=0x7E).contains(&byte) {
        return true;
    }

    matches!(
        byte,
        b'\\' | b'<' | b'>' | b'"' | b'`' | b'{' | b'}' | b'|' | b'^'
    )
}

fn parse_https_scheme(raw: &str) -> Result<&str, YouTubeUrlError> {
    if let Some((scheme, rest)) = raw.split_once("://") {
        let lower_scheme = scheme.to_ascii_lowercase();
        if lower_scheme == "https" {
            return Ok(rest);
        }
        return Err(YouTubeUrlError::UnsupportedScheme {
            scheme: lower_scheme,
        });
    }

    if let Some((candidate_scheme, _)) = raw.split_once(':')
        && is_valid_scheme_syntax(candidate_scheme)
    {
        return Err(YouTubeUrlError::UnsupportedScheme {
            scheme: candidate_scheme.to_ascii_lowercase(),
        });
    }

    Err(YouTubeUrlError::MissingScheme)
}

fn is_valid_scheme_syntax(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn split_authority(rest: &str) -> (&str, &str) {
    match rest.find(['/', '?', '#']) {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    }
}

fn validate_path_safety(path: &str) -> Result<(), YouTubeUrlError> {
    if path.is_empty() || path == "/" || !path.starts_with('/') {
        return Err(YouTubeUrlError::InvalidPath);
    }

    if path.contains("//") {
        return Err(YouTubeUrlError::InvalidPath);
    }

    let lower = path.to_ascii_lowercase();
    if lower.contains("%00")
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c")
    {
        return Err(YouTubeUrlError::InvalidPath);
    }

    for segment in path.trim_start_matches('/').split('/') {
        if segment == "." || segment == ".." {
            return Err(YouTubeUrlError::InvalidPath);
        }
    }

    Ok(())
}

fn extract_short_link_video_id(path: &str) -> Result<&str, YouTubeUrlError> {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() || trimmed.contains('/') {
        return Err(YouTubeUrlError::InvalidPath);
    }
    Ok(trimmed)
}

fn extract_standard_video_id<'a>(
    path: &'a str,
    query: Option<&'a str>,
) -> Result<&'a str, YouTubeUrlError> {
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if trimmed == "watch" {
        return extract_watch_query_video_id(query);
    }

    let Some((prefix, remainder)) = trimmed.split_once('/') else {
        return Err(YouTubeUrlError::InvalidPath);
    };

    if !matches!(prefix, "shorts" | "live" | "embed") {
        return Err(YouTubeUrlError::InvalidPath);
    }

    if remainder.is_empty() {
        return Err(YouTubeUrlError::MissingVideoId);
    }

    if remainder.contains('/') {
        return Err(YouTubeUrlError::InvalidPath);
    }

    Ok(remainder)
}

fn extract_watch_query_video_id(query: Option<&str>) -> Result<&str, YouTubeUrlError> {
    let Some(query_str) = query else {
        return Err(YouTubeUrlError::MissingVideoId);
    };

    let mut found: Option<&str> = None;
    for pair in query_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        if key == "v" {
            if found.is_some() {
                return Err(YouTubeUrlError::MultipleVideoIds);
            }
            found = Some(value);
        }
    }

    match found {
        Some("") | None => Err(YouTubeUrlError::MissingVideoId),
        Some(video_id) => Ok(video_id),
    }
}

fn validate_video_id(video_id: &str) -> Result<(), YouTubeUrlError> {
    if video_id.is_empty() {
        return Err(YouTubeUrlError::MissingVideoId);
    }

    if video_id.len() > MAX_VIDEO_ID_LENGTH {
        return Err(YouTubeUrlError::InvalidVideoId);
    }

    if !video_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(YouTubeUrlError::InvalidVideoId);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_YOUTUBE_URL_LENGTH, YouTubeHost, YouTubeUrl, YouTubeUrlError};

    #[test]
    fn accepts_all_allowlisted_youtube_hosts_and_canonicalizes() {
        let cases = [
            (
                "https://youtube.com/watch?v=dQw4w9WgXcQ",
                YouTubeHost::YoutubeCom,
                "dQw4w9WgXcQ",
            ),
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s&si=abc",
                YouTubeHost::WwwYoutubeCom,
                "dQw4w9WgXcQ",
            ),
            (
                "https://m.youtube.com/watch?v=abc_123-XYZ",
                YouTubeHost::MYoutubeCom,
                "abc_123-XYZ",
            ),
            (
                "https://music.youtube.com/watch?v=-tKVN2mAKRI&list=RD123",
                YouTubeHost::MusicYoutubeCom,
                "-tKVN2mAKRI",
            ),
            (
                "https://youtu.be/dQw4w9WgXcQ?t=15#section",
                YouTubeHost::YoutuBe,
                "dQw4w9WgXcQ",
            ),
            (
                "HTTPS://WWW.YOUTUBE.COM/shorts/ShortId_01",
                YouTubeHost::WwwYoutubeCom,
                "ShortId_01",
            ),
            (
                "https://www.youtube.com/live/LiveId-02/",
                YouTubeHost::WwwYoutubeCom,
                "LiveId-02",
            ),
            (
                "https://www.youtube.com/embed/EmbedId03",
                YouTubeHost::WwwYoutubeCom,
                "EmbedId03",
            ),
        ];

        for (raw, expected_host, expected_id) in cases {
            let parsed = YouTubeUrl::parse(raw).unwrap();
            assert_eq!(parsed.host(), expected_host);
            assert_eq!(parsed.video_id(), expected_id);
            assert_eq!(
                parsed.as_str(),
                format!("https://www.youtube.com/watch?v={expected_id}")
            );
        }
    }

    #[test]
    fn rejects_local_file_paths() {
        let local_paths = [
            "/etc/passwd",
            "C:\\Windows\\System32\\cmd.exe",
            "C:/Users/host/.env",
            "\\\\localhost\\c$\\secrets.txt",
            "./.env",
            "../secret.mp3",
            ".\\audio.wav",
            "..\\audio.wav",
            "~/secrets.txt",
        ];

        for input in local_paths {
            assert_eq!(
                YouTubeUrl::parse(input).unwrap_err(),
                YouTubeUrlError::LocalPath,
                "input was: {input}"
            );
        }
    }

    #[test]
    fn rejects_non_https_schemes() {
        let cases = [
            ("http://www.youtube.com/watch?v=dQw4w9WgXcQ", "http"),
            ("file:///etc/passwd", "file"),
            ("file:///C:/Windows/win.ini", "file"),
            ("ftp://www.youtube.com/watch?v=dQw4w9WgXcQ", "ftp"),
            ("javascript:alert(1)", "javascript"),
            ("data:text/plain;base64,SGVsbG8=", "data"),
            ("ytsearch:never", "ytsearch"),
        ];

        for (input, expected_scheme) in cases {
            assert_eq!(
                YouTubeUrl::parse(input).unwrap_err(),
                YouTubeUrlError::UnsupportedScheme {
                    scheme: expected_scheme.to_string(),
                },
                "input was: {input}"
            );
        }
    }

    #[test]
    fn rejects_argument_injection_and_control_characters() {
        let argument_injections = ["--exec", "-o", "   --config-location=.env", "-f"];

        for input in argument_injections {
            assert_eq!(
                YouTubeUrl::parse(input).unwrap_err(),
                YouTubeUrlError::ArgumentInjection,
                "input was: {input}"
            );
        }

        let unsafe_chars = [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ --exec calc",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ\n--exec=calc",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ\r\n",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ\0",
            "https://www.youtube.com\\@evil.com/watch?v=dQw4w9WgXcQ",
            " https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        ];

        for input in unsafe_chars {
            assert_eq!(
                YouTubeUrl::parse(input).unwrap_err(),
                YouTubeUrlError::InvalidCharacters,
                "input was: {input}"
            );
        }
    }

    #[test]
    fn rejects_ssrf_userinfo_ports_and_unlisted_hosts() {
        assert_eq!(
            YouTubeUrl::parse("https://www.youtube.com@127.0.0.1/watch?v=dQw4w9WgXcQ").unwrap_err(),
            YouTubeUrlError::UserInfoNotAllowed
        );
        assert_eq!(
            YouTubeUrl::parse("https://user:pass@www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap_err(),
            YouTubeUrlError::UserInfoNotAllowed
        );

        assert_eq!(
            YouTubeUrl::parse("https://www.youtube.com:443/watch?v=dQw4w9WgXcQ").unwrap_err(),
            YouTubeUrlError::PortNotAllowed
        );
        assert_eq!(
            YouTubeUrl::parse("https://[::1]/watch?v=dQw4w9WgXcQ").unwrap_err(),
            YouTubeUrlError::PortNotAllowed
        );

        let disallowed_hosts = [
            ("https://127.0.0.1/watch?v=dQw4w9WgXcQ", "127.0.0.1"),
            ("https://localhost/watch?v=dQw4w9WgXcQ", "localhost"),
            (
                "https://169.254.169.254/latest/meta-data/",
                "169.254.169.254",
            ),
            (
                "https://www.youtube.com.evil.com/watch?v=dQw4w9WgXcQ",
                "www.youtube.com.evil.com",
            ),
            (
                "https://evil-youtube.com/watch?v=dQw4w9WgXcQ",
                "evil-youtube.com",
            ),
            (
                "https://sub.youtube.com/watch?v=dQw4w9WgXcQ",
                "sub.youtube.com",
            ),
            ("https://youtube.com./watch?v=dQw4w9WgXcQ", "youtube.com."),
        ];

        for (input, expected_host) in disallowed_hosts {
            assert_eq!(
                YouTubeUrl::parse(input).unwrap_err(),
                YouTubeUrlError::HostNotAllowed {
                    host: expected_host.to_string(),
                },
                "input was: {input}"
            );
        }
    }

    #[test]
    fn rejects_malformed_paths_traversal_and_invalid_video_ids() {
        assert_eq!(YouTubeUrl::parse("").unwrap_err(), YouTubeUrlError::Empty);
        assert_eq!(
            YouTubeUrl::parse("   \t ").unwrap_err(),
            YouTubeUrlError::Empty
        );
        assert_eq!(
            YouTubeUrl::parse("www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap_err(),
            YouTubeUrlError::MissingScheme
        );

        let oversized = format!(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&pad={}",
            "a".repeat(MAX_YOUTUBE_URL_LENGTH)
        );
        assert!(matches!(
            YouTubeUrl::parse(&oversized).unwrap_err(),
            YouTubeUrlError::TooLong { .. }
        ));

        let bad_paths = [
            "https://www.youtube.com",
            "https://www.youtube.com/",
            "https://youtu.be/",
            "https://youtu.be/abc/extra",
            "https://www.youtube.com/../watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/%2e%2e/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com//watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/channel/UC12345",
            "https://www.youtube.com/shorts/",
            "https://www.youtube.com/shorts/abc/extra",
        ];

        for input in bad_paths {
            assert_eq!(
                YouTubeUrl::parse(input).unwrap_err(),
                YouTubeUrlError::InvalidPath,
                "input was: {input}"
            );
        }

        assert_eq!(
            YouTubeUrl::parse("https://www.youtube.com/watch").unwrap_err(),
            YouTubeUrlError::MissingVideoId
        );
        assert_eq!(
            YouTubeUrl::parse("https://www.youtube.com/watch?v=").unwrap_err(),
            YouTubeUrlError::MissingVideoId
        );
        assert_eq!(
            YouTubeUrl::parse("https://www.youtube.com/watch?v=aaa&v=bbb").unwrap_err(),
            YouTubeUrlError::MultipleVideoIds
        );
        assert_eq!(
            YouTubeUrl::parse("https://www.youtube.com/watch?v=bad%20id").unwrap_err(),
            YouTubeUrlError::InvalidVideoId
        );
        assert_eq!(
            YouTubeUrl::parse("https://youtu.be/bad.id").unwrap_err(),
            YouTubeUrlError::InvalidVideoId
        );
    }
}
