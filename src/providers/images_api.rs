//! The OpenAI Images API response shape, shared by OpenAI, xAI, and the Codex backend.

use serde::Deserialize;

use crate::http::download_image;
use crate::image::{GeneratedImage, OutputFormat};

#[derive(Debug, Deserialize)]
pub struct ImagesResponse {
    #[serde(default)]
    data: Vec<ImageItem>,
    output_format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImageItem {
    b64_json: Option<String>,
    url: Option<String>,
    revised_prompt: Option<String>,
    mime_type: Option<String>,
}

impl ImagesResponse {
    /// Decodes inline images and downloads URL results.
    pub async fn into_images(self, http: &reqwest::Client) -> anyhow::Result<Vec<GeneratedImage>> {
        let format_mime = self.output_format.as_deref().and_then(mime_for_format);
        let mut images = Vec::with_capacity(self.data.len());
        for item in self.data {
            let mut image = match (item.b64_json, item.url) {
                (Some(b64), _) => {
                    GeneratedImage::from_base64(&b64, item.mime_type.as_deref().or(format_mime))?
                }
                (None, Some(url)) => download_image(http, &url).await?,
                (None, None) => {
                    anyhow::bail!("an image in the response had neither b64_json nor url")
                }
            };
            image.revised_prompt = item.revised_prompt;
            images.push(image);
        }
        Ok(images)
    }
}

fn mime_for_format(format: &str) -> Option<&'static str> {
    match format {
        "png" => Some("image/png"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        }
    }
}
