//! The MCP surface end to end: an rmcp client talking to the server over an in-memory pipe.

mod common;

use ai_image_mcp::server::ImageServer;
use common::{app, png_b64};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn tools_are_listed_and_generate_image_returns_files_and_data() {
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
    );

    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let running = ImageServer::new(app)
            .serve(server_io)
            .await
            .expect("server starts");
        running.waiting().await.ok();
    });
    let client = ().serve(client_io).await.expect("client connects");

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names.len(), 2);
    assert!(
        names.contains(&"generate_image") && names.contains(&"list_providers"),
        "{names:?}"
    );
    let schema = Value::Object(
        (*tools
            .iter()
            .find(|t| t.name == "generate_image")
            .unwrap()
            .input_schema)
            .clone(),
    );
    for field in [
        "prompt",
        "provider",
        "model",
        "input_images",
        "aspect_ratio",
        "quality",
        "extra",
    ] {
        assert!(
            schema["properties"].get(field).is_some(),
            "schema lacks {field}: {schema}"
        );
    }
    assert_eq!(schema["required"], json!(["prompt"]));

    let providers = client
        .call_tool(CallToolRequestParams::new("list_providers"))
        .await
        .unwrap();
    let providers = serde_json::to_value(&providers).unwrap();
    assert_eq!(
        providers["structuredContent"]["providers"][0]["name"],
        "openai"
    );
    assert_eq!(
        providers["structuredContent"]["providers"][0]["default"],
        true
    );

    let result = client
        .call_tool(
            CallToolRequestParams::new("generate_image").with_arguments(
                json!({"prompt": "a red fox", "size": "1024x1024"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let result = serde_json::to_value(&result).unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["content"][1]["type"], "image");
    assert_eq!(result["content"][1]["mimeType"], "image/png");
    let saved = result["structuredContent"]["images"][0]["path"]
        .as_str()
        .unwrap();
    assert!(saved.starts_with(out.to_str().unwrap()) && std::path::Path::new(saved).is_file());

    let failed = client
        .call_tool(
            CallToolRequestParams::new("generate_image").with_arguments(
                json!({"prompt": "x", "provider": "missing"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let failed = serde_json::to_value(&failed).unwrap();
    assert_eq!(failed["isError"], true);
    assert!(
        failed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown provider \"missing\"")
    );

    client.cancel().await.unwrap();
}
