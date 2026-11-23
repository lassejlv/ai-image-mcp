//! MCP over streamable HTTP, optionally behind the built-in OAuth server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::Router;
use axum::middleware;
use reqwest::Url;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::app::App;
use crate::oauth::{self, AuthServer, OAuthConfig};
use crate::server::ImageServer;

/// Builds the HTTP routes: `/mcp`, plus the OAuth endpoints when `auth` is set.
///
/// `public_url` is the address clients reach this server at; it names the OAuth issuer
/// and is the only non-loopback host the MCP endpoint accepts.
pub fn router(app: App, public_url: &str, auth: Option<OAuthConfig>) -> anyhow::Result<Router> {
    let url = Url::parse(public_url).with_context(|| format!("invalid public URL {public_url}"))?;
    let host = url.host_str().context("public URL has no host")?.to_owned();
    let mut allowed_hosts = vec![
        host.clone(),
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
    ];
    if let Some(port) = url.port() {
        allowed_hosts.push(format!("{host}:{port}"));
    }

    let server = ImageServer::new(app);
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
    );
    let mcp_routes = Router::new().nest_service("/mcp", mcp);

    let Some(auth) = auth else {
        return Ok(mcp_routes);
    };
    let auth = Arc::new(AuthServer::new(public_url, auth));
    Ok(mcp_routes
        .route_layer(middleware::from_fn_with_state(
            auth.clone(),
            oauth::require_bearer,
        ))
        .merge(oauth::routes(auth)))
}

/// Serves MCP over HTTP on `bind` until the process is stopped.
pub async fn serve(app: App, bind: SocketAddr, public_url: Option<String>) -> anyhow::Result<()> {
    let auth = OAuthConfig::from_env()?;
    if auth.is_none() && !bind.ip().is_loopback() {
        bail!(
            "refusing to serve MCP on {bind} without authentication; set {} and {} to require a sign-in",
            oauth::USERNAME_ENV,
            oauth::PASSWORD_ENV
        );
    }
    let public_url = public_url.unwrap_or_else(|| format!("http://{bind}"));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let secured = if auth.is_some() {
        "OAuth sign-in required"
    } else {
        "no authentication"
    };
    eprintln!(
        "MCP endpoint: {}/mcp ({secured})",
        public_url.trim_end_matches('/')
    );
    axum::serve(listener, router(app, &public_url, auth)?).await?;
    Ok(())
}
