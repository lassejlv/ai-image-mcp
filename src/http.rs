//! HTTP helpers shared by providers and login flows.

use std::fmt;

use anyhow::Context;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::image::{GeneratedImage, sniff_mime};

/// A non-success HTTP response, with the service's own error message when it sent one.
#[derive(Debug)]
pub struct HttpStatusError {
    pub service: String,
    pub status: StatusCode,
    pub message: String,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} returned HTTP {}: {}",
            self.service,
            self.status.as_u16(),
            self.message
        )
    }
}

impl std::error::Error for HttpStatusError {}

/// Returns the response unchanged when it succeeded, otherwise an [`HttpStatusError`].
pub async fn ensure_success(service: &str, response: Response) -> anyhow::Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(HttpStatusError {
        service: service.to_owned(),
        status,
        message: error_message(&body),
    }
    .into())
}

/// Checks the status and decodes the JSON body.
pub async fn json<T: DeserializeOwned>(service: &str, response: Response) -> anyhow::Result<T> {
    let response = ensure_success(service, response).await?;
    let bytes = response.bytes().await?;
    serde_json::from_slice(&bytes).with_context(|| format!("{service} sent an unexpected response"))
}

/// True when the error is an HTTP 401 from the remote service.
pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpStatusError>()
        .is_some_and(|e| e.status == StatusCode::UNAUTHORIZED)
}

/// Downloads an image a provider returned by URL.
pub async fn download_image(http: &reqwest::Client, url: &str) -> anyhow::Result<GeneratedImage> {
    let response = ensure_success("image download", http.get(url).send().await?).await?;
    let header_mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("image/"))
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_owned());
    let data = response.bytes().await?.to_vec();
    let mime_type = header_mime
        .or_else(|| sniff_mime(&data).map(str::to_owned))
        .unwrap_or_else(|| "image/png".to_owned());
    Ok(GeneratedImage {
        data,
        mime_type,
        revised_prompt: None,
    })
}

fn error_message(body: &str) -> String {
    let from_json = serde_json::from_str::<Value>(body).ok().and_then(|json| {
        [
            json.pointer("/error/message"),
            json.pointer("/error_description"),
            json.pointer("/detail/message"),
            json.pointer("/detail"),
            json.pointer("/message"),
            json.pointer("/error"),
        ]
        .into_iter()
        .flatten()
        .find_map(|v| v.as_str().map(str::to_owned))
    });
    let message = from_json.unwrap_or_else(|| body.trim().to_owned());
    if message.is_empty() {
        return "no error details".to_owned();
    }
    message.chars().take(800).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_error_messages() {
        assert_eq!(
            error_message(r#"{"error":{"message":"bad size"}}"#),
            "bad size"
        );
        assert_eq!(
            error_message(r#"{"error":"invalid_grant"}"#),
            "invalid_grant"
        );
        assert_eq!(
            error_message(r#"{"detail":"Unauthorized"}"#),
            "Unauthorized"
        );
        assert_eq!(error_message("plain text"), "plain text");
        assert_eq!(error_message(""), "no error details");
    }
}
