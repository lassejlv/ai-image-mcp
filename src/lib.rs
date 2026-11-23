//! MCP server that generates images through subscription logins (SuperGrok, ChatGPT/Codex)
//! and API-key providers (OpenAI and compatible services, Gemini, fal).

pub mod app;
pub mod auth;
pub mod config;
pub mod dashboard;
pub mod image;
pub mod media;
pub mod providers;
pub mod server;

mod http;
