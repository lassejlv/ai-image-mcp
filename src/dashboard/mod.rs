//! A small local web dashboard: provider status, browser logins, a generate form, and a
//! gallery of saved images. Served on loopback only.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Context;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tokio::sync::oneshot;

use crate::app::{App, GenerateImageArgs, Outcome};
use crate::auth::{self, DevicePrompt, Issuer};
use crate::config::Paths;
use crate::image::extension_for;

/// Request bodies carry input images as data URIs, so allow well beyond axum's 2 MB default.
const BODY_LIMIT: usize = 64 * 1024 * 1024;
/// Most recent images listed in the gallery.
const GALLERY_LIMIT: usize = 120;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_CSS: &str = include_str!("assets/app.css");
const APP_JS: &str = include_str!("assets/app.js");

struct Dashboard {
    app: App,
    paths: Paths,
    http: reqwest::Client,
    logins: Mutex<HashMap<&'static str, LoginState>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LoginState {
    Pending { prompt: DevicePrompt },
    Connected,
    Failed { error: String },
}

/// Serves the dashboard on `127.0.0.1:<port>` until the process is stopped.
pub async fn serve(app: App, paths: Paths, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let addr = listener.local_addr()?;
    println!("Dashboard running at http://{addr}");
    axum::serve(listener, router(app, paths)).await?;
    Ok(())
}

/// The dashboard's routes, exposed for tests.
pub fn router(app: App, paths: Paths) -> Router {
    let state = Arc::new(Dashboard {
        app,
        paths,
        http: reqwest::Client::new(),
        logins: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route("/api/providers", get(providers))
        .route("/api/generate", post(generate))
        .route("/api/images", get(images))
        .route(
            "/api/login/{account}",
            get(login_status).post(start_login).delete(logout),
        )
        .route("/images/{name}", get(image_file))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn(loopback_host_only))
        .with_state(state)
}

/// Rejects requests whose Host is not loopback, so a web page cannot reach the dashboard
/// through DNS rebinding.
async fn loopback_host_only(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let hostname = host.rsplit_once(':').map_or(host, |(name, _)| name);
    if matches!(hostname, "127.0.0.1" | "localhost" | "[::1]") {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden host").into_response()
    }
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// An error returned to the browser as `{"error": "..."}`.
struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type Shared = State<Arc<Dashboard>>;

async fn providers(State(dash): Shared) -> Json<serde_json::Value> {
    Json(json!({
        "providers": dash.app.providers(),
        "output_dir": dash.app.output_dir(),
    }))
}

/// A saved image as the browser sees it.
#[derive(Debug, Serialize)]
struct GalleryImage {
    name: String,
    url: String,
    bytes: u64,
    /// Unix seconds.
    modified: u64,
}

#[derive(Serialize)]
struct GenerateResponse {
    #[serde(flatten)]
    outcome: Outcome,
    urls: Vec<String>,
}

async fn generate(
    State(dash): Shared,
    Json(mut args): Json<GenerateImageArgs>,
) -> Result<Json<GenerateResponse>, ApiError> {
    args.output_dir = None;
    args.include_image_data = Some(false);
    let outcome = dash.app.generate(args).await?;
    let urls = outcome
        .images
        .iter()
        .filter_map(|image| image.path.file_name())
        .map(|name| image_url(&name.to_string_lossy()))
        .collect();
    Ok(Json(GenerateResponse { outcome, urls }))
}

async fn images(State(dash): Shared) -> Result<Json<Vec<GalleryImage>>, ApiError> {
    let dir = dash.app.output_dir();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Json(Vec::new())),
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(format!("reading {}", dir.display()))
                .into());
        }
    };
    let mut images = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(anyhow::Error::from)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = entry.metadata().await.map_err(anyhow::Error::from)?;
        if !metadata.is_file() || image_mime(&name).is_none() {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        images.push(GalleryImage {
            url: image_url(&name),
            name,
            bytes: metadata.len(),
            modified,
        });
    }
    images.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.name.cmp(&a.name))
    });
    images.truncate(GALLERY_LIMIT);
    Ok(Json(images))
}

async fn image_file(State(dash): Shared, Path(name): Path<String>) -> Result<Response, ApiError> {
    let mime = image_mime(&name)
        .filter(|_| !name.contains(['/', '\\']) && !name.starts_with('.'))
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "not found".into()))?;
    let path: PathBuf = dash.app.output_dir().join(&name);
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "not found".into()))?;
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(mime))],
        data,
    )
        .into_response())
}

fn image_url(name: &str) -> String {
    format!("/images/{name}")
}

/// The MIME type for a file name with an image extension this server writes.
fn image_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    ["image/png", "image/jpeg", "image/webp", "image/gif"]
        .into_iter()
        .find(|mime| extension_for(mime) == ext || (ext == "jpeg" && *mime == "image/jpeg"))
}

fn issuer(account: &str) -> Result<Issuer, ApiError> {
    match account {
        "xai" => Ok(Issuer::Xai),
        "codex" => Ok(Issuer::ChatGpt),
        other => Err(ApiError::bad_request(format!(
            "unknown account \"{other}\"; use xai or codex"
        ))),
    }
}

async fn login_status(
    State(dash): Shared,
    Path(account): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let issuer = issuer(&account)?;
    let state = dash
        .logins
        .lock()
        .expect("login state lock poisoned")
        .get(issuer.id())
        .cloned();
    Ok(Json(
        state.map_or_else(|| json!({ "state": "idle" }), |s| json!(s)),
    ))
}

/// Starts a device-code login in the background and returns the code to show.
/// A login already waiting for approval returns its existing code.
async fn start_login(
    State(dash): Shared,
    Path(account): Path<String>,
) -> Result<Json<DevicePrompt>, ApiError> {
    let issuer = issuer(&account)?;
    if let Some(LoginState::Pending { prompt }) = dash
        .logins
        .lock()
        .expect("login state lock poisoned")
        .get(issuer.id())
    {
        return Ok(Json(prompt.clone()));
    }

    let (prompt_tx, prompt_rx) = oneshot::channel();
    let task = dash.clone();
    tokio::spawn(async move {
        let on_prompt = |prompt: &DevicePrompt| {
            task.logins
                .lock()
                .expect("login state lock poisoned")
                .insert(
                    issuer.id(),
                    LoginState::Pending {
                        prompt: prompt.clone(),
                    },
                );
            let _ = prompt_tx.send(prompt.clone());
        };
        let next =
            match auth::login(issuer, &task.http, task.paths.token_file(issuer), on_prompt).await {
                Ok(_) => LoginState::Connected,
                Err(error) => LoginState::Failed {
                    error: format!("{error:#}"),
                },
            };
        task.logins
            .lock()
            .expect("login state lock poisoned")
            .insert(issuer.id(), next);
    });

    match prompt_rx.await {
        Ok(prompt) => Ok(Json(prompt)),
        Err(_) => {
            let state = dash
                .logins
                .lock()
                .expect("login state lock poisoned")
                .get(issuer.id())
                .cloned();
            let message = match state {
                Some(LoginState::Failed { error }) => error,
                _ => "the login could not be started".to_owned(),
            };
            Err(ApiError(StatusCode::BAD_GATEWAY, message))
        }
    }
}

async fn logout(
    State(dash): Shared,
    Path(account): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let issuer = issuer(&account)?;
    let removed = auth::logout(&dash.paths.token_file(issuer))?;
    dash.logins
        .lock()
        .expect("login state lock poisoned")
        .remove(issuer.id());
    Ok(Json(json!({ "removed": removed })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_written_image_types_are_served() {
        assert_eq!(image_mime("a.png"), Some("image/png"));
        assert_eq!(image_mime("a.JPG"), Some("image/jpeg"));
        assert_eq!(image_mime("a.jpeg"), Some("image/jpeg"));
        assert_eq!(image_mime("a.webp"), Some("image/webp"));
        assert_eq!(image_mime("config.toml"), None);
        assert_eq!(image_mime("png"), None);
    }
}
