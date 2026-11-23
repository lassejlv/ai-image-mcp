//! Google Gemini native image generation through `generateContent`.

use anyhow::bail;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Client, merge_json};
use crate::http;
use crate::image::{GeneratedImage, ImageRequest};

#[derive(Debug)]
pub struct Gemini {
    pub(super) client: Client,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    prompt_feedback: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: Option<Content>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Content {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part {
    text: Option<String>,
    inline_data: Option<InlineData>,
    #[serde(default)]
    thought: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineData {
    mime_type: String,
    data: String,
}

impl Gemini {
    /// Gemini returns one image per call, so `n` images take `n` calls.
    pub async fn generate(&self, request: &ImageRequest) -> anyhow::Result<Vec<GeneratedImage>> {
        let c = &self.client;
        let body = &request_body(request);
        let path = &format!("models/{}:generateContent", request.model);
        let mut images = Vec::new();
        for _ in 0..request.settings.count() {
            let response: GenerateContentResponse = c
                .authorized(|bearer| async move {
                    let sent = c
                        .http
                        .post(c.url(path))
                        .header("x-goog-api-key", bearer.token)
                        .json(body)
                        .send()
                        .await?;
                    http::json(&c.name, sent).await
                })
                .await?;
            images.extend(extract_images(response)?);
        }
        Ok(images)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPage {
    #[serde(default)]
    models: Vec<ModelEntry>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelEntry {
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

impl ModelEntry {
    /// Gemini lists no output modalities, so image models are recognised by name: the
    /// `*-image` ids and the Nano Banana family.
    fn is_image_model(&self) -> bool {
        let named = |s: &str| {
            let s = s.to_ascii_lowercase();
            s.contains("image") || s.contains("banana")
        };
        (named(&self.name) || named(&self.display_name))
            && self
                .supported_generation_methods
                .iter()
                .any(|g| g == "generateContent")
    }
}

impl Gemini {
    /// Image-capable models from `GET /models`, following every page.
    pub async fn models(&self) -> anyhow::Result<Vec<String>> {
        let c = &self.client;
        let mut models = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let token = page_token.as_deref();
            let page: ModelPage = c
                .authorized(|bearer| async move {
                    let mut url = format!("{}?pageSize=1000", c.url("models"));
                    if let Some(token) = token {
                        url.push_str(&format!("&pageToken={token}"));
                    }
                    let sent = c
                        .http
                        .get(url)
                        .header("x-goog-api-key", bearer.token)
                        .send()
                        .await?;
                    http::json(&c.name, sent).await
                })
                .await?;
            models.extend(
                page.models
                    .into_iter()
                    .filter(|m| m.is_image_model())
                    .map(|m| m.name.trim_start_matches("models/").to_owned()),
            );
            match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(next) => page_token = Some(next),
                None => return Ok(models),
            }
        }
    }
}

fn request_body(request: &ImageRequest) -> Map<String, Value> {
    let s = &request.settings;
    let mut parts = vec![json!({ "text": request.prompt })];
    parts.extend(request.input_images.iter().map(
        |image| json!({ "inlineData": { "mimeType": image.mime_type, "data": image.base64() } }),
    ));
    let mut image_config = Map::new();
    if let Some(ratio) = &s.aspect_ratio {
        image_config.insert("aspectRatio".into(), json!(ratio));
    }
    if let Some(resolution) = &s.resolution {
        image_config.insert("imageSize".into(), json!(resolution.to_uppercase()));
    }
    let mut generation_config = Map::new();
    generation_config.insert("responseModalities".into(), json!(["IMAGE"]));
    if !image_config.is_empty() {
        generation_config.insert("imageConfig".into(), Value::Object(image_config));
    }
    let mut body = Map::new();
    body.insert(
        "contents".into(),
        json!([{ "role": "user", "parts": parts }]),
    );
    body.insert("generationConfig".into(), Value::Object(generation_config));
    merge_json(&mut body, &s.extra);
    body
}

fn extract_images(response: GenerateContentResponse) -> anyhow::Result<Vec<GeneratedImage>> {
    let mut images = Vec::new();
    let mut notes = Vec::new();
    for candidate in response.candidates {
        let parts = candidate.content.map(|c| c.parts).unwrap_or_default();
        for part in parts.into_iter().filter(|p| !p.thought) {
            if let Some(inline) = part.inline_data {
                images.push(GeneratedImage::from_base64(
                    &inline.data,
                    Some(&inline.mime_type),
                )?);
            } else if let Some(text) = part.text {
                notes.push(text);
            }
        }
        if let Some(reason) = candidate.finish_reason.filter(|r| r != "STOP") {
            notes.push(format!("finish reason {reason}"));
        }
    }
    if images.is_empty() {
        if let Some(feedback) = response.prompt_feedback {
            notes.push(format!("prompt feedback {feedback}"));
        }
        bail!("Gemini returned no image: {}", notes.join("; "));
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageSettings;

    #[test]
    fn image_config_uses_gemini_casing() {
        let request = ImageRequest {
            prompt: "a banana".into(),
            model: "gemini-nano-banana-2.1".into(),
            settings: ImageSettings {
                aspect_ratio: Some("16:9".into()),
                resolution: Some("2k".into()),
                extra: json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}}})
                    .as_object()
                    .cloned()
                    .unwrap(),
                ..Default::default()
            },
            input_images: vec![],
        };
        let body = Value::Object(request_body(&request));
        assert_eq!(
            body["generationConfig"],
            json!({
                "responseModalities": ["IMAGE"],
                "imageConfig": {"aspectRatio": "16:9", "imageSize": "2K"},
                "thinkingConfig": {"thinkingLevel": "high"}
            })
        );
    }

    #[test]
    fn explains_a_response_without_images() {
        let response: GenerateContentResponse = serde_json::from_value(json!({
            "candidates": [{"content": {"parts": [{"text": "I can't draw that."}]}, "finishReason": "IMAGE_SAFETY"}]
        }))
        .unwrap();
        let error = extract_images(response).unwrap_err().to_string();
        assert!(
            error.contains("I can't draw that.") && error.contains("IMAGE_SAFETY"),
            "{error}"
        );
    }
}
