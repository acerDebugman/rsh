use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const DEFAULT_MODEL: &str = "deepseek-flash";
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const CONFIG_PATH_ENV: &str = "RSH_CONFIG";

/// Runtime configuration, loaded from `~/.config/rsh/config.toml` and
/// overridden by `RSH_*` environment variables.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key_file: PathBuf,
    pub max_steps: usize,
    pub max_tokens: u32,
    pub temperature: f32,
    pub tool_output_limit: usize,
    pub send_reasoning: bool,
    pub permissions: Permissions,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key_file: PathBuf::from("~/.config/ds.key"),
            max_steps: 50,
            max_tokens: 16_384,
            temperature: 0.2,
            tool_output_limit: 30_000,
            send_reasoning: false,
            permissions: Permissions::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub mode: PermissionMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Ask,
    WorkspaceWrite,
    #[default]
    DangerFullAccess,
}

impl PermissionMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

impl Config {
    /// Load configuration from `path`, or the default config path when `None`.
    /// A missing file yields defaults; a malformed file is an error.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.map(Path::to_path_buf).or_else(default_config_path);
        let mut config = match path {
            Some(ref path) if path.exists() => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("read config {}", path.display()))?;
                toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?
            }
            _ => Self::default(),
        };
        config.apply_env(|key| std::env::var(key).ok());
        config.validate()?;
        Ok(config)
    }

    /// Apply environment overrides. Takes a getter so tests stay hermetic.
    pub fn apply_env<F>(&mut self, get: F)
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(value) = get("RSH_MODEL").filter(|v| !v.trim().is_empty()) {
            self.model = value;
        }
        if let Some(value) = get("RSH_BASE_URL").filter(|v| !v.trim().is_empty()) {
            self.base_url = value;
        }
        if let Some(value) = get("RSH_API_KEY_FILE").filter(|v| !v.trim().is_empty()) {
            self.api_key_file = PathBuf::from(value);
        }
        if let Some(value) = get("RSH_MAX_STEPS").and_then(|v| v.parse().ok()) {
            self.max_steps = value;
        }
        if let Some(mode) = get("RSH_PERMISSION_MODE").and_then(|v| PermissionMode::parse(&v)) {
            self.permissions.mode = mode;
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty() {
            bail!("model must not be empty");
        }
        if self.max_steps == 0 {
            bail!("max_steps must be greater than zero");
        }
        if !(0.0..=2.0).contains(&self.temperature) {
            bail!("temperature must be between 0.0 and 2.0");
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            bail!("base_url must start with http:// or https://");
        }
        Ok(())
    }

    /// Resolve the API key: `RSH_API_KEY`, then the key file, then
    /// `DEEPSEEK_API_KEY`.
    pub fn api_key(&self) -> Result<String> {
        if let Some(key) = non_empty(std::env::var("RSH_API_KEY").ok()) {
            return Ok(key);
        }
        let path = expand_tilde(&self.api_key_file);
        if let Some(key) = read_key_file(&path)? {
            return Ok(key);
        }
        if let Some(key) = non_empty(std::env::var("DEEPSEEK_API_KEY").ok()) {
            return Ok(key);
        }
        bail!(
            "no API key: set RSH_API_KEY, write {}, or export DEEPSEEK_API_KEY",
            path.display()
        )
    }
}

#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("rsh").join("config.toml"))
}

#[must_use]
pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// Read a key file, returning `None` when it does not exist and trimming
/// surrounding whitespace when it does.
pub fn read_key_file(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(non_empty(Some(text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read api key {}", path.display())),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The sandbox makes the machine-wide TMPDIR read-only, so tests keep
    /// their scratch space under `target/`.
    fn temp_dir() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("rsh-")
            .tempdir_in(&base)
            .unwrap()
    }

    #[test]
    fn defaults_match_the_agreed_contract() {
        let config = Config::default();
        assert_eq!(config.model, "deepseek-flash");
        assert_eq!(config.base_url, "https://api.deepseek.com");
        assert_eq!(config.max_steps, 50);
        assert_eq!(config.permissions.mode, PermissionMode::DangerFullAccess);
    }

    #[test]
    fn toml_overrides_defaults() {
        let dir = temp_dir();
        let path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "model = \"deepseek-v4-pro\"\nmax_steps = 7\n\n[permissions]\nmode = \"ask\"\n"
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.model, "deepseek-v4-pro");
        assert_eq!(config.max_steps, 7);
        assert_eq!(config.permissions.mode, PermissionMode::Ask);
    }

    #[test]
    fn env_overrides_apply_in_order() {
        let mut config = Config::default();
        config.apply_env(|key| match key {
            "RSH_MODEL" => Some("custom-model".to_string()),
            "RSH_BASE_URL" => Some("http://localhost:1234".to_string()),
            "RSH_MAX_STEPS" => Some("9".to_string()),
            "RSH_PERMISSION_MODE" => Some("workspace-write".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.base_url, "http://localhost:1234");
        assert_eq!(config.max_steps, 9);
        assert_eq!(config.permissions.mode, PermissionMode::WorkspaceWrite);
    }

    #[test]
    fn permission_mode_round_trips() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(PermissionMode::parse("nope"), None);
    }

    #[test]
    fn key_file_is_trimmed_and_missing_is_none() {
        let dir = temp_dir();
        let path = dir.path().join("ds.key");
        std::fs::write(&path, "  sk-test\n").unwrap();
        assert_eq!(read_key_file(&path).unwrap(), Some("sk-test".to_string()));
        assert_eq!(
            read_key_file(&dir.path().join("missing.key")).unwrap(),
            None
        );
    }

    #[test]
    fn tilde_expands_against_home() {
        let expanded = expand_tilde(Path::new("~/.config/ds.key"));
        assert!(!expanded.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn invalid_temperature_is_rejected() {
        let mut config = Config::default();
        config.temperature = 3.0;
        assert!(config.validate().is_err());
    }
}
