//! QuiverAI's Arrow models, which generate vector SVGs from a prompt and optional
//! reference images.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Client, merge_json, newest_first, put};
use crate::http;
use crate::image::{GeneratedImage, ImageRequest};

#[derive(Debug)]
pub struct Quiver {
    pub(super) client: Client,
}

#[derive(Debug, Deserialize)]
struct SvgResponse {
    #[serde(default)]
    data: Vec<SvgItem>,
}

#[derive(Debug, Deserialize)]
struct SvgItem {
    svg: String,
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    created: i64,
    #[serde(default)]
    supported_operations: Vec<String>,
}

impl Quiver {
    pub async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let c = &self.client;
        let body = &request_body(request);
        let response: SvgResponse = c
            .authorized(|bearer| async move {
                let sent = c
                    .http
                    .post(c.url("svgs/generations"))
                    .bearer_auth(bearer.token)
                    .json(body)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?;
        Ok(response
            .data
            .into_iter()
            .map(|item| GeneratedImage {
                data: item.svg.into_bytes(),
                mime_type: item.mime_type.unwrap_or_else(|| "image/svg+xml".to_owned()),
                revised_prompt: None,
            })
            .collect())
    }

    /// Models from `GET /models` that can generate SVGs, newest first.
    pub async fn models(&self) -> anyhow::Result<Vec<String>> {
        let c = &self.client;
        let list: ModelList = c
            .authorized(|bearer| async move {
                let sent = c
                    .http
                    .get(c.url("models"))
                    .bearer_auth(bearer.token)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?;
        Ok(newest_first(
            list.data
                .into_iter()
                .filter(|m| {
                    m.supported_operations.is_empty()
                        || m.supported_operations
                            .iter()
                            .any(|op| op.contains("generat"))
                })
                .map(|m| (m.created, m.id)),
        ))
    }
}

fn request_body(request: &ImageRequest) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("prompt".into(), json!(request.prompt));
    put(&mut body, "n", request.settings.n);
    if !request.input_images.is_empty() {
        let references: Vec<Value> = request
            .input_images
            .iter()
            .map(|image| json!({ "base64": image.base64() }))
            .collect();
        body.insert("references".into(), Value::Array(references));
    }
    merge_json(&mut body, &request.settings.extra);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{ImageSettings, InputImage};

    #[test]
    fn sends_references_as_base64_and_extra_verbatim() {
        let request = ImageRequest {
            prompt: "a fox logo".into(),
            model: "arrow-2".into(),
            settings: ImageSettings {
                n: Some(2),
                extra: json!({"instructions": "flat, two colours", "temperature": 0.4})
                    .as_object()
                    .cloned()
                    .unwrap(),
                ..Default::default()
            },
            input_images: vec![InputImage {
                data: b"foo".to_vec(),
                mime_type: "image/png".into(),
            }],
        };
        assert_eq!(
            Value::Object(request_body(&request)),
            json!({
                "model": "arrow-2",
                "prompt": "a fox logo",
                "n": 2,
                "references": [{"base64": "Zm9v"}],
                "instructions": "flat, two colours",
                "temperature": 0.4
            })
        );
    }
}
