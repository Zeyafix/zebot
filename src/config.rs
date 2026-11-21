use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const DEFAULT_API_PORT: u16 = 8080;
pub const DEFAULT_YT_DLP_PATH: &str = "yt-dlp";
pub const DEFAULT_FFMPEG_PATH: &str = "ffmpeg";

const ENV_DISCORD_TOKEN: &str = "DISCORD_TOKEN";
const ENV_DISCORD_GUILD_ID: &str = "DISCORD_GUILD_ID";
const ENV_ALLOWED_CHANNEL_ID: &str = "ALLOWED_CHANNEL_ID";
const ENV_API_TOKEN: &str = "API_TOKEN";
const ENV_API_PORT: &str = "API_PORT";
const ENV_YT_DLP_PATH: &str = "YT_DLP_PATH";
const ENV_FFMPEG_PATH: &str = "FFMPEG_PATH";

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub discord_token: String,
    pub discord_guild_id: u64,
    pub allowed_channel_id: u64,
    pub api_token: String,
    pub api_port: u16,
    pub yt_dlp_path: String,
    pub ffmpeg_path: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("discord_token", &"[REDACTED]")
            .field("discord_guild_id", &self.discord_guild_id)
            .field("allowed_channel_id", &self.allowed_channel_id)
            .field("api_token", &"[REDACTED]")
            .field("api_port", &self.api_port)
            .field("yt_dlp_path", &self.yt_dlp_path)
            .field("ffmpeg_path", &self.ffmpeg_path)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigIssue {
    Missing(&'static str),
    Empty(&'static str),
    InvalidSnowflake { key: &'static str, value: String },
    InvalidPort { key: &'static str, value: String },
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(key) => write!(f, "{key} is required but not set"),
            Self::Empty(key) => write!(f, "{key} must not be empty"),
            Self::InvalidSnowflake { key, value } => {
                write!(
                    f,
                    "{key} must be a positive Discord ID integer, got '{value}'"
                )
            }
            Self::InvalidPort { key, value } => {
                write!(
                    f,
                    "{key} must be an integer between 1 and 65535, got '{value}'"
                )
            }
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    EnvFileRead {
        path: PathBuf,
        source: io::Error,
    },
    EnvFileSyntax {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    Validation(Vec<ConfigIssue>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvFileRead { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::EnvFileSyntax { path, line, reason } => {
                write!(
                    f,
                    "invalid syntax in {} at line {line}: {reason}",
                    path.display()
                )
            }
            Self::Validation(issues) => {
                writeln!(f, "configuration validation failed:")?;
                for issue in issues {
                    writeln!(f, "  - {issue}")?;
                }
                write!(
                    f,
                    "copy .env.example to .env and fill in the required values (see docs/quick-start.md)"
                )
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EnvFileRead { source, .. } => Some(source),
            Self::EnvFileSyntax { .. } | Self::Validation(_) => None,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from_dotenv_path(Path::new(".env"))
    }

    pub fn load_from_dotenv_path(path: &Path) -> Result<Self, ConfigError> {
        let dotenv_values = load_dotenv_file(path)?;
        Self::from_lookup(|key| match env::var(key) {
            Ok(value) => Some(value),
            Err(_) => dotenv_values.get(key).cloned(),
        })
    }

    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut issues = Vec::new();

        let discord_token = read_required_string(&lookup, ENV_DISCORD_TOKEN, &mut issues);
        let discord_guild_id = read_required_snowflake(&lookup, ENV_DISCORD_GUILD_ID, &mut issues);
        let allowed_channel_id =
            read_required_snowflake(&lookup, ENV_ALLOWED_CHANNEL_ID, &mut issues);
        let api_token = read_required_string(&lookup, ENV_API_TOKEN, &mut issues);
        let api_port = read_optional_port(&lookup, ENV_API_PORT, DEFAULT_API_PORT, &mut issues);
        let yt_dlp_path =
            read_optional_string(&lookup, ENV_YT_DLP_PATH, DEFAULT_YT_DLP_PATH, &mut issues);
        let ffmpeg_path =
            read_optional_string(&lookup, ENV_FFMPEG_PATH, DEFAULT_FFMPEG_PATH, &mut issues);

        if !issues.is_empty() {
            return Err(ConfigError::Validation(issues));
        }

        match (
            discord_token,
            discord_guild_id,
            allowed_channel_id,
            api_token,
        ) {
            (
                Some(discord_token),
                Some(discord_guild_id),
                Some(allowed_channel_id),
                Some(api_token),
            ) => Ok(Self {
                discord_token,
                discord_guild_id,
                allowed_channel_id,
                api_token,
                api_port,
                yt_dlp_path,
                ffmpeg_path,
            }),
            _ => Err(ConfigError::Validation(issues)),
        }
    }
}

fn load_dotenv_file(path: &Path) -> Result<HashMap<String, String>, ConfigError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => {
            return Err(ConfigError::EnvFileRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    parse_dotenv(&content).map_err(|(line, reason)| ConfigError::EnvFileSyntax {
        path: path.to_path_buf(),
        line,
        reason,
    })
}

pub fn parse_dotenv(content: &str) -> Result<HashMap<String, String>, (usize, String)> {
    let mut values = HashMap::new();

    for (zero_based_index, raw_line) in content.lines().enumerate() {
        let line_number = zero_based_index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let assignment = trimmed
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(trimmed);

        let Some((raw_key, raw_value)) = assignment.split_once('=') else {
            return Err((line_number, "expected KEY=VALUE assignment".to_string()));
        };

        let key = raw_key.trim();
        if key.is_empty() {
            return Err((line_number, "variable name before '=' is empty".to_string()));
        }

        let value = parse_dotenv_value(raw_value.trim()).map_err(|reason| (line_number, reason))?;
        values.insert(key.to_string(), value);
    }

    Ok(values)
}

fn parse_dotenv_value(raw: &str) -> Result<String, String> {
    if let Some(rest) = raw.strip_prefix('"') {
        let Some(inner) = rest.strip_suffix('"') else {
            return Err("unclosed double-quoted value".to_string());
        };
        return Ok(unescape_double_quoted(inner));
    }

    if let Some(rest) = raw.strip_prefix('\'') {
        let Some(inner) = rest.strip_suffix('\'') else {
            return Err("unclosed single-quoted value".to_string());
        };
        return Ok(inner.to_string());
    }

    Ok(raw.to_string())
}

fn unescape_double_quoted(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some('r') => output.push('\r'),
                Some('t') => output.push('\t'),
                Some('"') => output.push('"'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(ch);
        }
    }

    output
}

fn read_required_string<F>(
    lookup: &F,
    key: &'static str,
    issues: &mut Vec<ConfigIssue>,
) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        None => {
            issues.push(ConfigIssue::Missing(key));
            None
        }
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                issues.push(ConfigIssue::Empty(key));
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    }
}

fn read_required_snowflake<F>(
    lookup: &F,
    key: &'static str,
    issues: &mut Vec<ConfigIssue>,
) -> Option<u64>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        None => {
            issues.push(ConfigIssue::Missing(key));
            None
        }
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                issues.push(ConfigIssue::Empty(key));
                return None;
            }
            match trimmed.parse::<u64>() {
                Ok(parsed) if parsed > 0 => Some(parsed),
                _ => {
                    issues.push(ConfigIssue::InvalidSnowflake {
                        key,
                        value: trimmed.to_string(),
                    });
                    None
                }
            }
        }
    }
}

fn read_optional_port<F>(
    lookup: &F,
    key: &'static str,
    default_value: u16,
    issues: &mut Vec<ConfigIssue>,
) -> u16
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        None => default_value,
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                issues.push(ConfigIssue::Empty(key));
                return default_value;
            }
            match trimmed.parse::<u16>() {
                Ok(port) if port > 0 => port,
                _ => {
                    issues.push(ConfigIssue::InvalidPort {
                        key,
                        value: trimmed.to_string(),
                    });
                    default_value
                }
            }
        }
    }
}

fn read_optional_string<F>(
    lookup: &F,
    key: &'static str,
    default_value: &str,
    issues: &mut Vec<ConfigIssue>,
) -> String
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        None => default_value.to_string(),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                issues.push(ConfigIssue::Empty(key));
                default_value.to_string()
            } else {
                trimmed.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        Config, ConfigError, ConfigIssue, DEFAULT_API_PORT, DEFAULT_FFMPEG_PATH,
        DEFAULT_YT_DLP_PATH, parse_dotenv,
    };

    fn base_env() -> HashMap<String, String> {
        HashMap::from([
            ("DISCORD_TOKEN".to_string(), "discord-secret".to_string()),
            (
                "DISCORD_GUILD_ID".to_string(),
                "111222333444555666".to_string(),
            ),
            (
                "ALLOWED_CHANNEL_ID".to_string(),
                "999888777666555444".to_string(),
            ),
            ("API_TOKEN".to_string(), "api-secret".to_string()),
        ])
    }

    #[test]
    fn loads_valid_config_with_defaults() {
        let env_map = base_env();
        let config = Config::from_lookup(|key| env_map.get(key).cloned()).unwrap();

        assert_eq!(config.discord_token, "discord-secret");
        assert_eq!(config.discord_guild_id, 111_222_333_444_555_666);
        assert_eq!(config.allowed_channel_id, 999_888_777_666_555_444);
        assert_eq!(config.api_token, "api-secret");
        assert_eq!(config.api_port, DEFAULT_API_PORT);
        assert_eq!(config.yt_dlp_path, DEFAULT_YT_DLP_PATH);
        assert_eq!(config.ffmpeg_path, DEFAULT_FFMPEG_PATH);
    }

    #[test]
    fn applies_optional_overrides() {
        let mut env_map = base_env();
        env_map.insert("API_PORT".to_string(), "9090".to_string());
        env_map.insert(
            "YT_DLP_PATH".to_string(),
            "C:\\tools\\yt-dlp.exe".to_string(),
        );
        env_map.insert(
            "FFMPEG_PATH".to_string(),
            "C:\\tools\\ffmpeg.exe".to_string(),
        );

        let config = Config::from_lookup(|key| env_map.get(key).cloned()).unwrap();
        assert_eq!(config.api_port, 9090);
        assert_eq!(config.yt_dlp_path, "C:\\tools\\yt-dlp.exe");
        assert_eq!(config.ffmpeg_path, "C:\\tools\\ffmpeg.exe");
    }

    #[test]
    fn reports_all_missing_required_variables_together() {
        let error = Config::from_lookup(|_| None).unwrap_err();
        let ConfigError::Validation(issues) = error else {
            unreachable!("expected validation error");
        };

        assert_eq!(
            issues,
            vec![
                ConfigIssue::Missing("DISCORD_TOKEN"),
                ConfigIssue::Missing("DISCORD_GUILD_ID"),
                ConfigIssue::Missing("ALLOWED_CHANNEL_ID"),
                ConfigIssue::Missing("API_TOKEN"),
            ]
        );
    }

    #[test]
    fn rejects_invalid_ids_ports_and_blank_tokens() {
        let env_map = HashMap::from([
            ("DISCORD_TOKEN".to_string(), "   ".to_string()),
            ("DISCORD_GUILD_ID".to_string(), "0".to_string()),
            ("ALLOWED_CHANNEL_ID".to_string(), "not-a-number".to_string()),
            ("API_TOKEN".to_string(), "valid-token".to_string()),
            ("API_PORT".to_string(), "0".to_string()),
        ]);

        let error = Config::from_lookup(|key| env_map.get(key).cloned()).unwrap_err();
        let ConfigError::Validation(issues) = error else {
            unreachable!("expected validation error");
        };

        assert_eq!(
            issues,
            vec![
                ConfigIssue::Empty("DISCORD_TOKEN"),
                ConfigIssue::InvalidSnowflake {
                    key: "DISCORD_GUILD_ID",
                    value: "0".to_string(),
                },
                ConfigIssue::InvalidSnowflake {
                    key: "ALLOWED_CHANNEL_ID",
                    value: "not-a-number".to_string(),
                },
                ConfigIssue::InvalidPort {
                    key: "API_PORT",
                    value: "0".to_string(),
                },
            ]
        );
    }

    #[test]
    fn redacts_secrets_in_debug_output() {
        let env_map = base_env();
        let config = Config::from_lookup(|key| env_map.get(key).cloned()).unwrap();
        let rendered = format!("{config:?}");

        assert!(!rendered.contains("discord-secret"));
        assert!(!rendered.contains("api-secret"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn parses_dotenv_lines_and_quotes() {
        let raw = concat!(
            "# comment line\n",
            "\n",
            "DISCORD_TOKEN=\"tok=en\\\"123\"\n",
            "export API_TOKEN='single=secret'\n",
            "API_PORT = 8181\n",
        );

        let parsed = parse_dotenv(raw).unwrap();
        assert_eq!(
            parsed.get("DISCORD_TOKEN").map(String::as_str),
            Some("tok=en\"123")
        );
        assert_eq!(
            parsed.get("API_TOKEN").map(String::as_str),
            Some("single=secret")
        );
        assert_eq!(parsed.get("API_PORT").map(String::as_str), Some("8181"));
    }

    #[test]
    fn rejects_malformed_dotenv_lines() {
        assert_eq!(parse_dotenv("BROKEN_LINE").unwrap_err().0, 1);
        assert_eq!(parse_dotenv("=value").unwrap_err().0, 1);
        assert_eq!(parse_dotenv("KEY=\"unclosed").unwrap_err().0, 1);
    }
}
