//! A small local web dashboard: provider status, browser logins, a generate form, and a
//! gallery of saved images. Served on loopback only.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tokio::sync::oneshot;

use crate::app::{App, GenerateImageArgs, Outcome};
use crate::auth::{self, DevicePrompt, Issuer};

/// Request bodies carry input images as data URIs, so allow well beyond axum's 2 MB default.
const BODY_LIMIT: usize = 64 * 1024 * 1024;
/// Most recent images listed in the gallery.
const GALLERY_LIMIT: usize = 120;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_CSS: &str = include_str!("assets/app.css");
const APP_JS: &str = include_str!("assets/app.js");
const LOGO_SVG: &str = include_str!("../../assets/logo.svg");

struct Dashboard {
    app: App,
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
pub async fn serve(app: App, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let addr = listener.local_addr()?;
    println!("Dashboard running at http://{addr}");
    axum::serve(listener, router(app)).await?;
    Ok(())
}

/// The dashboard's routes, exposed for tests.
pub fn router(app: App) -> Router {
    let state = Arc::new(Dashboard {
        app,
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
        .route(
            "/favicon.svg",
            get(|| async { asset("image/svg+xml", LOGO_SVG) }),
        )
        .route("/api/providers", get(providers))
        .route("/api/generate", post(generate))
        .route("/api/images", get(images))
        .route(
            "/api/login/{account}",
            get(login_status).post(start_login).delete(logout),
        )
        .route("/images/{key}", get(image_file))
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
        "providers": dash.app.providers().await,
        "storage": dash.app.storage().describe(),
    }))
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
        .map(|image| image_url(&image.location.key()))
        .collect();
    Ok(Json(GenerateResponse { outcome, urls }))
}

/// A saved image as the browser sees it.
#[derive(Serialize)]
struct GalleryImage {
    name: String,
    url: String,
    bytes: u64,
    created: i64,
}

async fn images(State(dash): Shared) -> Result<Json<Vec<GalleryImage>>, ApiError> {
    let images = dash.app.storage().list(GALLERY_LIMIT).await?;
    Ok(Json(
        images
            .into_iter()
            .map(|image| GalleryImage {
                url: image_url(&image.key),
                name: image.name,
                bytes: image.bytes,
                created: image.created,
            })
            .collect(),
    ))
}

async fn image_file(State(dash): Shared, Path(key): Path<String>) -> Result<Response, ApiError> {
    let (mime, data) = dash
        .app
        .storage()
        .read(&key)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "not found".into()))?;
    Ok(([(header::CONTENT_TYPE, mime)], data).into_response())
}

fn image_url(key: &str) -> String {
    format!("/images/{key}")
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
        let result = match auth::login(issuer, &task.http, on_prompt).await {
            Ok(tokens) => task.app.storage().login_store(issuer).save(&tokens).await,
            Err(error) => Err(error),
        };
        let next = match result {
            Ok(()) => LoginState::Connected,
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
    let removed = dash.app.storage().login_store(issuer).delete().await?;
    dash.logins
        .lock()
        .expect("login state lock poisoned")
        .remove(issuer.id());
    Ok(Json(json!({ "removed": removed })))
}
