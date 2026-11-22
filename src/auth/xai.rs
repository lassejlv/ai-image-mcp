//! xAI OAuth: the device-code flow that turns a SuperGrok or X Premium+ subscription
//! into a bearer token for `api.x.ai`.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use reqwest::Url;
use serde::Deserialize;

use super::DevicePrompt;
use super::tokens::{OAuthTokens, TokenResponse};
use crate::http;

const ISSUER: &str = "https://auth.x.ai";
/// xAI's public OAuth client for coding agents; device-code public clients have no secret.
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const SERVICE: &str = "xAI auth";

#[derive(Debug, Deserialize)]
struct Discovery {
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

fn default_expires_in() -> u64 {
    900
}

#[derive(Debug, Deserialize)]
struct OAuthError {
    error: String,
}

/// Runs the RFC 8628 device-code flow against `auth.x.ai`.
pub async fn device_login(
    http: &reqwest::Client,
    on_prompt: impl FnOnce(&DevicePrompt),
) -> anyhow::Result<OAuthTokens> {
    let discovery = discover(http).await?;
    let device_endpoint = discovery
        .device_authorization_endpoint
        .unwrap_or_else(|| format!("{ISSUER}/oauth2/device/code"));
    ensure_xai_https(&device_endpoint)?;

    let device: DeviceCode = http::json(
        SERVICE,
        http.post(&device_endpoint)
            .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
            .send()
            .await?,
    )
    .await?;
    on_prompt(&DevicePrompt {
        verification_uri: device
            .verification_uri_complete
            .clone()
            .unwrap_or(device.verification_uri.clone()),
        user_code: device.user_code.clone(),
    });

    let mut interval = device.interval.max(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = http
            .post(&discovery.token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ])
            .send()
            .await?;
        if response.status().is_success() {
            let tokens: TokenResponse = response
                .json()
                .await
                .context("decoding xAI token response")?;
            return OAuthTokens::from_response(tokens, None);
        }
        let body = response.text().await.unwrap_or_default();
        match serde_json::from_str::<OAuthError>(&body)
            .map(|e| e.error)
            .as_deref()
        {
            Ok("authorization_pending") => {}
            Ok("slow_down") => interval = (interval + 5).min(30),
            _ => bail!("xAI device authorization failed: {}", body.trim()),
        }
    }
    bail!("timed out waiting for the xAI login to be approved")
}

/// Exchanges the refresh token for a new access token. xAI rotates refresh tokens on use.
pub async fn refresh(http: &reqwest::Client, tokens: &OAuthTokens) -> anyhow::Result<OAuthTokens> {
    let discovery = discover(http).await?;
    let response = http
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", tokens.refresh_token.as_str()),
        ])
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        bail!(
            "xAI refused the refresh (HTTP 403): this subscription tier is not entitled to API access"
        );
    }
    let body: TokenResponse = http::json(SERVICE, response).await?;
    OAuthTokens::from_response(body, Some(tokens))
}

async fn discover(http: &reqwest::Client) -> anyhow::Result<Discovery> {
    let discovery: Discovery = http::json(
        SERVICE,
        http.get(format!("{ISSUER}/.well-known/openid-configuration"))
            .send()
            .await?,
    )
    .await?;
    ensure_xai_https(&discovery.token_endpoint)?;
    Ok(discovery)
}

/// Refuses endpoints outside xAI so a tampered discovery document cannot capture tokens.
fn ensure_xai_https(endpoint: &str) -> anyhow::Result<()> {
    let url = Url::parse(endpoint).with_context(|| format!("invalid xAI endpoint {endpoint}"))?;
    let host = url.host_str().unwrap_or_default();
    ensure!(
        url.scheme() == "https" && (host == "x.ai" || host.ends_with(".x.ai")),
        "refusing non-xAI auth endpoint {endpoint}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_xai_https_endpoints_are_trusted() {
        assert!(ensure_xai_https("https://auth.x.ai/oauth2/token").is_ok());
        assert!(ensure_xai_https("http://auth.x.ai/oauth2/token").is_err());
        assert!(ensure_xai_https("https://evil.example/x.ai").is_err());
        assert!(ensure_xai_https("https://notx.ai/token").is_err());
    }
}
