//! ChatGPT OAuth: the device-code flow the Codex CLI uses, which lets a ChatGPT plan
//! generate images through the Codex backend instead of paid API credits.

use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use super::DevicePrompt;
use super::tokens::{OAuthTokens, TokenResponse};
use crate::http;

const AUTH_ORIGIN: &str = "https://auth.openai.com";
/// The Codex CLI's public OAuth client.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SERVICE: &str = "ChatGPT auth";
/// How long OpenAI keeps a device code valid.
const DEVICE_CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Deserialize)]
struct UserCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceGrant {
    authorization_code: String,
    code_verifier: String,
}

/// Accepts the polling interval as a number or a numeric string.
fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        Number(u64),
        String(String),
    }
    Ok(match Option::<NumberOrString>::deserialize(deserializer)? {
        Some(NumberOrString::Number(n)) => Some(n),
        Some(NumberOrString::String(s)) => s.trim().parse().ok(),
        None => None,
    })
}

/// Runs OpenAI's device-code flow and exchanges the result for ChatGPT tokens.
pub async fn device_login(
    http: &reqwest::Client,
    on_prompt: impl FnOnce(&DevicePrompt),
) -> anyhow::Result<OAuthTokens> {
    let code: UserCode = http::json(
        SERVICE,
        http.post(format!("{AUTH_ORIGIN}/api/accounts/deviceauth/usercode"))
            .json(&json!({ "client_id": CLIENT_ID }))
            .send()
            .await?,
    )
    .await?;
    on_prompt(&DevicePrompt {
        verification_uri: format!("{AUTH_ORIGIN}/codex/device"),
        user_code: code.user_code.clone(),
    });

    let interval = Duration::from_secs(code.interval.unwrap_or(5).max(1));
    let deadline = tokio::time::Instant::now() + DEVICE_CODE_LIFETIME;
    let grant = loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for the ChatGPT login to be approved");
        }
        tokio::time::sleep(interval).await;
        let response = http
            .post(format!("{AUTH_ORIGIN}/api/accounts/deviceauth/token"))
            .json(&json!({ "device_auth_id": code.device_auth_id, "user_code": code.user_code }))
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            continue;
        }
        break http::json::<DeviceGrant>(SERVICE, response).await?;
    };

    let tokens: TokenResponse = http::json(
        SERVICE,
        http.post(format!("{AUTH_ORIGIN}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", grant.authorization_code.as_str()),
                ("code_verifier", grant.code_verifier.as_str()),
                (
                    "redirect_uri",
                    &format!("{AUTH_ORIGIN}/deviceauth/callback"),
                ),
            ])
            .send()
            .await?,
    )
    .await
    .context("exchanging the ChatGPT authorization code")?;
    OAuthTokens::from_response(tokens, None)
}

/// Exchanges the refresh token for a new access token.
pub async fn refresh(http: &reqwest::Client, tokens: &OAuthTokens) -> anyhow::Result<OAuthTokens> {
    let body: TokenResponse = http::json(
        SERVICE,
        http.post(format!("{AUTH_ORIGIN}/oauth/token"))
            .json(&json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": tokens.refresh_token,
                "scope": "openid profile email",
            }))
            .send()
            .await?,
    )
    .await?;
    OAuthTokens::from_response(body, Some(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_code_accepts_both_field_spellings_and_string_interval() {
        let a: UserCode =
            serde_json::from_str(r#"{"device_auth_id":"d","user_code":"AB-CD","interval":"5"}"#)
                .unwrap();
        let b: UserCode =
            serde_json::from_str(r#"{"device_auth_id":"d","usercode":"AB-CD","interval":3}"#)
                .unwrap();
        assert_eq!((a.user_code.as_str(), a.interval), ("AB-CD", Some(5)));
        assert_eq!((b.user_code.as_str(), b.interval), ("AB-CD", Some(3)));
    }
}
