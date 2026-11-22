//! Credentials: API keys from the environment and OAuth logins to subscriptions.

pub mod chatgpt;
pub mod xai;

mod tokens;

use std::path::PathBuf;

use anyhow::{Context, bail};
use tokio::sync::Mutex;

pub use tokens::OAuthTokens;
use tokens::TokenFile;

/// An account system this server can log in to with the device-code flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issuer {
    /// xAI accounts, entitled by a SuperGrok or X Premium+ subscription.
    Xai,
    /// ChatGPT accounts, entitled by a ChatGPT plan that includes Codex.
    ChatGpt,
}

impl Issuer {
    pub fn id(self) -> &'static str {
        match self {
            Self::Xai => "xai",
            Self::ChatGpt => "chatgpt",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Xai => "xAI (SuperGrok / X Premium+)",
            Self::ChatGpt => "ChatGPT (Codex)",
        }
    }

    async fn refresh(
        self,
        http: &reqwest::Client,
        tokens: &OAuthTokens,
    ) -> anyhow::Result<OAuthTokens> {
        match self {
            Self::Xai => xai::refresh(http, tokens).await,
            Self::ChatGpt => chatgpt::refresh(http, tokens).await,
        }
    }
}

/// The verification step a person completes in a browser during device-code login.
#[derive(Debug, Clone)]
pub struct DevicePrompt {
    pub verification_uri: String,
    pub user_code: String,
}

/// Runs the device-code login for `issuer` and saves the grant to `path`.
///
/// `on_prompt` receives the URL and code to show the person while the flow polls.
pub async fn login(
    issuer: Issuer,
    http: &reqwest::Client,
    path: PathBuf,
    on_prompt: impl FnOnce(&DevicePrompt),
) -> anyhow::Result<OAuthTokens> {
    let tokens = match issuer {
        Issuer::Xai => xai::device_login(http, on_prompt).await?,
        Issuer::ChatGpt => chatgpt::device_login(http, on_prompt).await?,
    };
    TokenFile::Own(path).save(&tokens)?;
    Ok(tokens)
}

/// Deletes a saved login. Returns false when there was none.
pub fn logout(path: &std::path::Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// An OAuth grant on disk, refreshed on demand and written back after each refresh.
#[derive(Debug)]
pub struct OAuthGrant {
    issuer: Issuer,
    file: TokenFile,
    /// Refresh tokens rotate on use, so refreshes must never run concurrently.
    refresh_lock: Mutex<()>,
}

impl OAuthGrant {
    /// A grant created by `ai-image-mcp login`.
    pub fn own(issuer: Issuer, path: PathBuf) -> Self {
        Self::new(issuer, TokenFile::Own(path))
    }

    /// The grant the Codex CLI keeps in `$CODEX_HOME/auth.json`, shared with it.
    pub fn codex_cli(path: PathBuf) -> Self {
        Self::new(Issuer::ChatGpt, TokenFile::CodexCli(path))
    }

    fn new(issuer: Issuer, file: TokenFile) -> Self {
        Self {
            issuer,
            file,
            refresh_lock: Mutex::new(()),
        }
    }

    pub fn is_present(&self) -> bool {
        self.file.path().is_file()
    }

    fn describe(&self) -> String {
        match &self.file {
            TokenFile::Own(_) => format!("{} login", self.issuer.display_name()),
            TokenFile::CodexCli(path) => format!("Codex CLI login ({})", path.display()),
        }
    }

    async fn tokens(
        &self,
        http: &reqwest::Client,
        force_refresh: bool,
    ) -> anyhow::Result<OAuthTokens> {
        let _guard = self.refresh_lock.lock().await;
        let tokens = self.file.load()?;
        if !force_refresh && !tokens.expires_soon() {
            return Ok(tokens);
        }
        let refreshed =
            self.issuer.refresh(http, &tokens).await.with_context(|| {
                format!("refreshing the {} failed; log in again", self.describe())
            })?;
        self.file.save(&refreshed)?;
        Ok(refreshed)
    }
}

/// An API key read from the config file or an environment variable.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub literal: Option<String>,
    pub env: String,
}

impl ApiKey {
    fn value(&self) -> Option<String> {
        self.literal
            .clone()
            .or_else(|| std::env::var(&self.env).ok())
            .filter(|key| !key.trim().is_empty())
    }
}

/// One way a provider can authenticate.
#[derive(Debug)]
pub enum Credential {
    ApiKey(ApiKey),
    OAuth(OAuthGrant),
}

impl Credential {
    fn is_available(&self) -> bool {
        match self {
            Self::ApiKey(key) => key.value().is_some(),
            Self::OAuth(grant) => grant.is_present(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::ApiKey(key) if key.literal.is_some() => "API key from config".to_owned(),
            Self::ApiKey(key) => format!("API key from ${}", key.env),
            Self::OAuth(grant) => grant.describe(),
        }
    }
}

/// The token a request is sent with.
#[derive(Debug, Clone)]
pub struct Bearer {
    pub token: String,
    /// ChatGPT workspace the token belongs to, sent by the Codex backend.
    pub account_id: Option<String>,
    /// Whether a forced refresh can produce a new token after a 401.
    pub refreshable: bool,
}

/// Whether a provider can authenticate right now, and how.
#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub ready: bool,
    pub detail: String,
}

/// The credentials a provider accepts, tried in order; the first available one is used.
#[derive(Debug)]
pub struct Auth {
    credentials: Vec<Credential>,
    setup_hint: String,
}

impl Auth {
    pub fn new(credentials: Vec<Credential>, setup_hint: impl Into<String>) -> Self {
        Self {
            credentials,
            setup_hint: setup_hint.into(),
        }
    }

    fn active(&self) -> Option<&Credential> {
        self.credentials.iter().find(|c| c.is_available())
    }

    pub fn status(&self) -> AuthStatus {
        match self.active() {
            Some(credential) => AuthStatus {
                ready: true,
                detail: credential.describe(),
            },
            None => AuthStatus {
                ready: false,
                detail: format!("not connected: {}", self.setup_hint),
            },
        }
    }

    /// Resolves the bearer token to send, refreshing an OAuth grant when it is about to expire.
    pub async fn bearer(
        &self,
        http: &reqwest::Client,
        force_refresh: bool,
    ) -> anyhow::Result<Bearer> {
        let Some(credential) = self.active() else {
            bail!("not connected: {}", self.setup_hint);
        };
        match credential {
            Credential::ApiKey(key) => Ok(Bearer {
                token: key.value().context("API key disappeared")?,
                account_id: None,
                refreshable: false,
            }),
            Credential::OAuth(grant) => {
                let tokens = grant.tokens(http, force_refresh).await?;
                Ok(Bearer {
                    account_id: tokens.account_id(),
                    token: tokens.access_token,
                    refreshable: true,
                })
            }
        }
    }
}
