//! The generation pipeline shared by the MCP server and the command line.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::config::{Config, OutputConfig, Paths, expand_home};
use crate::image::{GeneratedImage, ImageSettings, Setting};
use crate::media::{load_input, save_images};
use crate::providers::{ProviderInfo, Registry};

/// Arguments of the `generate_image` tool.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct GenerateImageArgs {
    /// What to create, or the change to make to the input images.
    pub prompt: String,
    /// Provider name from list_providers. Defaults to the configured default provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model id. Defaults to the provider's default model.
    #[serde(default)]
    pub model: Option<String>,
    /// Images to edit or use as references: local paths, http(s) URLs, or data URIs.
    #[serde(default)]
    pub input_images: Vec<String>,
    /// Directory to save into instead of the configured output directory.
    #[serde(default)]
    pub output_dir: Option<String>,
    /// File name for the result, without extension. Several images get -1, -2, ... suffixes.
    #[serde(default)]
    pub filename: Option<String>,
    /// Return the image data in the result as well as saving it. Defaults to the server setting.
    #[serde(default)]
    pub include_image_data: Option<bool>,
    #[serde(flatten)]
    pub settings: ImageSettings,
}

/// One saved result image.
#[derive(Debug, Clone, Serialize)]
pub struct OutputImage {
    pub path: PathBuf,
    pub mime_type: String,
    pub bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

/// The result of one `generate_image` call.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub provider: String,
    pub model: String,
    pub images: Vec<OutputImage>,
    /// Requested settings the provider does not support, which were not applied.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_settings: Vec<Setting>,
    #[serde(skip)]
    pub inline: Vec<GeneratedImage>,
}

/// Providers, output settings, and an HTTP client for fetching input images.
#[derive(Debug)]
pub struct App {
    registry: Registry,
    output: OutputConfig,
    http: reqwest::Client,
}

impl App {
    pub fn new(config: &Config, paths: &Paths) -> anyhow::Result<Self> {
        Ok(Self {
            registry: Registry::from_config(config, paths)?,
            output: config.output.clone(),
            http: reqwest::Client::builder()
                .user_agent(concat!("ai-image-mcp/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    /// Loads `config.toml` from `config_path`, or from the default location.
    pub fn load(config_path: Option<&Path>) -> anyhow::Result<Self> {
        let paths = Paths::from_env()?;
        let config_path = config_path.map_or_else(|| paths.config_file(), Path::to_path_buf);
        Self::new(&Config::load(&config_path)?, &paths)
    }

    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.registry.infos()
    }

    pub async fn generate(&self, args: GenerateImageArgs) -> anyhow::Result<Outcome> {
        let entry = self.registry.get(args.provider.as_deref())?;
        let mut inputs = Vec::with_capacity(args.input_images.len());
        for source in &args.input_images {
            inputs.push(
                load_input(&self.http, source)
                    .await
                    .with_context(|| format!("loading {source}"))?,
            );
        }

        let generation = entry
            .generate(args.prompt.clone(), args.model, args.settings, inputs)
            .await?;

        let dir = args
            .output_dir
            .as_deref()
            .map_or_else(|| self.output.dir(), |dir| expand_home(Path::new(dir)));
        let saved = save_images(
            &dir,
            args.filename.as_deref(),
            &args.prompt,
            &generation.images,
        )
        .await?;
        let images = saved
            .into_iter()
            .zip(&generation.images)
            .map(|(saved, image)| OutputImage {
                path: saved.path,
                mime_type: saved.mime_type,
                bytes: saved.bytes,
                revised_prompt: image.revised_prompt.clone(),
            })
            .collect();
        let inline = if args.include_image_data.unwrap_or(self.output.inline_images) {
            generation.images
        } else {
            Vec::new()
        };
        Ok(Outcome {
            provider: generation.provider,
            model: generation.model,
            images,
            ignored_settings: generation.ignored,
            inline,
        })
    }
}

impl Outcome {
    /// A short human-readable report of what was generated and where it was saved.
    pub fn summary(&self) -> String {
        let count = self.images.len();
        let mut text = format!(
            "Generated {count} image{} with {} ({}):",
            if count == 1 { "" } else { "s" },
            self.provider,
            self.model
        );
        for image in &self.images {
            text.push_str(&format!("\n- {}", image.path.display()));
            if let Some(revised) = &image.revised_prompt {
                text.push_str(&format!("\n  revised prompt: {revised}"));
            }
        }
        if !self.ignored_settings.is_empty() {
            let names: Vec<&str> = self.ignored_settings.iter().map(|s| s.name()).collect();
            text.push_str(&format!(
                "\nNot supported by {}, so not applied: {}",
                self.provider,
                names.join(", ")
            ));
        }
        text
    }
}
