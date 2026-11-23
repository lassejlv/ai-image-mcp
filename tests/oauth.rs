//! The HTTP transport's OAuth flow end to end: discovery, registration, sign-in,
//! token exchange, refresh, and access to /mcp.

mod common;

use ai_image_mcp::oauth::{OAuthConfig, pkce_challenge};
use ai_image_mcp::remote;
use common::app;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, LOCATION, WWW_AUTHENTICATE};
use serde_json::{Value, json};

const VERIFIER: &str = "correct-horse-battery-staple-correct-horse-battery";
const REDIRECT: &str = "http://localhost:6274/oauth/callback";

async fn start(auth: Option<OAuthConfig>) -> String {
    let home = tempfile::tempdir().unwrap();
    let app = app(home.path(), "").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = remote::router(app, &base, auth).unwrap();
    tokio::spawn(async move {
        let _home = home;
        axum::serve(listener, router).await.unwrap();
    });
    base
}

fn credentials() -> OAuthConfig {
    OAuthConfig {
        username: "lasse".into(),
        password: "s3cret-pass".into(),
        secret: None,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn initialize(http: &reqwest::Client, base: &str, token: Option<&str>) -> reqwest::Response {
    let mut request = http
        .post(format!("{base}/mcp"))
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
        }));
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    request.send().await.unwrap()
}

fn query_param(location: &str, name: &str) -> Option<String> {
    reqwest::Url::parse(location)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

#[tokio::test]
async fn full_oauth_flow_grants_access_to_mcp() {
    let base = start(Some(credentials())).await;
    let http = http();

    let unauthorized = initialize(&http, &base, None).await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let challenge = unauthorized.headers()[WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        challenge,
        format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#)
    );

    let resource: Value = http
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resource["resource"], format!("{base}/mcp"));
    let metadata: Value = http
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        metadata["code_challenge_methods_supported"],
        json!(["S256"])
    );

    let registration = http
        .post(metadata["registration_endpoint"].as_str().unwrap())
        .json(&json!({"client_name": "Claude <script>", "redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap();
    assert_eq!(registration.status(), StatusCode::CREATED);
    let client_id = registration.json::<Value>().await.unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let authorize = |extra: &[(&str, &str)]| {
        let mut url = reqwest::Url::parse(&format!("{base}/oauth/authorize")).unwrap();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", REDIRECT)
            .append_pair("code_challenge", &pkce_challenge(VERIFIER))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", "xyz")
            .extend_pairs(extra);
        url
    };
    let page = http.get(authorize(&[])).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("Claude &lt;script&gt;") && !html.contains("<script>"),
        "client name is escaped"
    );

    let form = |password: &str| {
        let url = authorize(&[]);
        let mut fields: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        fields.push(("username".into(), "lasse".into()));
        fields.push(("password".into(), password.into()));
        fields
    };
    let wrong = http
        .post(format!("{base}/oauth/authorize"))
        .form(&form("nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(
        wrong
            .text()
            .await
            .unwrap()
            .contains("Wrong username or password.")
    );

    let signed_in = http
        .post(format!("{base}/oauth/authorize"))
        .form(&form("s3cret-pass"))
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    let location = signed_in.headers()[LOCATION].to_str().unwrap().to_owned();
    assert!(location.starts_with(REDIRECT), "{location}");
    assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));
    let code = query_param(&location, "code").unwrap();

    let exchange = |verifier: &'static str| {
        http.post(format!("{base}/oauth/token")).form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier),
        ])
    };
    let bad_verifier = exchange("wrong-verifier").send().await.unwrap();
    assert_eq!(bad_verifier.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        bad_verifier.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );

    let tokens: Value = exchange(VERIFIER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tokens["token_type"], "Bearer");
    let access = tokens["access_token"].as_str().unwrap();

    let reused = exchange(VERIFIER).send().await.unwrap();
    assert_eq!(
        reused.json::<Value>().await.unwrap()["error_description"],
        "authorization code was already used"
    );

    let authorized = initialize(&http, &base, Some(access)).await;
    assert_eq!(authorized.status(), StatusCode::OK);
    assert!(authorized.text().await.unwrap().contains("ai-image-mcp"));

    let forged = initialize(&http, &base, Some(&format!("{access}x"))).await;
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    assert!(
        forged.headers()[WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains(r#"error="invalid_token""#)
    );

    let refreshed: Value = http
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let access = refreshed["access_token"].as_str().unwrap();
    assert_eq!(
        initialize(&http, &base, Some(access)).await.status(),
        StatusCode::OK
    );

    let refresh_as_access = initialize(&http, &base, tokens["refresh_token"].as_str()).await;
    assert_eq!(
        refresh_as_access.status(),
        StatusCode::UNAUTHORIZED,
        "a refresh token is not an access token"
    );
}

#[tokio::test]
async fn authorization_requests_are_validated() {
    let base = start(Some(credentials())).await;
    let http = http();

    let unknown = http
        .get(format!(
            "{base}/oauth/authorize?response_type=code&client_id=forged"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    let evil = http
        .post(format!("{base}/oauth/register"))
        .json(&json!({"redirect_uris": ["http://evil.example/cb"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status(), StatusCode::BAD_REQUEST);

    let client: Value = http
        .post(format!("{base}/oauth/register"))
        .json(&json!({"redirect_uris": [REDIRECT]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = client["client_id"].as_str().unwrap();

    let other_redirect = http
        .get(
            reqwest::Url::parse_with_params(
                &format!("{base}/oauth/authorize"),
                &[
                    ("response_type", "code"),
                    ("client_id", client_id),
                    ("redirect_uri", "https://elsewhere.example/cb"),
                ],
            )
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        other_redirect.status(),
        StatusCode::BAD_REQUEST,
        "never redirect to an unregistered URI"
    );

    let no_pkce = http
        .get(
            reqwest::Url::parse_with_params(
                &format!("{base}/oauth/authorize"),
                &[
                    ("response_type", "code"),
                    ("client_id", client_id),
                    ("redirect_uri", REDIRECT),
                    ("state", "s"),
                ],
            )
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(no_pkce.status(), StatusCode::SEE_OTHER);
    let location = no_pkce.headers()[LOCATION].to_str().unwrap();
    assert_eq!(
        query_param(location, "error").as_deref(),
        Some("invalid_request")
    );
    assert_eq!(query_param(location, "state").as_deref(), Some("s"));
}

#[tokio::test]
async fn without_credentials_mcp_is_open_and_oauth_is_absent() {
    let base = start(None).await;
    let http = http();
    assert_eq!(
        initialize(&http, &base, None).await.status(),
        StatusCode::OK
    );
    let metadata = http
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::NOT_FOUND);
}
