//! OAuth token storage: this server's own files and the Codex CLI's `auth.json`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Seconds before expiry at which a token counts as expired, so a request never races it.
const EXPIRY_SKEW_SECS: i64 = 120;

/// An OAuth grant: the short-lived access token and what is needed to renew it.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Unix seconds; used when the access token is not a JWT with an `exp` claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("expires_at", &self.expiry())
            .finish_non_exhaustive()
    }
}

impl OAuthTokens {
    /// Builds tokens from an OAuth token-endpoint response, keeping `previous` values the
    /// response omits (refresh tokens are not always rotated).
    pub fn from_response(
        response: TokenResponse,
        previous: Option<&OAuthTokens>,
    ) -> anyhow::Result<Self> {
        let refresh_token = response
            .refresh_token
            .filter(|t| !t.is_empty())
            .or_else(|| previous.map(|p| p.refresh_token.clone()))
            .ok_or_else(|| anyhow!("token response did not include a refresh token"))?;
        Ok(Self {
            access_token: response.access_token,
            refresh_token,
            id_token: response
                .id_token
                .or_else(|| previous.and_then(|p| p.id_token.clone())),
            expires_at: response.expires_in.map(|secs| now() + secs),
        })
    }

    fn expiry(&self) -> Option<i64> {
        jwt_claims(&self.access_token)
            .and_then(|claims| claims.get("exp").and_then(Value::as_i64))
            .or(self.expires_at)
    }

    pub fn expires_soon(&self) -> bool {
        self.expiry()
            .is_some_and(|exp| exp - now() <= EXPIRY_SKEW_SECS)
    }

    /// The ChatGPT workspace id carried in the token claims.
    pub fn account_id(&self) -> Option<String> {
        [Some(self.access_token.as_str()), self.id_token.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(jwt_claims)
            .find_map(|claims| {
                claims
                    .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
    }
}

/// The fields of an OAuth token-endpoint response this server uses.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<i64>,
}

/// Where a grant is persisted.
#[derive(Debug, Clone)]
pub enum TokenFile {
    /// A file written by this server, holding [`OAuthTokens`] as JSON.
    Own(PathBuf),
    /// The Codex CLI's `auth.json`; other fields in it are preserved on write.
    CodexCli(PathBuf),
}

impl TokenFile {
    pub fn path(&self) -> &Path {
        match self {
            Self::Own(path) | Self::CodexCli(path) => path,
        }
    }

    pub fn load(&self) -> anyhow::Result<OAuthTokens> {
        let path = self.path();
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        match self {
            Self::Own(_) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Self::CodexCli(_) => {
                let doc: Value = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                let tokens = doc.get("tokens").filter(|t| !t.is_null()).ok_or_else(|| {
                    anyhow!("{} has no ChatGPT login; run `codex login`", path.display())
                })?;
                serde_json::from_value(tokens.clone())
                    .with_context(|| format!("parsing tokens in {}", path.display()))
            }
        }
    }

    pub fn save(&self, tokens: &OAuthTokens) -> anyhow::Result<()> {
        let contents = match self {
            Self::Own(_) => serde_json::to_string_pretty(tokens)?,
            Self::CodexCli(path) => {
                let mut doc: Value = fs::read_to_string(path)
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_else(|| json!({}));
                let entry = &mut doc["tokens"];
                if !entry.is_object() {
                    *entry = json!({});
                }
                entry["access_token"] = json!(tokens.access_token);
                entry["refresh_token"] = json!(tokens.refresh_token);
                if let Some(id_token) = &tokens.id_token {
                    entry["id_token"] = json!(id_token);
                }
                if let Some(account_id) = tokens.account_id() {
                    entry["account_id"] = json!(account_id);
                }
                doc["last_refresh"] = json!(jiff::Timestamp::now().to_string());
                serde_json::to_string_pretty(&doc)?
            }
        };
        write_private(self.path(), contents.as_bytes())
    }
}

/// Writes a file readable only by the owner, replacing it atomically.
fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("token path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Decodes a JWT payload without verifying it; used only for expiry and account metadata.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    fn tokens(access_token: String) -> OAuthTokens {
        OAuthTokens {
            access_token,
            refresh_token: "refresh".into(),
            id_token: None,
            expires_at: None,
        }
    }

    #[test]
    fn expiry_comes_from_the_jwt() {
        assert!(tokens(jwt(json!({"exp": now() + 30}))).expires_soon());
        assert!(!tokens(jwt(json!({"exp": now() + 3600}))).expires_soon());
        assert!(!tokens("opaque".into()).expires_soon());
    }

    #[test]
    fn reads_the_chatgpt_account_id_claim() {
        let access = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_1"}}));
        assert_eq!(tokens(access).account_id().as_deref(), Some("acct_1"));
    }

    #[test]
    fn keeps_previous_refresh_token_when_not_rotated() {
        let previous = tokens("old".into());
        let response = TokenResponse {
            access_token: "new".into(),
            refresh_token: None,
            id_token: None,
            expires_in: Some(60),
        };
        let next = OAuthTokens::from_response(response, Some(&previous)).unwrap();
        assert_eq!(next.refresh_token, "refresh");
        assert!(next.expires_soon());
    }

    #[test]
    fn codex_cli_file_round_trips_and_keeps_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(
            &path,
            json!({"auth_mode": "chatgpt", "tokens": {"access_token": "a", "refresh_token": "r", "id_token": "i"}}).to_string(),
        )
        .unwrap();
        let file = TokenFile::CodexCli(path.clone());

        let mut loaded = file.load().unwrap();
        loaded.access_token = "a2".into();
        file.save(&loaded).unwrap();

        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["auth_mode"], "chatgpt");
        assert_eq!(doc["tokens"]["access_token"], "a2");
        assert_eq!(doc["tokens"]["id_token"], "i");
        assert!(doc["last_refresh"].is_string());
    }
}
