//! Each provider against a mock HTTP server: request shape, auth headers, and saved output.

mod common;

use ai_image_mcp::app::GenerateImageArgs;
use ai_image_mcp::image::{ImageSettings, Quality, Setting};
use common::{PNG, app, jwt, png_b64, save_login};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, header_regex, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn args(prompt: &str, provider: &str, out: &std::path::Path) -> GenerateImageArgs {
    GenerateImageArgs {
        prompt: prompt.into(),
        provider: Some(provider.into()),
        output_dir: Some(out.to_string_lossy().into_owned()),
        ..Default::default()
    }
}

fn images_response(count: usize) -> ResponseTemplate {
    let data: Vec<Value> = (0..count).map(|_| json!({"b64_json": png_b64()})).collect();
    ResponseTemplate::new(200).set_body_json(json!({"created": 1, "data": data}))
}

#[tokio::test]
async fn openai_generates_saves_and_reports_ignored_settings() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(json!({"model": "gpt-image-2", "prompt": "a fox", "n": 2, "quality": "high", "moderation": "low"})))
        .respond_with(images_response(2))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let app = app(
        home.path(),
        &format!(
            "[providers.openai]\ntype = \"openai\"\nbase_url = \"{}/v1\"\napi_key = \"sk-test\"\ndefaults = {{ quality = \"high\", extra = {{ moderation = \"low\" }} }}",
            server.uri()
        ),
    );

    let mut request = args("a fox", "openai", home.path());
    request.settings = ImageSettings {
        n: Some(2),
        aspect_ratio: Some("16:9".into()),
        ..Default::default()
    };
    let outcome = app.generate(request).await.unwrap();

    assert_eq!(outcome.images.len(), 2);
    assert_eq!(outcome.ignored_settings, vec![Setting::AspectRatio]);
    for image in &outcome.images {
        assert_eq!(std::fs::read(&image.path).unwrap(), PNG);
        assert_eq!(image.mime_type, "image/png");
    }
    assert!(
        outcome
            .summary()
            .contains("Not supported by openai, so not applied: aspect_ratio")
    );
}

#[tokio::test]
async fn openai_compatible_edits_upload_multipart_and_download_urls() {
    let server = MockServer::start().await;
    let image_url = format!("{}/files/out.png", server.uri());
    Mock::given(method("POST"))
        .and(path("/v1/images/edits"))
        .and(header_regex("content-type", "^multipart/form-data"))
        .respond_with(move |request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            assert!(
                body.contains("name=\"image[]\"; filename=\"image-1.png\""),
                "{body}"
            );
            assert!(body.contains("name=\"prompt\""), "{body}");
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"url": image_url}]}))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/out.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PNG)
                .insert_header("content-type", "image/png"),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let input = home.path().join("input.png");
    std::fs::write(&input, PNG).unwrap();
    let app = app(
        home.path(),
        &format!(
            "[providers.together]\ntype = \"openai\"\nbase_url = \"{}/v1\"\napi_key = \"k\"\nmodel = \"flux-kontext\"",
            server.uri()
        ),
    );

    let mut request = args("make it blue", "together", home.path());
    request.input_images = vec![input.to_string_lossy().into_owned()];
    request.filename = Some("blue".into());
    let outcome = app.generate(request).await.unwrap();

    assert_eq!(outcome.model, "flux-kontext");
    assert_eq!(outcome.images[0].path, home.path().join("blue.png"));
}

#[tokio::test]
async fn xai_uses_the_supergrok_login_before_an_api_key() {
    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();
    let token = jwt(json!({"sub": "user"}));
    save_login(home.path(), "xai", &token);
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(body_partial_json(json!({
            "model": "grok-imagine-image-2.0",
            "aspect_ratio": "9:16",
            "resolution": "2k",
            "response_format": "b64_json"
        })))
        .respond_with(images_response(1))
        .expect(1)
        .mount(&server)
        .await;
    let app = app(
        home.path(),
        &format!(
            "[providers.grok]\ntype = \"xai\"\nbase_url = \"{}/v1\"\napi_key = \"unused\"",
            server.uri()
        ),
    );

    let info = &app.providers()[0];
    assert!(info.ready);
    assert!(info.auth.contains("SuperGrok"), "{}", info.auth);

    let mut request = args("a tall lighthouse", "grok", home.path());
    request.settings = ImageSettings {
        aspect_ratio: Some("9:16".into()),
        resolution: Some("2K".into()),
        ..Default::default()
    };
    app.generate(request).await.unwrap();
}

#[tokio::test]
async fn codex_sends_the_chatgpt_account_and_edits_with_data_urls() {
    let server = MockServer::start().await;
    let home = tempfile::tempdir().unwrap();
    let token = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_42"}}));
    save_login(home.path(), "chatgpt", &token);
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/images/edits"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(header("chatgpt-account-id", "acct_42"))
        .and(header("originator", "codex_cli_rs"))
        .and(body_partial_json(json!({
            "model": "gpt-image-2",
            "quality": "high",
            "images": [{"image_url": format!("data:image/png;base64,{}", png_b64())}]
        })))
        .respond_with(images_response(1))
        .expect(1)
        .mount(&server)
        .await;
    let app = app(
        home.path(),
        &format!(
            "[providers.chatgpt]\ntype = \"codex\"\nbase_url = \"{}/backend-api/codex\"",
            server.uri()
        ),
    );

    let mut request = args("add a red hat", "chatgpt", home.path());
    request.input_images = vec![format!("data:image/png;base64,{}", png_b64())];
    request.settings.quality = Some(Quality::High);
    app.generate(request).await.unwrap();
}

#[tokio::test]
async fn codex_falls_back_to_the_codex_cli_login() {
    let home = tempfile::tempdir().unwrap();
    let codex_home = home.path().join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("auth.json"),
        json!({"tokens": {"access_token": jwt(json!({})), "refresh_token": "r"}}).to_string(),
    )
    .unwrap();
    let app = app(
        home.path(),
        &format!(
            "[providers.chatgpt]\ntype = \"codex\"\ncodex_home = \"{}\"",
            codex_home.display()
        ),
    );
    let info = &app.providers()[0];
    assert!(info.ready);
    assert!(info.auth.contains("Codex CLI login"), "{}", info.auth);
}

#[tokio::test]
async fn gemini_makes_one_call_per_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-3-pro-image:generateContent"))
        .and(header("x-goog-api-key", "g-key"))
        .and(body_partial_json(json!({
            "generationConfig": {"responseModalities": ["IMAGE"], "imageConfig": {"aspectRatio": "4:3", "imageSize": "4K"}}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{"content": {"parts": [
                {"text": "Here you go"},
                {"inlineData": {"mimeType": "image/png", "data": png_b64()}}
            ]}, "finishReason": "STOP"}]
        })))
        .expect(2)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let app = app(
        home.path(),
        &format!(
            "[providers.gemini]\ntype = \"gemini\"\nbase_url = \"{}/v1beta\"\napi_key = \"g-key\"",
            server.uri()
        ),
    );

    let mut request = args("a banana", "gemini", home.path());
    request.model = Some("gemini-3-pro-image".into());
    request.settings = ImageSettings {
        n: Some(2),
        aspect_ratio: Some("4:3".into()),
        resolution: Some("4k".into()),
        ..Default::default()
    };
    let outcome = app.generate(request).await.unwrap();
    assert_eq!(outcome.images.len(), 2);
}

#[tokio::test]
async fn fal_posts_to_the_model_path_and_downloads_results() {
    let server = MockServer::start().await;
    let image_url = format!("{}/media/1.png", server.uri());
    Mock::given(method("POST"))
        .and(path("/fal-ai/flux/dev"))
        .and(header("authorization", "Key fal-key"))
        .and(body_partial_json(
            json!({"prompt": "koi", "seed": 7, "image_size": {"width": 1280, "height": 720}}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"images": [{"url": image_url, "content_type": "image/png"}]}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/media/1.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let app = app(
        home.path(),
        &format!(
            "[providers.fal]\ntype = \"fal\"\nbase_url = \"{}\"\napi_key = \"fal-key\"",
            server.uri()
        ),
    );

    let mut request = args("koi", "fal", home.path());
    request.settings = ImageSettings {
        seed: Some(7),
        size: Some("1280x720".into()),
        ..Default::default()
    };
    let outcome = app.generate(request).await.unwrap();
    assert_eq!(std::fs::read(&outcome.images[0].path).unwrap(), PNG);
}

#[tokio::test]
async fn provider_errors_carry_the_service_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "Invalid size"}})),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let app = app(
        home.path(),
        &format!(
            "[providers.openai]\ntype = \"openai\"\nbase_url = \"{}\"\napi_key = \"k\"",
            server.uri()
        ),
    );

    let error = app
        .generate(args("x", "openai", home.path()))
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("HTTP 400: Invalid size"), "{message}");
}

#[tokio::test]
async fn disconnected_providers_explain_how_to_connect() {
    let home = tempfile::tempdir().unwrap();
    let app = app(
        home.path(),
        "[providers.grok]\ntype = \"xai\"\napi_key_env = \"AI_IMAGE_MCP_TEST_UNSET\"",
    );

    let info = &app.providers()[0];
    assert!(!info.ready);
    let error = app
        .generate(args("x", "grok", home.path()))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("ai-image-mcp login xai"),
        "{error:#}"
    );
}
