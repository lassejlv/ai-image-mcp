//! fal.ai's synchronous endpoint (`fal.run/<model>`), covering its whole model catalog.

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Client, merge_json, put};
use crate::http::{self, download_image};
use crate::image::{GeneratedImage, ImageRequest, sniff_mime};

#[derive(Debug)]
pub struct Fal {
    pub(super) client: Client,
}

#[derive(Debug, Deserialize)]
struct FalResponse {
    #[serde(default)]
    images: Vec<FalImage>,
    image: Option<FalImage>,
}

#[derive(Debug, Deserialize)]
struct FalImage {
    url: String,
    content_type: Option<String>,
}

impl Fal {
    pub async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let c = &self.client;
        let body = &request_body(request);
        let path = request.model.as_str();
        let response: FalResponse = c
            .authorized(|bearer| async move {
                let sent = c
                    .http
                    .post(c.url(path))
                    .header("Authorization", format!("Key {}", bearer.token))
                    .json(body)
                    .send()
                    .await?;
                http::json(&c.name, sent).await
            })
            .await?;
        let mut images = Vec::new();
        for image in response.images.into_iter().chain(response.image) {
            images.push(fetch(&c.http, image).await?);
        }
        Ok(images)
    }
}

/// fal returns hosted URLs, or data URIs when `sync_mode` is set.
async fn fetch(http: &reqwest::Client, image: FalImage) -> anyhow::Result<GeneratedImage> {
    let Some(data_uri) = image.url.strip_prefix("data:") else {
        return download_image(http, &image.url).await;
    };
    let (meta, payload) = data_uri
        .split_once(',')
        .context("malformed data URI from fal")?;
    let data = BASE64
        .decode(payload)
        .context("invalid base64 in fal data URI")?;
    let mime_type = meta
        .split(';')
        .next()
        .filter(|m| m.starts_with("image/"))
        .map(str::to_owned)
        .or(image.content_type)
        .or_else(|| sniff_mime(&data).map(str::to_owned))
        .unwrap_or_else(|| "image/png".to_owned());
    Ok(GeneratedImage {
        data,
        mime_type,
        revised_prompt: None,
    })
}

fn request_body(request: &ImageRequest) -> Map<String, Value> {
    let s = &request.settings;
    let mut body = Map::new();
    body.insert("prompt".into(), json!(request.prompt));
    put(&mut body, "num_images", s.n);
    put(&mut body, "image_size", s.size.as_deref().map(image_size));
    put(&mut body, "aspect_ratio", s.aspect_ratio.as_deref());
    put(&mut body, "resolution", s.resolution.as_deref());
    put(&mut body, "seed", s.seed);
    put(&mut body, "negative_prompt", s.negative_prompt.as_deref());
    put(
        &mut body,
        "output_format",
        s.output_format.map(|f| f.as_str()),
    );
    if !request.input_images.is_empty() {
        let urls: Vec<String> = request.input_images.iter().map(|i| i.data_uri()).collect();
        body.insert("image_urls".into(), json!(urls));
    }
    merge_json(&mut body, &s.extra);
    body
}

/// "1280x720" becomes `{width, height}`; named presets such as "landscape_16_9" pass through.
fn image_size(size: &str) -> Value {
    size.split_once(['x', 'X'])
        .and_then(|(w, h)| Some(json!({ "width": w.trim().parse::<u32>().ok()?, "height": h.trim().parse::<u32>().ok()? })))
        .unwrap_or_else(|| json!(size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageSettings;

    #[test]
    fn maps_sizes_and_diffusion_settings() {
        let request = ImageRequest {
            prompt: "a koi pond".into(),
            model: "fal-ai/flux/dev".into(),
            settings: ImageSettings {
                n: Some(2),
                size: Some("1280x720".into()),
                seed: Some(7),
                negative_prompt: Some("text".into()),
                extra: json!({"num_inference_steps": 28})
                    .as_object()
                    .cloned()
                    .unwrap(),
                ..Default::default()
            },
            input_images: vec![],
        };
        assert_eq!(
            Value::Object(request_body(&request)),
            json!({
                "prompt": "a koi pond",
                "num_images": 2,
                "image_size": {"width": 1280, "height": 720},
                "seed": 7,
                "negative_prompt": "text",
                "num_inference_steps": 28
            })
        );
        assert_eq!(image_size("landscape_16_9"), json!("landscape_16_9"));
    }
}
