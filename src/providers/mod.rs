//! Image providers and the registry that routes requests to them.

mod codex;
mod fal;
mod gemini;
mod images_api;
mod openai;
mod xai;

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::auth::{ApiKey, Auth, AuthMethod, Bearer, Credential, Issuer, OAuthGrant};
use crate::config::{AuthMode, Config, Paths, ProviderConfig, expand_home};
use crate::http;
use crate::image::{GeneratedImage, ImageRequest, ImageSettings, InputImage, Setting};

/// Default request timeout; high-quality generations can take minutes.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// The most images any provider accepts per request.
pub const MAX_IMAGES: u8 = 10;

/// The API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// xAI Grok Imagine, through a SuperGrok / X Premium+ login or an xAI API key.
    Xai,
    /// OpenAI GPT Image through a ChatGPT plan, via the Codex backend.
    Codex,
    /// The OpenAI Images API, or any service compatible with it.
    #[serde(rename = "openai")]
    OpenAi,
    /// Google Gemini native image generation (Nano Banana).
    Gemini,
    /// fal.ai's model catalog (Flux, Recraft, Ideogram, and more).
    Fal,
}

impl ProviderKind {
    pub const ALL: [Self; 5] = [
        Self::Xai,
        Self::Codex,
        Self::OpenAi,
        Self::Gemini,
        Self::Fal,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Xai => "xai",
            Self::Codex => "codex",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Fal => "fal",
        }
    }

    pub fn default_model(self) -> &'static str {
        self.known_models()[0]
    }

    /// Models to suggest; any model id the service accepts can be requested.
    pub fn known_models(self) -> &'static [&'static str] {
        match self {
            Self::Xai => &[
                "grok-imagine-image-2.0",
                "grok-imagine-image",
                "grok-imagine-image-quality",
            ],
            Self::Codex => &["gpt-image-2"],
            Self::OpenAi => &["gpt-image-2", "gpt-image-1.5", "gpt-image-1-mini"],
            Self::Gemini => &[
                "gemini-nano-banana-2.1",
                "gemini-3-pro-image",
                "gemini-3.1-flash-image",
                "gemini-3.1-flash-lite-image",
            ],
            Self::Fal => &[
                "fal-ai/flux/dev",
                "fal-ai/flux-2",
                "fal-ai/flux-2/edit",
                "fal-ai/nano-banana-pro",
                "fal-ai/recraft/v3/text-to-image",
            ],
        }
    }

    /// Settings this provider maps onto its API.
    pub fn settings(self) -> &'static [Setting] {
        use Setting::*;
        match self {
            Self::Xai => &[N, AspectRatio, Resolution, Quality],
            Self::Codex => &[N, Size, Quality, Background],
            Self::OpenAi => &[
                N,
                Size,
                Quality,
                Background,
                OutputFormat,
                OutputCompression,
            ],
            Self::Gemini => &[N, AspectRatio, Resolution],
            Self::Fal => &[
                N,
                Size,
                AspectRatio,
                Resolution,
                Seed,
                NegativePrompt,
                OutputFormat,
            ],
        }
    }

    /// How many input images an edit accepts.
    pub fn max_input_images(self) -> usize {
        match self {
            Self::Xai | Self::Codex => 5,
            Self::OpenAi => 16,
            Self::Gemini => 14,
            Self::Fal => 10,
        }
    }

    /// The `ai-image-mcp login` account that connects this provider type.
    pub fn login_account(self) -> Option<&'static str> {
        match self {
            Self::Xai => Some("xai"),
            Self::Codex => Some("codex"),
            Self::OpenAi | Self::Gemini | Self::Fal => None,
        }
    }

    fn api_key_env(self) -> &'static str {
        match self {
            Self::Xai => "XAI_API_KEY",
            Self::OpenAi | Self::Codex => "OPENAI_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::Fal => "FAL_KEY",
        }
    }
}

/// A configured provider instance.
#[derive(Debug)]
pub enum Provider {
    Xai(xai::Xai),
    Codex(codex::Codex),
    OpenAi(openai::OpenAi),
    Gemini(gemini::Gemini),
    Fal(fal::Fal),
}

impl Provider {
    async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        match self {
            Self::Xai(p) => p.generate(request).await,
            Self::Codex(p) => p.generate(request).await,
            Self::OpenAi(p) => p.generate(request).await,
            Self::Gemini(p) => p.generate(request).await,
            Self::Fal(p) => p.generate(request).await,
        }
    }
}

/// Connection details every provider implementation shares.
#[derive(Debug)]
struct Client {
    name: String,
    base_url: String,
    http: reqwest::Client,
    auth: Auth,
}

impl Client {
    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// Runs `call` with the current credential, retrying once with a refreshed OAuth token on 401.
    async fn authorized<T, F, Fut>(&self, call: F) -> anyhow::Result<T>
    where
        F: Fn(Bearer) -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let bearer = self.auth.bearer(&self.http, false).await?;
        let refreshable = bearer.refreshable;
        match call(bearer).await {
            Err(e) if refreshable && http::is_unauthorized(&e) => {
                call(self.auth.bearer(&self.http, true).await?).await
            }
            result => result,
        }
    }
}

/// A provider plus the defaults it was configured with.
#[derive(Debug)]
pub struct Entry {
    name: String,
    kind: ProviderKind,
    model: String,
    defaults: ImageSettings,
    provider: Provider,
}

/// What a finished generation produced.
#[derive(Debug)]
pub struct Generation {
    pub provider: String,
    pub model: String,
    pub images: Vec<GeneratedImage>,
    /// Requested settings this provider does not support and therefore did not apply.
    pub ignored: Vec<Setting>,
}

/// A provider's capabilities and connection state, as shown to clients.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    pub ready: bool,
    pub auth: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<AuthMethod>,
    /// The account `ai-image-mcp login <account>` connects for this provider, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<&'static str>,
    pub default: bool,
    pub default_model: String,
    pub suggested_models: Vec<&'static str>,
    pub settings: Vec<Setting>,
    pub max_input_images: usize,
    #[serde(skip_serializing_if = "ImageSettings::is_empty")]
    pub defaults: ImageSettings,
}

impl ImageSettings {
    fn is_empty(&self) -> bool {
        self == &ImageSettings::default()
    }
}

impl Entry {
    pub fn name(&self) -> &str {
        &self.name
    }

    fn client(&self) -> &Client {
        match &self.provider {
            Provider::Xai(p) => &p.client,
            Provider::Codex(p) => &p.client,
            Provider::OpenAi(p) => &p.client,
            Provider::Gemini(p) => &p.client,
            Provider::Fal(p) => &p.client,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.client().auth.status().ready()
    }

    pub fn info(&self, is_default: bool) -> ProviderInfo {
        let status = self.client().auth.status();
        ProviderInfo {
            name: self.name.clone(),
            kind: self.kind,
            ready: status.ready(),
            auth: status.detail,
            auth_method: status.method,
            login: self.kind.login_account(),
            default: is_default,
            default_model: self.model.clone(),
            suggested_models: self.kind.known_models().to_vec(),
            settings: self.kind.settings().to_vec(),
            max_input_images: self.kind.max_input_images(),
            defaults: self.defaults.clone(),
        }
    }

    /// Validates and sends one request, applying this provider's configured defaults.
    pub async fn generate(
        &self,
        prompt: String,
        model: Option<String>,
        settings: ImageSettings,
        input_images: Vec<InputImage>,
    ) -> anyhow::Result<Generation> {
        ensure!(!prompt.trim().is_empty(), "prompt must not be empty");
        let settings = settings.with_defaults(&self.defaults);
        ensure!(
            settings.count() <= MAX_IMAGES,
            "n must be between 1 and {MAX_IMAGES}, got {}",
            settings.count()
        );
        ensure!(
            input_images.len() <= self.kind.max_input_images(),
            "{} accepts at most {} input images, got {}",
            self.name,
            self.kind.max_input_images(),
            input_images.len()
        );
        let ignored = settings
            .present()
            .into_iter()
            .filter(|s| !self.kind.settings().contains(s))
            .collect();
        let request = ImageRequest {
            prompt,
            model: model
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| self.model.clone()),
            settings,
            input_images,
        };
        let images = self
            .provider
            .generate(&request)
            .await
            .with_context(|| format!("{} ({}) failed", self.name, request.model))?;
        ensure!(!images.is_empty(), "{} returned no images", self.name);
        Ok(Generation {
            provider: self.name.clone(),
            model: request.model,
            images,
            ignored,
        })
    }
}

/// All configured providers, in priority order.
#[derive(Debug)]
pub struct Registry {
    entries: IndexMap<String, Entry>,
    default: Option<String>,
}

impl Registry {
    pub fn from_config(config: &Config, paths: &Paths) -> anyhow::Result<Self> {
        let entries = config
            .provider_configs()
            .into_iter()
            .map(|(name, cfg)| {
                let entry = build_entry(&name, cfg, paths)
                    .with_context(|| format!("setting up provider \"{name}\""))?;
                Ok((name, entry))
            })
            .collect::<anyhow::Result<IndexMap<_, _>>>()?;
        Ok(Self {
            entries,
            default: config.default_provider.clone(),
        })
    }

    /// The configured default, else the first provider that is connected.
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref().or_else(|| {
            self.entries
                .values()
                .find(|e| e.is_ready())
                .map(Entry::name)
        })
    }

    /// Looks up a provider by name, or the default when `name` is `None`.
    pub fn get(&self, name: Option<&str>) -> anyhow::Result<&Entry> {
        let Some(name) = name
            .filter(|n| !n.trim().is_empty())
            .or_else(|| self.default_name())
        else {
            bail!(
                "no provider is connected; run `ai-image-mcp login xai`, `ai-image-mcp login codex`, or set an API key"
            );
        };
        self.entries.get(name).with_context(|| {
            let known: Vec<&str> = self.entries.keys().map(String::as_str).collect();
            format!(
                "unknown provider \"{name}\"; configured providers: {}",
                known.join(", ")
            )
        })
    }

    pub fn infos(&self) -> Vec<ProviderInfo> {
        let default = self.default_name();
        self.entries
            .values()
            .map(|entry| entry.info(Some(entry.name()) == default))
            .collect()
    }
}

fn build_entry(name: &str, cfg: ProviderConfig, paths: &Paths) -> anyhow::Result<Entry> {
    let kind = cfg.kind;
    let env = cfg
        .api_key_env
        .clone()
        .unwrap_or_else(|| kind.api_key_env().to_owned());
    let api_key = || {
        Credential::ApiKey(ApiKey {
            literal: cfg.api_key.clone(),
            env: env.clone(),
        })
    };
    let (credentials, hint) = match kind {
        ProviderKind::Xai => {
            let oauth =
                || Credential::OAuth(OAuthGrant::own(Issuer::Xai, paths.token_file(Issuer::Xai)));
            match cfg.auth {
                AuthMode::Oauth => (vec![oauth()], "run `ai-image-mcp login xai`".to_owned()),
                AuthMode::ApiKey => (vec![api_key()], format!("set {env}")),
                _ => (
                    vec![oauth(), api_key()],
                    format!("run `ai-image-mcp login xai` or set {env}"),
                ),
            }
        }
        ProviderKind::Codex => {
            let own = || {
                Credential::OAuth(OAuthGrant::own(
                    Issuer::ChatGpt,
                    paths.token_file(Issuer::ChatGpt),
                ))
            };
            let codex_cli = || {
                Credential::OAuth(OAuthGrant::codex_cli(
                    codex_home(cfg.codex_home.as_deref()).join("auth.json"),
                ))
            };
            let credentials = match cfg.auth {
                AuthMode::Oauth => vec![own()],
                AuthMode::CodexCli => vec![codex_cli()],
                _ => vec![own(), codex_cli()],
            };
            (
                credentials,
                "run `ai-image-mcp login codex` (or `codex login`)".to_owned(),
            )
        }
        ProviderKind::OpenAi | ProviderKind::Gemini | ProviderKind::Fal => {
            (vec![api_key()], format!("set {env}"))
        }
    };

    let http = reqwest::Client::builder()
        .user_agent(concat!("ai-image-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(
            cfg.timeout_secs
                .map_or(DEFAULT_TIMEOUT, Duration::from_secs),
        )
        .build()?;
    let client = Client {
        name: name.to_owned(),
        base_url: cfg
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url(kind).to_owned()),
        http,
        auth: Auth::new(credentials, hint),
    };
    let provider = match kind {
        ProviderKind::Xai => Provider::Xai(xai::Xai { client }),
        ProviderKind::Codex => Provider::Codex(codex::Codex { client }),
        ProviderKind::OpenAi => Provider::OpenAi(openai::OpenAi { client }),
        ProviderKind::Gemini => Provider::Gemini(gemini::Gemini { client }),
        ProviderKind::Fal => Provider::Fal(fal::Fal { client }),
    };
    Ok(Entry {
        name: name.to_owned(),
        kind,
        model: cfg.model.unwrap_or_else(|| kind.default_model().to_owned()),
        defaults: cfg.defaults,
        provider,
    })
}

fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Xai => "https://api.x.ai/v1",
        ProviderKind::Codex => "https://chatgpt.com/backend-api/codex",
        ProviderKind::OpenAi => "https://api.openai.com/v1",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        ProviderKind::Fal => "https://fal.run",
    }
}

fn codex_home(configured: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(path) = configured {
        return expand_home(path);
    }
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| ".codex".into())
}

/// Merges `patch` into `target`: objects merge key by key, anything else replaces.
fn merge_json(target: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (key, value) in patch {
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                merge_json(existing, incoming)
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Inserts `value` under `key` when it is set.
fn put<T: Serialize>(body: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        body.insert(
            key.to_owned(),
            serde_json::to_value(value).expect("settings serialize to JSON"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_json_merges_nested_objects() {
        let mut target = json!({"a": 1, "cfg": {"x": 1, "y": 2}})
            .as_object()
            .cloned()
            .unwrap();
        let patch = json!({"a": 2, "cfg": {"y": 3}, "b": true})
            .as_object()
            .cloned()
            .unwrap();
        merge_json(&mut target, &patch);
        assert_eq!(
            Value::Object(target),
            json!({"a": 2, "cfg": {"x": 1, "y": 3}, "b": true})
        );
    }

    #[test]
    fn every_kind_suggests_its_default_model() {
        for kind in ProviderKind::ALL {
            assert!(!kind.default_model().is_empty(), "{kind:?}");
            assert!(kind.max_input_images() > 0, "{kind:?}");
        }
    }
}
