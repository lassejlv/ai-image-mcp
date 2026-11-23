//! A built-in OAuth 2.1 authorization server for the HTTP transport, as the MCP
//! authorization spec expects: protected-resource and authorization-server metadata,
//! dynamic client registration, authorization code with PKCE, and refresh tokens.
//! People sign in with one username and password taken from the environment.

pub mod token;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use axum::extract::{Form, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tower_http::cors::CorsLayer;

use token::{Claims, Signer};

pub const USERNAME_ENV: &str = "AI_IMAGE_MCP_AUTH_USERNAME";
pub const PASSWORD_ENV: &str = "AI_IMAGE_MCP_AUTH_PASSWORD";
/// Key that signs tokens. Without it a random key is used, and tokens end with the process.
pub const SECRET_ENV: &str = "AI_IMAGE_MCP_AUTH_SECRET";

/// Where `serve --http` mounts the dashboard.
pub const DASHBOARD_PATH: &str = "/dashboard";
const SESSION_COOKIE: &str = "aim_session";
const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;
const CODE_TTL_SECS: i64 = 120;
const ACCESS_TTL_SECS: i64 = 60 * 60;
const REFRESH_TTL_SECS: i64 = 30 * 24 * 60 * 60;
/// Slows password guessing; each failed sign-in waits this long before answering.
const FAILED_LOGIN_DELAY: Duration = Duration::from_secs(1);

const LOGIN_CSS: &str = include_str!("login.css");
const LOGO_SVG: &str = include_str!("../../assets/logo.svg");

/// The credentials people sign in with.
#[derive(Clone)]
pub struct OAuthConfig {
    pub username: String,
    pub password: String,
    pub secret: Option<String>,
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl OAuthConfig {
    /// Reads the credentials from the environment. `None` when neither is set.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let read = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        match (read(USERNAME_ENV), read(PASSWORD_ENV)) {
            (None, None) => Ok(None),
            (Some(username), Some(password)) => Ok(Some(Self {
                username,
                password,
                secret: read(SECRET_ENV),
            })),
            _ => bail!("set both {USERNAME_ENV} and {PASSWORD_ENV} to enable OAuth, or neither"),
        }
    }
}

/// Issues and checks tokens for one public URL.
pub struct AuthServer {
    /// Public base URL without a trailing slash, e.g. `https://images.example.com`.
    issuer: String,
    config: OAuthConfig,
    signer: Signer,
    /// Authorization codes already redeemed, until they expire.
    used_codes: Mutex<HashMap<String, i64>>,
}

impl AuthServer {
    pub fn new(issuer: &str, config: OAuthConfig) -> Self {
        let key = match &config.secret {
            Some(secret) => secret.as_bytes().to_vec(),
            None => random_bytes(32),
        };
        Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            signer: Signer::new(&key),
            config,
            used_codes: Mutex::new(HashMap::new()),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.issuer)
    }

    fn issue_tokens(&self, client_id: &str) -> Json<serde_json::Value> {
        let now = now();
        let access = self.signer.sign(&Claims::Access {
            client_id: client_id.to_owned(),
            exp: now + ACCESS_TTL_SECS,
        });
        let refresh = self.signer.sign(&Claims::Refresh {
            client_id: client_id.to_owned(),
            exp: now + REFRESH_TTL_SECS,
        });
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
        }))
    }

    /// Checks a sign-in against the configured username and password in constant time.
    fn credentials_match(&self, username: &str, password: &str) -> bool {
        let username_ok = self.signer.same_secret(username, &self.config.username);
        let password_ok = self.signer.same_secret(password, &self.config.password);
        username_ok && password_ok
    }

    /// Marks a code as redeemed; false when it already was.
    fn redeem(&self, jti: &str, exp: i64) -> bool {
        let mut used = self.used_codes.lock().expect("code set lock poisoned");
        let now = now();
        used.retain(|_, expiry| *expiry > now);
        used.insert(jti.to_owned(), exp).is_none()
    }
}

/// The OAuth endpoints, to merge into the HTTP router.
pub fn routes(auth: Arc<AuthServer>) -> Router {
    let public = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/token", post(token))
        .layer(CorsLayer::permissive());
    Router::new()
        .route(
            "/oauth/authorize",
            get(authorize_page).post(authorize_submit),
        )
        .route(
            "/oauth/login.css",
            get(|| async { asset("text/css; charset=utf-8", LOGIN_CSS) }),
        )
        .route(
            "/favicon.svg",
            get(|| async { asset("image/svg+xml", LOGO_SVG) }),
        )
        .merge(public)
        .with_state(auth)
}

/// The dashboard's sign-in and sign-out pages, to merge into the HTTP router.
pub fn session_routes(auth: Arc<AuthServer>) -> Router {
    Router::new()
        .route(
            &format!("{DASHBOARD_PATH}/login"),
            get(session_login_page).post(session_login_submit),
        )
        .route(&format!("{DASHBOARD_PATH}/logout"), post(session_logout))
        .with_state(auth)
}

/// Middleware for the dashboard: requires a signed-in session cookie. Pages redirect to
/// the sign-in; API calls get 401.
pub async fn require_session(
    State(auth): State<Arc<AuthServer>>,
    request: Request,
    next: Next,
) -> Response {
    let prefix = format!("{SESSION_COOKIE}=");
    let signed_in = request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().strip_prefix(prefix.as_str()))
        .filter_map(|token| auth.signer.verify(token))
        .any(|claims| matches!(claims, Claims::Session { exp } if exp > now()));
    if signed_in {
        return next.run(request).await;
    }
    if request.uri().path().contains("/api/") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Redirect::to(&format!("{DASHBOARD_PATH}/login")).into_response()
}

#[derive(Debug, Deserialize)]
struct SessionForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

async fn session_login_page() -> Response {
    session_page(None, StatusCode::OK)
}

async fn session_login_submit(State(auth): Auth, Form(form): Form<SessionForm>) -> Response {
    if !auth.credentials_match(&form.username, &form.password) {
        tokio::time::sleep(FAILED_LOGIN_DELAY).await;
        return session_page(
            Some("Wrong username or password."),
            StatusCode::UNAUTHORIZED,
        );
    }
    let token = auth.signer.sign(&Claims::Session {
        exp: now() + SESSION_TTL_SECS,
    });
    let secure = if auth.issuer.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path={DASHBOARD_PATH}; Max-Age={SESSION_TTL_SECS}; HttpOnly; SameSite=Strict{secure}"
    );
    (
        [(header::SET_COOKIE, cookie)],
        Redirect::to(&format!("{DASHBOARD_PATH}/")),
    )
        .into_response()
}

async fn session_logout() -> Response {
    let cookie =
        format!("{SESSION_COOKIE}=; Path={DASHBOARD_PATH}; Max-Age=0; HttpOnly; SameSite=Strict");
    (
        [(header::SET_COOKIE, cookie)],
        Redirect::to(&format!("{DASHBOARD_PATH}/login")),
    )
        .into_response()
}

fn session_page(error: Option<&str>, status: StatusCode) -> Response {
    let error = error.map_or_else(String::new, |e| {
        format!(r#"<p class="error" role="alert">{}</p>"#, escape(e))
    });
    page(
        status,
        &format!(
            r#"<h1>Sign in to the dashboard</h1>
<p class="lede">Use the username and password this server was started with.</p>
<form method="post" action="{DASHBOARD_PATH}/login">
<label>Username<input name="username" autocomplete="username" autofocus required></label>
<label>Password<input name="password" type="password" autocomplete="current-password" required></label>
{error}
<button type="submit">Sign in</button>
</form>"#
        ),
    )
}

/// Middleware for the MCP endpoint: requires a valid access token.
pub async fn require_bearer(
    State(auth): State<Arc<AuthServer>>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let valid = token
        .and_then(|t| auth.signer.verify(t.trim()))
        .is_some_and(|claims| matches!(claims, Claims::Access { exp, .. } if exp > now()));
    if valid {
        return next.run(request).await;
    }
    let mut challenge = format!(
        r#"Bearer resource_metadata="{}""#,
        auth.url("/.well-known/oauth-protected-resource")
    );
    if token.is_some() {
        challenge.push_str(r#", error="invalid_token""#);
    }
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

type Auth = State<Arc<AuthServer>>;

async fn protected_resource(State(auth): Auth) -> Json<serde_json::Value> {
    Json(json!({
        "resource": auth.url("/mcp"),
        "authorization_servers": [auth.issuer],
        "bearer_methods_supported": ["header"],
    }))
}

async fn authorization_server(State(auth): Auth) -> Json<serde_json::Value> {
    Json(json!({
        "issuer": auth.issuer,
        "authorization_endpoint": auth.url("/oauth/authorize"),
        "token_endpoint": auth.url("/oauth/token"),
        "registration_endpoint": auth.url("/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

#[derive(Debug, Deserialize)]
struct RegistrationRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

/// An OAuth error response body (RFC 6749 section 5.2).
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

/// Dynamic client registration (RFC 7591). The client id is the signed registration,
/// so nothing is stored.
async fn register(State(auth): Auth, Json(request): Json<RegistrationRequest>) -> Response {
    if request.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "at least one redirect_uri is required",
        );
    }
    if let Some(bad) = request
        .redirect_uris
        .iter()
        .find(|uri| !is_allowed_redirect(uri))
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("redirect_uri {bad} is not allowed"),
        );
    }
    let client_id = auth.signer.sign(&Claims::Client {
        name: request.client_name.clone(),
        redirect_uris: request.redirect_uris.clone(),
    });
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "client_id_issued_at": now(),
            "client_name": request.client_name,
            "redirect_uris": request.redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

/// Redirect targets must be https, a loopback http address, or an app's own scheme.
fn is_allowed_redirect(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        "javascript" | "data" | "file" | "about" | "blob" | "vbscript" => false,
        _ => url.fragment().is_none(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    #[serde(flatten)]
    params: AuthorizeParams,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// A validated authorization request.
struct Authorization {
    client_id: String,
    client_name: Option<String>,
    redirect_uri: String,
    code_challenge: String,
}

/// How an invalid authorization request is answered: on our own page when the redirect
/// target is unknown, otherwise by redirecting the error back to the client.
enum AuthorizeError {
    Page(String),
    Redirect {
        redirect_uri: String,
        error: &'static str,
        description: &'static str,
        state: Option<String>,
    },
}

impl AuthServer {
    fn check(&self, params: &AuthorizeParams) -> Result<Authorization, AuthorizeError> {
        let Some(Claims::Client {
            name,
            redirect_uris,
        }) = self.signer.verify(&params.client_id)
        else {
            return Err(AuthorizeError::Page(
                "This app is not registered with this server. Reconnect it from your MCP client."
                    .into(),
            ));
        };
        let redirect_uri = match (&params.redirect_uri, redirect_uris.as_slice()) {
            (Some(uri), registered) if registered.contains(uri) => uri.clone(),
            (None, [only]) => only.clone(),
            _ => {
                return Err(AuthorizeError::Page(
                    "The redirect address does not match the one this app registered.".into(),
                ));
            }
        };
        let fail = |error, description| AuthorizeError::Redirect {
            redirect_uri: redirect_uri.clone(),
            error,
            description,
            state: params.state.clone(),
        };
        if params.response_type != "code" {
            return Err(fail(
                "unsupported_response_type",
                "only response_type=code is supported",
            ));
        }
        let challenge = params.code_challenge.clone().filter(|c| !c.is_empty());
        let Some(code_challenge) =
            challenge.filter(|_| params.code_challenge_method.as_deref() == Some("S256"))
        else {
            return Err(fail(
                "invalid_request",
                "PKCE with code_challenge_method=S256 is required",
            ));
        };
        Ok(Authorization {
            client_id: params.client_id.clone(),
            client_name: name,
            redirect_uri,
            code_challenge,
        })
    }
}

fn redirect_with(uri: &str, pairs: &[(&str, &str)], state: Option<&str>) -> Response {
    let Ok(mut url) = Url::parse(uri) else {
        return (StatusCode::BAD_REQUEST, "invalid redirect_uri").into_response();
    };
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(key, value);
        }
        if let Some(state) = state {
            query.append_pair("state", state);
        }
    }
    Redirect::to(url.as_str()).into_response()
}

async fn authorize_page(
    State(auth): Auth,
    axum::extract::Query(params): axum::extract::Query<AuthorizeParams>,
) -> Response {
    match auth.check(&params) {
        Ok(request) => login_page(&params, &request, None, StatusCode::OK),
        Err(error) => authorize_error(error),
    }
}

async fn authorize_submit(State(auth): Auth, Form(form): Form<LoginForm>) -> Response {
    let request = match auth.check(&form.params) {
        Ok(request) => request,
        Err(error) => return authorize_error(error),
    };
    if !auth.credentials_match(&form.username, &form.password) {
        tokio::time::sleep(FAILED_LOGIN_DELAY).await;
        return login_page(
            &form.params,
            &request,
            Some("Wrong username or password."),
            StatusCode::UNAUTHORIZED,
        );
    }
    let code = auth.signer.sign(&Claims::Code {
        jti: URL_SAFE_NO_PAD.encode(random_bytes(16)),
        client_id: request.client_id,
        redirect_uri: request.redirect_uri.clone(),
        code_challenge: request.code_challenge,
        exp: now() + CODE_TTL_SECS,
    });
    redirect_with(
        &request.redirect_uri,
        &[("code", &code)],
        form.params.state.as_deref(),
    )
}

fn authorize_error(error: AuthorizeError) -> Response {
    match error {
        AuthorizeError::Redirect {
            redirect_uri,
            error,
            description,
            state,
        } => redirect_with(
            &redirect_uri,
            &[("error", error), ("error_description", description)],
            state.as_deref(),
        ),
        AuthorizeError::Page(message) => page(
            StatusCode::BAD_REQUEST,
            &format!(
                r#"<h1>Can't sign in</h1><p class="lede">{}</p>"#,
                escape(&message)
            ),
        ),
    }
}

fn login_page(
    params: &AuthorizeParams,
    request: &Authorization,
    error: Option<&str>,
    status: StatusCode,
) -> Response {
    let hidden: String = [
        ("response_type", Some(params.response_type.as_str())),
        ("client_id", Some(params.client_id.as_str())),
        ("redirect_uri", Some(request.redirect_uri.as_str())),
        ("code_challenge", params.code_challenge.as_deref()),
        (
            "code_challenge_method",
            params.code_challenge_method.as_deref(),
        ),
        ("state", params.state.as_deref()),
        ("scope", params.scope.as_deref()),
        ("resource", params.resource.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value.map(|v| {
            format!(
                r#"<input type="hidden" name="{name}" value="{}">"#,
                escape(v)
            )
        })
    })
    .collect();
    let client = request.client_name.as_deref().unwrap_or("An MCP client");
    let target = Url::parse(&request.redirect_uri)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(str::to_owned)
                .or_else(|| Some(format!("{}:", url.scheme())))
        })
        .unwrap_or_default();
    let error = error.map_or_else(String::new, |e| {
        format!(r#"<p class="error" role="alert">{}</p>"#, escape(e))
    });
    page(
        status,
        &format!(
            r#"<h1>Sign in to ai-image-mcp</h1>
<p class="lede"><strong>{client}</strong> wants to generate images with your providers. You'll return to <span class="mono">{target}</span>.</p>
<form method="post" action="/oauth/authorize">
{hidden}
<label>Username<input name="username" autocomplete="username" autofocus required></label>
<label>Password<input name="password" type="password" autocomplete="current-password" required></label>
{error}
<button type="submit">Sign in</button>
</form>"#,
            client = escape(client),
            target = escape(&target),
        ),
    )
}

fn page(status: StatusCode, body: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Html(format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in · ai-image-mcp</title>
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/oauth/login.css">
</head>
<body><main><img class="logo" src="/favicon.svg" alt="">{body}</main></body>
</html>"#
        )),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn token(State(auth): Auth, Form(request): Form<TokenRequest>) -> Response {
    let invalid_grant =
        |description: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", description);
    match request.grant_type.as_str() {
        "authorization_code" => {
            let Some(Claims::Code {
                jti,
                client_id,
                redirect_uri,
                code_challenge,
                exp,
            }) = request.code.as_deref().and_then(|c| auth.signer.verify(c))
            else {
                return invalid_grant("unknown authorization code");
            };
            if exp <= now() {
                return invalid_grant("authorization code expired");
            }
            if request
                .client_id
                .as_deref()
                .is_some_and(|id| id != client_id)
            {
                return invalid_grant("authorization code was issued to another client");
            }
            if request
                .redirect_uri
                .as_deref()
                .is_some_and(|uri| uri != redirect_uri)
            {
                return invalid_grant("redirect_uri does not match the authorization request");
            }
            let Some(verifier) = request.code_verifier.as_deref() else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "code_verifier is required",
                );
            };
            if pkce_challenge(verifier) != code_challenge {
                return invalid_grant("code_verifier does not match the code_challenge");
            }
            if !auth.redeem(&jti, exp) {
                return invalid_grant("authorization code was already used");
            }
            with_no_store(auth.issue_tokens(&client_id))
        }
        "refresh_token" => {
            let Some(Claims::Refresh { client_id, exp }) = request
                .refresh_token
                .as_deref()
                .and_then(|t| auth.signer.verify(t))
            else {
                return invalid_grant("unknown refresh token");
            };
            if exp <= now() {
                return invalid_grant("refresh token expired; sign in again");
            }
            if request
                .client_id
                .as_deref()
                .is_some_and(|id| id != client_id)
            {
                return invalid_grant("refresh token was issued to another client");
            }
            with_no_store(auth.issue_tokens(&client_id))
        }
        other => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("grant_type {other} is not supported"),
        ),
    }
}

fn with_no_store(body: Json<serde_json::Value>) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    rand::fill(&mut bytes[..]);
    bytes
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// The S256 PKCE challenge for `verifier`.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_must_be_safe() {
        assert!(is_allowed_redirect(
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(is_allowed_redirect("http://localhost:6274/oauth/callback"));
        assert!(is_allowed_redirect("http://127.0.0.1:33418/callback"));
        assert!(is_allowed_redirect(
            "cursor://anysphere.cursor-retrieval/oauth/callback"
        ));
        assert!(!is_allowed_redirect("http://evil.example/callback"));
        assert!(!is_allowed_redirect("javascript:alert(1)"));
        assert!(!is_allowed_redirect("not a url"));
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }
}
