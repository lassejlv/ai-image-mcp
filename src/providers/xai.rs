//! xAI Grok Imagine. Works with a SuperGrok / X Premium+ login or an xAI API key.

use anyhow::bail;
use serde_json::{Map, Value, json};

use super::images_api::ImagesResponse;
use super::{Client, merge_json, newest_first, put};
use crate::http;
use crate::image::{GeneratedImage, ImageRequest, Quality};

#[derive(Debug)]
pub struct Xai {
    pub(super) client: Client,
}

impl Xai {
    pub async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let c = &self.client;
        let body = &request_body(request)?;
        let path = if request.input_images.is_empty() {
            "images/generations"
        } else {
            "images/edits"
        };
        let response: ImagesResponse = c
            .authorized(|bearer| async move {
                let sent = c
                    .http
                    .post(c.url(path))
                    .bearer_auth(bearer.token)
                    .json(body)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?;
        response.into_images(&c.http).await
    }
}

#[derive(Debug, serde::Deserialize)]
struct ModelList {
    models: Vec<ModelEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    created: i64,
}

impl Xai {
    /// Models from `GET /image-generation-models`, newest first.
    pub async fn models(&self) -> anyhow::Result<Vec<String>> {
        let c = &self.client;
        let list: ModelList = c
            .authorized(|bearer| async move {
                let sent = c
                    .http
                    .get(c.url("image-generation-models"))
                    .bearer_auth(bearer.token)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?;
        Ok(newest_first(
            list.models.into_iter().map(|m| (m.created, m.id)),
        ))
    }
}

fn request_body(request: &ImageRequest) -> anyhow::Result<Map<String, Value>> {
    let s = &request.settings;
    if s.quality == Some(Quality::High) {
        bail!("xAI supports quality low, medium or auto");
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("prompt".into(), json!(request.prompt));
    body.insert("response_format".into(), json!("b64_json"));
    put(&mut body, "n", s.n);
    put(&mut body, "aspect_ratio", s.aspect_ratio.as_deref());
    put(
        &mut body,
        "resolution",
        s.resolution.as_deref().map(str::to_lowercase),
    );
    put(&mut body, "quality", s.quality);
    let images: Vec<Value> = request
        .input_images
        .iter()
        .map(|image| json!({ "type": "image_url", "url": image.data_uri() }))
        .collect();
    match images.as_slice() {
        [] => {}
        [single] => {
            body.insert("image".into(), single.clone());
        }
        _ => {
            body.insert("images".into(), Value::Array(images));
        }
    }
    merge_json(&mut body, &s.extra);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{ImageSettings, InputImage};

    fn request(settings: ImageSettings, images: usize) -> ImageRequest {
        ImageRequest {
            prompt: "a lighthouse".into(),
            model: "grok-imagine-image-2.0".into(),
            settings,
            input_images: vec![
                InputImage {
                    data: b"png".to_vec(),
                    mime_type: "image/png".into(),
                };
                images
            ],
        }
    }

    #[test]
    fn generation_body_uses_aspect_ratio_and_base64_output() {
        let settings = ImageSettings {
            aspect_ratio: Some("16:9".into()),
            resolution: Some("2K".into()),
            quality: Some(Quality::Medium),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(request_body(&request(settings, 0)).unwrap()),
            json!({
                "model": "grok-imagine-image-2.0",
                "prompt": "a lighthouse",
                "response_format": "b64_json",
                "aspect_ratio": "16:9",
                "resolution": "2k",
                "quality": "medium"
            })
        );
    }

    #[test]
    fn edits_send_one_image_or_an_array() {
        let one = request_body(&request(ImageSettings::default(), 1)).unwrap();
        assert_eq!(
            one["image"],
            json!({"type": "image_url", "url": "data:image/png;base64,cG5n"})
        );
        let two = request_body(&request(ImageSettings::default(), 2)).unwrap();
        assert_eq!(two["images"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn rejects_high_quality() {
        let settings = ImageSettings {
            quality: Some(Quality::High),
            ..Default::default()
        };
        assert!(request_body(&request(settings, 0)).is_err());
    }
}
