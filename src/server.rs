//! The MCP server: `generate_image` and `list_providers` tools over the official rmcp SDK.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Icon, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use serde_json::json;

use crate::app::{App, GenerateImageArgs};

/// The app icon, advertised to clients that show server icons.
const LOGO_SVG: &str = include_str!("../assets/logo.svg");

const INSTRUCTIONS: &str = "Generate and edit images with generate_image. \
Call list_providers first to see which providers are connected, their models, and which settings each one supports. \
Pass input_images (paths, URLs, or data URIs) to edit images or use them as references. \
Results are saved to disk and the file paths are returned.";

#[derive(Clone)]
pub struct ImageServer {
    app: Arc<App>,
}

#[tool_router]
impl ImageServer {
    pub fn new(app: impl Into<Arc<App>>) -> Self {
        Self { app: app.into() }
    }

    #[tool(
        description = "Generate or edit images with a configured provider (xAI Grok Imagine, ChatGPT/Codex GPT Image, OpenAI, Gemini, fal, or an OpenAI-compatible service). Saves the images to disk and returns their paths, plus the image data unless disabled. Settings a provider does not support are reported back, not silently applied."
    )]
    async fn generate_image(
        &self,
        Parameters(args): Parameters<GenerateImageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let outcome = match self.app.generate(args).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{error:#}"
                ))]));
            }
        };
        let mut content = vec![ContentBlock::text(outcome.summary())];
        content.extend(
            outcome.inline.iter().map(|image| {
                ContentBlock::image(BASE64.encode(&image.data), image.mime_type.clone())
            }),
        );
        let mut result = CallToolResult::success(content);
        result.structured_content = Some(json!(outcome));
        Ok(result)
    }

    #[tool(
        description = "List the configured image providers: whether each is connected, its default model, the image models it offers right now (fetched from the provider's API), the settings it supports, and how many input images it accepts for edits."
    )]
    async fn list_providers(&self) -> Result<CallToolResult, McpError> {
        let providers = json!({ "providers": self.app.providers().await });
        let text = serde_json::to_string_pretty(&providers)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content = Some(providers);
        Ok(result)
    }
}

#[tool_handler]
impl ServerHandler for ImageServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")).with_icons(
                    vec![
                        Icon::new(format!(
                            "data:image/svg+xml;base64,{}",
                            BASE64.encode(LOGO_SVG)
                        ))
                        .with_mime_type("image/svg+xml")
                        .with_sizes(vec!["any".to_owned()]),
                    ],
                ),
            )
            .with_instructions(INSTRUCTIONS)
    }
}
