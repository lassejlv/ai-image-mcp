//! Provider-neutral image request and result types.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Output quality tier. Providers that do not support a tier reject or remap it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Medium,
    High,
    Auto,
}

/// Background treatment for providers that can render transparency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    Transparent,
    Opaque,
    Auto,
}

/// Encoded file format of the generated image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
}

/// Generation settings shared by every provider.
///
/// Each provider maps the settings it understands onto its own API and reports the rest
/// as ignored, so one request shape works everywhere.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageSettings {
    /// Number of images to generate.
    #[schemars(range(min = 1, max = 10))]
    pub n: Option<u8>,
    /// Pixel size such as "1024x1024", "1536x1024" or "auto" (OpenAI, Codex, fal).
    pub size: Option<String>,
    /// Aspect ratio such as "1:1", "16:9" or "9:16" (xAI, Gemini, fal).
    pub aspect_ratio: Option<String>,
    /// Resolution tier such as "1k", "2k" or "4k" (xAI, Gemini, fal).
    pub resolution: Option<String>,
    /// Quality tier.
    pub quality: Option<Quality>,
    /// Background treatment (OpenAI, Codex).
    pub background: Option<Background>,
    /// File format of the result (OpenAI, fal).
    pub output_format: Option<OutputFormat>,
    /// Compression level 0-100 for jpeg and webp output (OpenAI).
    #[schemars(range(max = 100))]
    pub output_compression: Option<u8>,
    /// Seed for reproducible results (fal).
    pub seed: Option<u64>,
    /// What the image should not contain (fal).
    pub negative_prompt: Option<String>,
    /// Provider-specific request fields merged verbatim into the request body,
    /// for any option this server does not model.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One field of [`ImageSettings`], used to report which settings a provider honours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Setting {
    N,
    Size,
    AspectRatio,
    Resolution,
    Quality,
    Background,
    OutputFormat,
    OutputCompression,
    Seed,
    NegativePrompt,
}

impl Setting {
    pub fn name(self) -> &'static str {
        match self {
            Self::N => "n",
            Self::Size => "size",
            Self::AspectRatio => "aspect_ratio",
            Self::Resolution => "resolution",
            Self::Quality => "quality",
            Self::Background => "background",
            Self::OutputFormat => "output_format",
            Self::OutputCompression => "output_compression",
            Self::Seed => "seed",
            Self::NegativePrompt => "negative_prompt",
        }
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl ImageSettings {
    /// Fills every unset field from `defaults`; `extra` keys set here win over the defaults.
    pub fn with_defaults(mut self, defaults: &ImageSettings) -> Self {
        let mut extra = defaults.extra.clone();
        extra.append(&mut self.extra);
        Self {
            n: self.n.or(defaults.n),
            size: self.size.or_else(|| defaults.size.clone()),
            aspect_ratio: self.aspect_ratio.or_else(|| defaults.aspect_ratio.clone()),
            resolution: self.resolution.or_else(|| defaults.resolution.clone()),
            quality: self.quality.or(defaults.quality),
            background: self.background.or(defaults.background),
            output_format: self.output_format.or(defaults.output_format),
            output_compression: self.output_compression.or(defaults.output_compression),
            seed: self.seed.or(defaults.seed),
            negative_prompt: self
                .negative_prompt
                .or_else(|| defaults.negative_prompt.clone()),
            extra,
        }
    }

    /// Settings that carry a value.
    pub fn present(&self) -> Vec<Setting> {
        [
            (Setting::N, self.n.is_some()),
            (Setting::Size, self.size.is_some()),
            (Setting::AspectRatio, self.aspect_ratio.is_some()),
            (Setting::Resolution, self.resolution.is_some()),
            (Setting::Quality, self.quality.is_some()),
            (Setting::Background, self.background.is_some()),
            (Setting::OutputFormat, self.output_format.is_some()),
            (
                Setting::OutputCompression,
                self.output_compression.is_some(),
            ),
            (Setting::Seed, self.seed.is_some()),
            (Setting::NegativePrompt, self.negative_prompt.is_some()),
        ]
        .into_iter()
        .filter_map(|(setting, set)| set.then_some(setting))
        .collect()
    }

    /// Number of images requested, at least one.
    pub fn count(&self) -> u8 {
        self.n.unwrap_or(1).max(1)
    }
}

/// An image passed to the provider as an edit target or reference.
#[derive(Clone, PartialEq, Eq)]
pub struct InputImage {
    pub data: Vec<u8>,
    pub mime_type: String,
}

impl fmt::Debug for InputImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputImage")
            .field("mime_type", &self.mime_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl InputImage {
    pub fn base64(&self) -> String {
        BASE64.encode(&self.data)
    }

    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.base64())
    }
}

/// A fully resolved request handed to a provider.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub prompt: String,
    pub model: String,
    pub settings: ImageSettings,
    pub input_images: Vec<InputImage>,
}

/// One image returned by a provider.
#[derive(Clone)]
pub struct GeneratedImage {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub revised_prompt: Option<String>,
}

impl fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedImage")
            .field("mime_type", &self.mime_type)
            .field("bytes", &self.data.len())
            .field("revised_prompt", &self.revised_prompt)
            .finish()
    }
}

impl GeneratedImage {
    /// Decodes a base64 payload, sniffing the format when the provider does not state it.
    pub fn from_base64(b64: &str, mime_type: Option<&str>) -> anyhow::Result<Self> {
        let data = BASE64.decode(b64.trim())?;
        let mime_type = mime_type
            .map(str::to_owned)
            .or_else(|| sniff_mime(&data).map(str::to_owned))
            .unwrap_or_else(|| "image/png".to_owned());
        Ok(Self {
            data,
            mime_type,
            revised_prompt: None,
        })
    }
}

/// Detects common image formats from their magic bytes.
pub fn sniff_mime(data: &[u8]) -> Option<&'static str> {
    match data {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        _ => None,
    }
}

/// File extension for an image MIME type.
pub fn extension_for(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_values_win_over_defaults() {
        let defaults = ImageSettings {
            quality: Some(Quality::High),
            size: Some("1024x1024".into()),
            extra: json!({"moderation": "low", "style": "a"})
                .as_object()
                .cloned()
                .unwrap(),
            ..Default::default()
        };
        let request = ImageSettings {
            size: Some("1536x1024".into()),
            extra: json!({"style": "b"}).as_object().cloned().unwrap(),
            ..Default::default()
        };

        let merged = request.with_defaults(&defaults);

        assert_eq!(merged.size.as_deref(), Some("1536x1024"));
        assert_eq!(merged.quality, Some(Quality::High));
        assert_eq!(
            merged.extra,
            json!({"moderation": "low", "style": "b"})
                .as_object()
                .cloned()
                .unwrap()
        );
    }

    #[test]
    fn present_lists_only_set_fields() {
        let settings = ImageSettings {
            n: Some(2),
            seed: Some(7),
            ..Default::default()
        };
        assert_eq!(settings.present(), vec![Setting::N, Setting::Seed]);
    }

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8"), Some("image/webp"));
        assert_eq!(sniff_mime(b"hello"), None);
    }
}
