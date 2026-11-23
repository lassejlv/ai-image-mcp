//! Helpers shared by the integration tests.

#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::path::Path;

use ai_image_mcp::app::{App, OutputImage};
use ai_image_mcp::config::{Config, Paths};
use ai_image_mcp::storage::ImageLocation;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

/// The smallest valid PNG signature; enough for format sniffing.
pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-image";

pub fn png_b64() -> String {
    STANDARD.encode(PNG)
}

/// Builds an app from TOML, with credentials stored under `home`.
pub async fn app(home: &Path, toml: &str) -> App {
    let config: Config = toml::from_str(toml).expect("valid test config");
    App::new(&config, &Paths::new(home.to_path_buf()))
        .await
        .expect("app builds")
}

/// An unsigned JWT whose claims are `claims`, valid for an hour.
pub fn jwt(mut claims: Value) -> String {
    claims["exp"] = json!(jiff::Timestamp::now().as_second() + 3600);
    format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

/// Writes a saved login the way `ai-image-mcp login` does.
pub fn save_login(home: &Path, issuer: &str, access_token: &str) {
    let dir = home.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{issuer}.json")),
        json!({"access_token": access_token, "refresh_token": "refresh"}).to_string(),
    )
    .unwrap();
}

/// The file an image was saved to; panics for database storage.
pub fn saved_path(image: &OutputImage) -> &Path {
    match &image.location {
        ImageLocation::Path(path) => path,
        other => panic!("expected a file, got {other}"),
    }
}
