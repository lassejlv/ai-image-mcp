//! Postgres storage against a real database. Set `AI_IMAGE_MCP_TEST_DATABASE_URL` to a
//! Postgres URL whose user may create databases; each test runs in a fresh database.
//! Without it these tests are skipped.

mod common;

use ai_image_mcp::app::GenerateImageArgs;
use ai_image_mcp::auth::{Issuer, OAuthTokens};
use ai_image_mcp::storage::ImageLocation;
use common::{PNG, app, jwt, png_b64};
use serde_json::json;
use sqlx::{Connection, PgConnection};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADMIN_URL_ENV: &str = "AI_IMAGE_MCP_TEST_DATABASE_URL";

/// Creates an empty database and returns its URL, or `None` when no test server is configured.
async fn fresh_database() -> Option<String> {
    let Ok(admin_url) = std::env::var(ADMIN_URL_ENV) else {
        eprintln!("skipping: {ADMIN_URL_ENV} is not set");
        return None;
    };
    let name = format!(
        "aim_test_{}",
        std::process::id() as u64 * 1_000_000 + rand_suffix()
    );
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect to test server");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut admin)
        .await
        .expect("create test database");
    let (base, _) = admin_url.rsplit_once('/').expect("URL has a database path");
    Some(format!("{base}/{name}"))
}

fn rand_suffix() -> u64 {
    u64::from(jiff::Timestamp::now().subsec_nanosecond().unsigned_abs()) % 1_000_000
}

fn config(database_url: &str, extra: &str) -> String {
    format!("database_url = \"{database_url}\"\n{extra}")
}

fn tokens(access_token: String, refresh_token: &str) -> OAuthTokens {
    OAuthTokens {
        access_token,
        refresh_token: refresh_token.into(),
        id_token: None,
        expires_at: None,
    }
}

#[tokio::test]
async fn images_are_stored_listed_and_read_from_postgres() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/images/generations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"b64_json": png_b64()}, {"b64_json": png_b64()}]})),
        )
        .mount(&api)
        .await;
    let home = tempfile::tempdir().unwrap();
    let toml = config(
        &url,
        &format!(
            "[providers.openai]\ntype = \"openai\"\nbase_url = \"{}\"\napi_key = \"k\"\ndefaults = {{ quality = \"high\" }}",
            api.uri()
        ),
    );
    app(home.path(), &toml).await;
    let app = app(home.path(), &toml).await;

    let outcome = app
        .generate(GenerateImageArgs {
            prompt: "Two red foxes in snow".into(),
            settings: ai_image_mcp::image::ImageSettings {
                n: Some(2),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    let ids: Vec<i64> = outcome
        .images
        .iter()
        .map(|image| match image.location {
            ImageLocation::Id(id) => id,
            ref other => panic!("expected a database id, got {other}"),
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(
        outcome
            .summary()
            .contains(&format!("image {} in Postgres", ids[0]))
    );
    assert_eq!(
        std::fs::read_dir(home.path()).unwrap().count(),
        0,
        "nothing is written to disk"
    );

    let storage = app.storage();
    let listed = storage.list(10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].key, ids[1].to_string(), "newest first");
    assert!(
        listed[0].name.ends_with("-two-red-foxes-in-snow.png"),
        "{}",
        listed[0].name
    );

    let (mime, data) = storage.read(&ids[0].to_string()).await.unwrap().unwrap();
    assert_eq!((mime.as_str(), data.as_slice()), ("image/png", PNG));
    assert!(storage.read("999999").await.unwrap().is_none());
    assert!(storage.read("../etc/passwd").await.unwrap().is_none());

    let mut conn = PgConnection::connect(&url).await.unwrap();
    let (provider, quality): (String, String) =
        sqlx::query_as("SELECT provider, settings->>'quality' FROM images WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!((provider.as_str(), quality.as_str()), ("openai", "high"));
}

#[tokio::test]
async fn logins_live_in_postgres() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let api = MockServer::start().await;
    let token = jwt(json!({"sub": "user"}));
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"b64_json": png_b64()}]})),
        )
        .expect(1)
        .mount(&api)
        .await;
    let home = tempfile::tempdir().unwrap();
    let app = app(
        home.path(),
        &config(
            &url,
            &format!(
                "[providers.grok]\ntype = \"xai\"\nauth = \"oauth\"\nbase_url = \"{}/v1\"",
                api.uri()
            ),
        ),
    )
    .await;
    let store = app.storage().login_store(Issuer::Xai);
    assert!(!app.providers().await[0].ready);

    store.save(&tokens(token.clone(), "r1")).await.unwrap();
    let info = &app.providers().await[0];
    assert!(info.ready, "{}", info.auth);
    assert_eq!(store.describe(), "Postgres");
    app.generate(GenerateImageArgs {
        prompt: "a lighthouse".into(),
        provider: Some("grok".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(
        !home.path().join("credentials").exists(),
        "no token file is written"
    );

    assert!(store.delete().await.unwrap());
    assert!(!store.delete().await.unwrap());
    assert!(!app.providers().await[0].ready);
}

#[tokio::test]
async fn concurrent_refreshes_are_serialized_by_the_row_lock() {
    let Some(url) = fresh_database().await else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let app = app(home.path(), &config(&url, "")).await;
    let store = app.storage().login_store(Issuer::ChatGpt);
    store.save(&tokens("a".into(), "0")).await.unwrap();

    let updates: Vec<_> = (0..5)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .update(|current| async move {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        let next: u32 = current.refresh_token.parse::<u32>().unwrap() + 1;
                        Ok(Some(tokens("a".into(), &next.to_string())))
                    })
                    .await
                    .unwrap()
            })
        })
        .collect();
    for update in updates {
        update.await.unwrap();
    }

    let last = store.update(|_| async { Ok(None) }).await.unwrap();
    assert_eq!(
        last.refresh_token, "5",
        "every refresh saw the previous one's token"
    );
}
