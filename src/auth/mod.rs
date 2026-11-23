//! Credentials: API keys from the environment and OAuth logins to subscriptions.

pub mod chatgpt;
pub mod xai;

mod store;
mod tokens;

use anyhow::{Context, bail};
use serde::Serialize;
use tokio::sync::Mutex;

pub use store::TokenStore;
pub use tokens::{OAuthTokens, TokenFile};

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
#[derive(Debug, Clone, Serialize)]
pub struct DevicePrompt {
    pub verification_uri: String,
    pub user_code: String,
}

/// Runs the device-code login for `issuer` and returns the grant for the caller to store.
///
/// `on_prompt` receives the URL and code to show the person while the flow polls.
pub async fn login(
    issuer: Issuer,
    http: &reqwest::Client,
    on_prompt: impl FnOnce(&DevicePrompt),
) -> anyhow::Result<OAuthTokens> {
    match issuer {
        Issuer::Xai => xai::device_login(http, on_prompt).await,
        Issuer::ChatGpt => chatgpt::device_login(http, on_prompt).await,
    }
}

/// An OAuth grant, refreshed on demand and written back after each refresh.
#[derive(Debug)]
pub struct OAuthGrant {
    issuer: Issuer,
    store: TokenStore,
    /// Refresh tokens rotate on use, so refreshes must never run concurrently.
    refresh_lock: Mutex<()>,
}

impl OAuthGrant {
    pub fn new(issuer: Issuer, store: TokenStore) -> Self {
        Self {
            issuer,
            store,
            refresh_lock: Mutex::new(()),
        }
    }

    fn method(&self) -> AuthMethod {
        match &self.store {
            TokenStore::File(TokenFile::CodexCli(_)) => AuthMethod::CodexCli,
            _ => AuthMethod::Login,
        }
    }

    fn describe(&self) -> String {
        match &self.store {
            TokenStore::File(TokenFile::CodexCli(path)) => {
                format!("Codex CLI login ({})", path.display())
            }
            _ => format!("{} login", self.issuer.display_name()),
        }
    }

    async fn tokens(
        &self,
        http: &reqwest::Client,
        force_refresh: bool,
    ) -> anyhow::Result<OAuthTokens> {
        let _guard = self.refresh_lock.lock().await;
        self.store
            .update(|tokens| async move {
                if !force_refresh && !tokens.expires_soon() {
                    return Ok(None);
                }
                let refreshed = self.issuer.refresh(http, &tokens).await.with_context(|| {
                    format!("refreshing the {} failed; log in again", self.describe())
                })?;
                Ok(Some(refreshed))
            })
            .await
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
    async fn is_available(&self) -> anyhow::Result<bool> {
        match self {
            Self::ApiKey(key) => Ok(key.value().is_some()),
            Self::OAuth(grant) => grant.store.exists().await,
        }
    }

    fn method(&self) -> AuthMethod {
        match self {
            Self::ApiKey(_) => AuthMethod::ApiKey,
            Self::OAuth(grant) => grant.method(),
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

/// Which kind of credential a provider is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    ApiKey,
    /// A login made with `ai-image-mcp login`.
    Login,
    /// The Codex CLI's own login.
    CodexCli,
}

/// Whether a provider can authenticate right now, and how.
#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub method: Option<AuthMethod>,
    pub detail: String,
}

impl AuthStatus {
    pub fn ready(&self) -> bool {
        self.method.is_some()
    }
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

    async fn active(&self) -> anyhow::Result<Option<&Credential>> {
        for credential in &self.credentials {
            if credential.is_available().await? {
                return Ok(Some(credential));
            }
        }
        Ok(None)
    }

    pub async fn status(&self) -> AuthStatus {
        match self.active().await {
            Ok(Some(credential)) => AuthStatus {
                method: Some(credential.method()),
                detail: credential.describe(),
            },
            Ok(None) => AuthStatus {
                method: None,
                detail: format!("not connected: {}", self.setup_hint),
            },
            Err(error) => AuthStatus {
                method: None,
                detail: format!("credential check failed: {error:#}"),
            },
        }
    }

    /// Resolves the bearer token to send, refreshing an OAuth grant when it is about to expire.
    pub async fn bearer(
        &self,
        http: &reqwest::Client,
        force_refresh: bool,
    ) -> anyhow::Result<Bearer> {
        let Some(credential) = self.active().await? else {
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
