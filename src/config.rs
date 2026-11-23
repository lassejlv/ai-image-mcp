//! The TOML config file and the directories this server reads and writes.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use indexmap::IndexMap;
use serde::Deserialize;

use crate::auth::Issuer;
use crate::image::ImageSettings;
use crate::providers::ProviderKind;

/// Overrides the directory holding the config file and saved logins.
pub const HOME_ENV: &str = "AI_IMAGE_MCP_HOME";
/// Postgres URL; overrides `database_url` in the config file.
pub const DATABASE_URL_ENV: &str = "AI_IMAGE_MCP_DATABASE_URL";

/// Where configuration and credentials live.
#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
}

impl Paths {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    /// `$AI_IMAGE_MCP_HOME`, else `<platform config dir>/ai-image-mcp`.
    pub fn from_env() -> anyhow::Result<Self> {
        if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
            return Ok(Self::new(PathBuf::from(home)));
        }
        let config =
            dirs::config_dir().context("no config directory found; set AI_IMAGE_MCP_HOME")?;
        Ok(Self::new(config.join("ai-image-mcp")))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn token_file(&self, issuer: Issuer) -> PathBuf {
        self.home
            .join("credentials")
            .join(format!("{}.json", issuer.id()))
    }
}

/// The parsed `config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Provider used when a request names none. Defaults to the first connected provider.
    pub default_provider: Option<String>,
    /// Postgres URL. When set, saved logins and generated images live in the database
    /// instead of local files.
    pub database_url: Option<String>,
    #[serde(default)]
    pub output: OutputConfig,
    /// Named providers, in priority order. When empty, every built-in provider is offered.
    #[serde(default)]
    pub providers: IndexMap<String, ProviderConfig>,
}

impl Config {
    /// Reads the config file; a missing file yields the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// `$AI_IMAGE_MCP_DATABASE_URL`, else `database_url` from the file.
    pub fn database_url(&self) -> Option<String> {
        std::env::var(DATABASE_URL_ENV)
            .ok()
            .or_else(|| self.database_url.clone())
            .filter(|url| !url.trim().is_empty())
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Some(default) = &self.default_provider
            && !self.providers.is_empty()
            && !self.providers.contains_key(default)
        {
            bail!("default_provider \"{default}\" is not defined under [providers]");
        }
        for (name, provider) in &self.providers {
            provider
                .validate()
                .with_context(|| format!("in [providers.{name}]"))?;
        }
        Ok(())
    }

    /// The configured providers, or one of each built-in kind when none are configured.
    pub fn provider_configs(&self) -> IndexMap<String, ProviderConfig> {
        if !self.providers.is_empty() {
            return self.providers.clone();
        }
        ProviderKind::ALL
            .into_iter()
            .map(|kind| (kind.id().to_owned(), ProviderConfig::builtin(kind)))
            .collect()
    }
}

/// Where generated images go and how they are returned.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// Directory for generated images. Defaults to `~/Pictures/ai-image-mcp`.
    pub dir: Option<PathBuf>,
    /// Return image data inline in tool results, in addition to saving files.
    #[serde(default = "default_true")]
    pub inline_images: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            dir: None,
            inline_images: true,
        }
    }
}

impl OutputConfig {
    pub fn dir(&self) -> PathBuf {
        if let Some(dir) = &self.dir {
            return expand_home(dir);
        }
        dirs::picture_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ai-image-mcp")
    }
}

fn default_true() -> bool {
    true
}

/// How a provider authenticates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// Use whichever supported credential is available.
    #[default]
    Auto,
    /// Only a login made with `ai-image-mcp login` (xai, codex).
    Oauth,
    /// Only an API key.
    ApiKey,
    /// Only the Codex CLI's own login in `$CODEX_HOME/auth.json` (codex).
    CodexCli,
}

/// One `[providers.<name>]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    /// Model used when a request names none.
    pub model: Option<String>,
    /// API root, for proxies or OpenAI-compatible services.
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: AuthMode,
    /// API key in the file. Prefer `api_key_env`.
    pub api_key: Option<String>,
    /// Environment variable holding the API key.
    pub api_key_env: Option<String>,
    /// Codex CLI home holding `auth.json`. Defaults to `$CODEX_HOME` or `~/.codex`.
    pub codex_home: Option<PathBuf>,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Settings applied to every request unless the request overrides them.
    #[serde(default)]
    pub defaults: ImageSettings,
}

impl ProviderConfig {
    pub fn builtin(kind: ProviderKind) -> Self {
        Self {
            kind,
            model: None,
            base_url: None,
            auth: AuthMode::Auto,
            api_key: None,
            api_key_env: None,
            codex_home: None,
            timeout_secs: None,
            defaults: ImageSettings::default(),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        let allowed: &[AuthMode] = match self.kind {
            ProviderKind::Xai => &[AuthMode::Auto, AuthMode::Oauth, AuthMode::ApiKey],
            ProviderKind::Codex => &[AuthMode::Auto, AuthMode::Oauth, AuthMode::CodexCli],
            ProviderKind::OpenAi | ProviderKind::Gemini | ProviderKind::Fal => {
                &[AuthMode::Auto, AuthMode::ApiKey]
            }
        };
        if !allowed.contains(&self.auth) {
            bail!(
                "auth = {:?} is not supported by type = \"{}\"",
                self.auth,
                self.kind.id()
            );
        }
        if self.kind == ProviderKind::Codex
            && (self.api_key.is_some() || self.api_key_env.is_some())
        {
            bail!(
                "type = \"codex\" uses a ChatGPT login, not an API key; use type = \"openai\" for API keys"
            );
        }
        Ok(())
    }
}

/// Expands a leading `~` to the home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Quality;

    #[test]
    fn empty_config_offers_every_builtin_provider() {
        let names: Vec<String> = Config::default().provider_configs().into_keys().collect();
        assert_eq!(names, ["xai", "codex", "openai", "gemini", "fal"]);
    }

    #[test]
    fn parses_named_providers_in_file_order() {
        let config: Config = toml::from_str(
            r#"
            default_provider = "flux"

            [output]
            dir = "~/art"
            inline_images = false

            [providers.flux]
            type = "fal"
            model = "fal-ai/flux/dev"
            defaults = { seed = 42 }

            [providers.together]
            type = "openai"
            base_url = "https://api.together.xyz/v1"
            api_key_env = "TOGETHER_API_KEY"

            [providers.grok]
            type = "xai"
            auth = "oauth"
            defaults = { quality = "medium", aspect_ratio = "16:9" }
            "#,
        )
        .unwrap();
        config.validate().unwrap();

        let names: Vec<&String> = config.providers.keys().collect();
        assert_eq!(names, ["flux", "together", "grok"]);
        assert!(!config.output.inline_images);
        assert_eq!(
            config.providers["grok"].defaults.quality,
            Some(Quality::Medium)
        );
        assert_eq!(config.providers["flux"].defaults.seed, Some(42));
    }

    #[test]
    fn rejects_typos_and_unsupported_auth() {
        assert!(toml::from_str::<Config>("[providers.a]\ntype = \"xai\"\nmodle = \"x\"").is_err());
        let config: Config =
            toml::from_str("[providers.a]\ntype = \"gemini\"\nauth = \"oauth\"").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_provider_must_exist() {
        let config: Config =
            toml::from_str("default_provider = \"nope\"\n[providers.a]\ntype = \"fal\"").unwrap();
        assert!(config.validate().is_err());
    }
}
