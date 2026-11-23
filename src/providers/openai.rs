//! The OpenAI Images API, also spoken by many other services through `base_url`.

use anyhow::Context;
use reqwest::multipart::{Form, Part};
use serde_json::{Map, Value, json};

use super::images_api::ImagesResponse;
use super::{Client, merge_json, put};
use crate::http;
use crate::image::{GeneratedImage, ImageRequest, extension_for};

#[derive(Debug)]
pub struct OpenAi {
    pub(super) client: Client,
}

impl OpenAi {
    pub async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let c = &self.client;
        let body = &request_body(request);
        let response: ImagesResponse = if request.input_images.is_empty() {
            c.authorized(|bearer| async move {
                let sent = c
                    .http
                    .post(c.url("images/generations"))
                    .bearer_auth(bearer.token)
                    .json(body)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?
        } else {
            c.authorized(|bearer| async move {
                let form = edit_form(request, body)?;
                let sent = c
                    .http
                    .post(c.url("images/edits"))
                    .bearer_auth(bearer.token)
                    .multipart(form)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?
        };
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
    put(
        &mut body,
        "output_format",
        s.output_format.map(|f| f.as_str()),
    );
    put(&mut body, "output_compression", s.output_compression);
    merge_json(&mut body, &s.extra);
    body
}

/// Edits are multipart: every body field becomes a text part and each image an `image[]` file.
fn edit_form(request: &ImageRequest, body: &Map<String, Value>) -> anyhow::Result<Form> {
    let mut form = Form::new();
    for (key, value) in body {
        let text = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        form = form.text(key.clone(), text);
    }
    for (i, image) in request.input_images.iter().enumerate() {
        let part = Part::bytes(image.data.clone())
            .file_name(format!(
                "image-{}.{}",
                i + 1,
                extension_for(&image.mime_type)
            ))
            .mime_str(&image.mime_type)
            .with_context(|| format!("invalid MIME type {}", image.mime_type))?;
        form = form.part("image[]", part);
    }
    Ok(form)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Background, ImageSettings, OutputFormat, Quality};

    #[test]
    fn maps_supported_settings_and_extra() {
        let request = ImageRequest {
            prompt: "a fox".into(),
            model: "gpt-image-2".into(),
            settings: ImageSettings {
                n: Some(2),
                size: Some("1536x1024".into()),
                quality: Some(Quality::High),
                background: Some(Background::Transparent),
                output_format: Some(OutputFormat::Webp),
                output_compression: Some(80),
                aspect_ratio: Some("16:9".into()),
                extra: json!({"moderation": "low"}).as_object().cloned().unwrap(),
                ..Default::default()
            },
            input_images: vec![],
        };
        assert_eq!(
            Value::Object(request_body(&request)),
            json!({
                "model": "gpt-image-2",
                "prompt": "a fox",
                "n": 2,
                "size": "1536x1024",
                "quality": "high",
                "background": "transparent",
                "output_format": "webp",
                "output_compression": 80,
                "moderation": "low"
            })
        );
    }
}
