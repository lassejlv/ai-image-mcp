use std::path::PathBuf;

use ai_image_mcp::app::{App, GenerateImageArgs};
use ai_image_mcp::auth::{self, Issuer};
use ai_image_mcp::config::Paths;
use ai_image_mcp::dashboard;
use ai_image_mcp::image::ImageSettings;
use ai_image_mcp::server::ImageServer;
use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

/// Image generation MCP server for xAI, ChatGPT/Codex, OpenAI, Gemini, and fal.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to config.toml. Defaults to <config dir>/ai-image-mcp/config.toml.
    #[arg(long, global = true, env = "AI_IMAGE_MCP_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the MCP server over stdio (the default).
    Serve,
    /// Connect a subscription by signing in through the browser.
    Login { account: Account },
    /// Remove a saved login.
    Logout { account: Account },
    /// Show the configured providers and whether each one is connected.
    Providers,
    /// Generate an image from the command line, to try a provider.
    Generate(Box<GenerateCommand>),
    /// Open a local web dashboard for providers, generation, and saved images.
    Dashboard {
        /// Port on 127.0.0.1 to listen on.
        #[arg(long, default_value_t = 7878)]
        port: u16,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Account {
    /// SuperGrok or X Premium+ subscription.
    #[value(alias = "grok", alias = "supergrok")]
    Xai,
    /// ChatGPT plan with Codex.
    #[value(alias = "chatgpt")]
    Codex,
}

impl Account {
    fn issuer(self) -> Issuer {
        match self {
            Self::Xai => Issuer::Xai,
            Self::Codex => Issuer::ChatGpt,
        }
    }
}

#[derive(Debug, clap::Args)]
struct GenerateCommand {
    prompt: String,
    #[arg(short, long)]
    provider: Option<String>,
    #[arg(short, long)]
    model: Option<String>,
    #[arg(short)]
    n: Option<u8>,
    #[arg(long)]
    size: Option<String>,
    #[arg(long)]
    aspect_ratio: Option<String>,
    #[arg(long)]
    resolution: Option<String>,
    /// low, medium, high, or auto.
    #[arg(long)]
    quality: Option<String>,
    /// Image to edit or use as reference; repeat for several.
    #[arg(short, long = "input")]
    inputs: Vec<String>,
    #[arg(short, long)]
    output_dir: Option<String>,
    #[arg(long)]
    filename: Option<String>,
    /// Any other settings as a JSON object, e.g. '{"seed": 7, "extra": {"moderation": "low"}}'.
    #[arg(long)]
    settings: Option<String>,
}

impl GenerateCommand {
    fn into_args(self) -> anyhow::Result<GenerateImageArgs> {
        let base: ImageSettings = match &self.settings {
            Some(json) => serde_json::from_str(json)
                .context("--settings must be a JSON object of image settings")?,
            None => ImageSettings::default(),
        };
        let quality = self
            .quality
            .map(|q| serde_json::from_value(serde_json::Value::String(q)))
            .transpose()
            .context("--quality must be low, medium, high, or auto")?;
        let settings = ImageSettings {
            n: self.n,
            size: self.size,
            aspect_ratio: self.aspect_ratio,
            resolution: self.resolution,
            quality,
            ..Default::default()
        }
        .with_defaults(&base);
        Ok(GenerateImageArgs {
            prompt: self.prompt,
            provider: self.provider,
            model: self.model,
            input_images: self.inputs,
            output_dir: self.output_dir,
            filename: self.filename,
            include_image_data: Some(false),
            settings,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let paths = Paths::from_env()?;
    let config = cli.config.as_deref();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(App::load(config, &paths).await?).await,
        Command::Login { account } => login(account, &App::load(config, &paths).await?).await,
        Command::Logout { account } => {
            let app = App::load(config, &paths).await?;
            if app.storage().login_store(account.issuer()).delete().await? {
                println!("Removed the {} login.", account.issuer().display_name());
            } else {
                println!("No saved {} login.", account.issuer().display_name());
            }
            Ok(())
        }
        Command::Providers => {
            for info in App::load(config, &paths).await?.providers().await {
                let marker = if info.default { " (default)" } else { "" };
                let state = if info.ready { "ready" } else { "off" };
                println!(
                    "{}{marker} [{}] {state}: {}",
                    info.name,
                    info.kind.id(),
                    info.auth
                );
                println!("    model: {}", info.default_model);
            }
            Ok(())
        }
        Command::Generate(command) => {
            let app = App::load(config, &paths).await?;
            let outcome = app.generate(command.into_args()?).await?;
            println!("{}", outcome.summary());
            Ok(())
        }
        Command::Dashboard { port } => {
            dashboard::serve(App::load(config, &paths).await?, port).await
        }
    }
}

async fn serve(app: App) -> anyhow::Result<()> {
    tracing::info!("serving MCP over stdio");
    let service = ImageServer::new(app)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

async fn login(account: Account, app: &App) -> anyhow::Result<()> {
    let issuer = account.issuer();
    let http = reqwest::Client::builder()
        .user_agent(concat!("ai-image-mcp/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let tokens = auth::login(issuer, &http, |prompt| {
        println!("Sign in to {}:", issuer.display_name());
        println!("  1. Open {}", prompt.verification_uri);
        println!("  2. Enter the code {}", prompt.user_code);
        println!("Waiting for approval...");
    })
    .await?;
    let store = app.storage().login_store(issuer);
    store.save(&tokens).await?;
    println!("Connected. Saved to {}", store.describe());
    Ok(())
}
