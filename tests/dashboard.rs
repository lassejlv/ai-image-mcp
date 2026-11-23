//! The dashboard's HTTP API against a mock provider.

mod common;

use ai_image_mcp::dashboard;
use common::{PNG, app, png_b64};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn dashboard_generates_lists_and_serves_images() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/images/generations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"b64_json": png_b64()}]})),
        )
        .mount(&api)
        .await;
    let home = tempfile::tempdir().unwrap();
    let out = home.path().join("out");
    let app = app(
        home.path(),
        &format!(
            "[output]\ndir = \"{}\"\n[providers.openai]\ntype = \"openai\"\nbase_url = \"{}\"\napi_key = \"k\"",
            out.display(),
            api.uri()
        ),
    ).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = dashboard::router(app);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::new();

    let index = http.get(&base).send().await.unwrap().text().await.unwrap();
    assert!(index.contains("<title>ai-image-mcp</title>"));

    let providers: Value = http
        .get(format!("{base}/api/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(providers["providers"][0]["name"], "openai");
    assert_eq!(providers["providers"][0]["auth_method"], "api_key");

    let generated: Value = http
        .post(format!("{base}/api/generate"))
        .json(&json!({"prompt": "a red fox", "filename": "fox", "output_dir": "/tmp/ignored"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(generated["urls"], json!(["/images/fox.png"]), "{generated}");
    assert!(
        out.join("fox.png").is_file(),
        "output_dir from the browser must be ignored"
    );

    let image = http
        .get(format!("{base}/images/fox.png"))
        .send()
        .await
        .unwrap();
    assert_eq!(image.headers()["content-type"], "image/png");
    assert_eq!(image.bytes().await.unwrap().as_ref(), PNG);

    let gallery: Value = http
        .get(format!("{base}/api/images"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(gallery[0]["name"], "fox.png");

    let traversal = http
        .get(format!("{base}/images/..%2Fconfig.png"))
        .send()
        .await
        .unwrap();
    assert_eq!(traversal.status(), 404);

    let rebinding = http
        .get(&base)
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rebinding.status(), 403);

    let unknown = http
        .post(format!("{base}/api/login/bogus"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400);
    let idle: Value = http
        .get(format!("{base}/api/login/xai"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(idle["state"], "idle");

    let failed = http
        .post(format!("{base}/api/generate"))
        .json(&json!({"prompt": "x", "provider": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 500);
    let body: Value = failed.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("unknown provider"));
}
