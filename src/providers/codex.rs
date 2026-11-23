//! GPT Image through a ChatGPT plan, via the Images endpoints of the Codex backend
//! (`chatgpt.com/backend-api/codex`) that the Codex CLI's image tool calls.

use serde_json::{Map, Value, json};

use super::images_api::ImagesResponse;
use super::{Client, merge_json, put};
use crate::http;
use crate::image::{GeneratedImage, ImageRequest};

/// The client name the Codex backend expects in the `originator` header.
const ORIGINATOR: &str = "codex_cli_rs";

#[derive(Debug)]
pub struct Codex {
    pub(super) client: Client,
}

impl Codex {
    pub async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let c = &self.client;
        let body = &request_body(request);
        let path = if request.input_images.is_empty() {
            "images/generations"
        } else {
            "images/edits"
        };
        let response: ImagesResponse = c
            .authorized(|bearer| async move {
                let mut builder = c
                    .http
                    .post(c.url(path))
                    .bearer_auth(bearer.token)
                    .header("originator", ORIGINATOR)
                    .json(body);
                if let Some(account_id) = bearer.account_id {
                    builder = builder.header("ChatGPT-Account-ID", account_id);
                }
                http::json(&c.name, builder.send().await?).await
            })
            .await?;
        response.into_images(&c.http).await
    }
}

fn request_body(request: &ImageRequest) -> Map<String, Value> {
    let s = &request.settings;
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("prompt".into(), json!(request.prompt));
    put(&mut body, "n", s.n);
    put(&mut body, "size", s.size.as_deref());
    put(&mut body, "quality", s.quality);
    put(&mut body, "background", s.background);
    if !request.input_images.is_empty() {
        let images: Vec<Value> = request
            .input_images
            .iter()
            .map(|image| json!({ "image_url": image.data_uri() }))
            .collect();
        body.insert("images".into(), Value::Array(images));
    }
    merge_json(&mut body, &s.extra);
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{ImageSettings, InputImage, Quality};

    #[test]
    fn edit_body_inlines_images_as_data_urls() {
        let request = ImageRequest {
            prompt: "add a red hat".into(),
            model: "gpt-image-2".into(),
            settings: ImageSettings {
                size: Some("1024x1536".into()),
                quality: Some(Quality::High),
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
                "model": "gpt-image-2",
                "prompt": "add a red hat",
                "size": "1024x1536",
                "quality": "high",
                "images": [{"image_url": "data:image/png;base64,Zm9v"}]
            })
        );
    }
}
