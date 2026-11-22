//! Reading input images and writing generated images to disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;

use crate::config::expand_home;
use crate::http::download_image;
use crate::image::{GeneratedImage, InputImage, extension_for, sniff_mime};

/// Longest prompt fragment used in a generated file name.
const SLUG_LEN: usize = 48;

/// Loads an input image from a local path, an http(s) URL, or a `data:` URI.
pub async fn load_input(http: &reqwest::Client, source: &str) -> anyhow::Result<InputImage> {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',').context("malformed data URI")?;
        let data = BASE64
            .decode(payload)
            .context("invalid base64 in data URI")?;
        let mime_type = meta.split(';').next().filter(|m| !m.is_empty());
        return to_input(data, mime_type, "data URI");
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        let image = download_image(http, source).await?;
        return Ok(InputImage {
            data: image.data,
            mime_type: image.mime_type,
        });
    }
    let path = expand_home(Path::new(source));
    let data = tokio::fs::read(&path)
        .await
        .with_context(|| format!("reading input image {}", path.display()))?;
    to_input(data, None, source)
}

fn to_input(data: Vec<u8>, declared: Option<&str>, origin: &str) -> anyhow::Result<InputImage> {
    let Some(mime_type) = sniff_mime(&data).or(declared.filter(|m| m.starts_with("image/"))) else {
        bail!("{origin} is not a PNG, JPEG, WebP or GIF image");
    };
    Ok(InputImage {
        data,
        mime_type: mime_type.to_owned(),
    })
}

/// A generated image written to disk.
#[derive(Debug, Clone, Serialize)]
pub struct SavedImage {
    pub path: PathBuf,
    pub mime_type: String,
    pub bytes: usize,
}

/// Saves images under `dir` as `<stem>[-<n>].<ext>`, never overwriting existing files.
///
/// The stem is `name` when given, otherwise a timestamp plus a slug of the prompt.
pub async fn save_images(
    dir: &Path,
    name: Option<&str>,
    prompt: &str,
    images: &[GeneratedImage],
) -> anyhow::Result<Vec<SavedImage>> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating output directory {}", dir.display()))?;
    let stem = name.map(slug).filter(|s| !s.is_empty()).unwrap_or_else(|| {
        format!(
            "{}-{}",
            jiff::ZonedDateTime::now().strftime("%Y%m%d-%H%M%S"),
            slug(prompt)
        )
    });
    let mut saved = Vec::with_capacity(images.len());
    for (i, image) in images.iter().enumerate() {
        let base = if images.len() > 1 {
            format!("{stem}-{}", i + 1)
        } else {
            stem.clone()
        };
        let path = unused_path(dir, &base, extension_for(&image.mime_type)).await;
        tokio::fs::write(&path, &image.data)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        saved.push(SavedImage {
            path,
            mime_type: image.mime_type.clone(),
            bytes: image.data.len(),
        });
    }
    Ok(saved)
}

async fn unused_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{base}.{ext}"));
    let mut suffix = 2;
    while tokio::fs::try_exists(&path).await.unwrap_or(false) {
        path = dir.join(format!("{base}-v{suffix}.{ext}"));
        suffix += 1;
    }
    path
}

/// Lowercase ASCII words joined by dashes, safe as a file name on every platform.
fn slug(text: &str) -> String {
    let mut slug = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if slug.len() + word.len() + 1 > SLUG_LEN {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    if slug.is_empty() {
        "image".to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn png() -> GeneratedImage {
        GeneratedImage {
            data: PNG.to_vec(),
            mime_type: "image/png".into(),
            revised_prompt: None,
        }
    }

    #[test]
    fn slugs_are_short_and_safe() {
        assert_eq!(slug("A Red Fox, in the SNOW!"), "a-red-fox-in-the-snow");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("???"), "image");
        assert!(slug(&"word ".repeat(40)).len() <= SLUG_LEN);
    }

    #[tokio::test]
    async fn saving_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let first = save_images(dir.path(), Some("cover"), "", &[png()])
            .await
            .unwrap();
        let second = save_images(dir.path(), Some("cover"), "", &[png()])
            .await
            .unwrap();
        assert_eq!(first[0].path, dir.path().join("cover.png"));
        assert_eq!(second[0].path, dir.path().join("cover-v2.png"));

        let batch = save_images(dir.path(), Some("set"), "", &[png(), png()])
            .await
            .unwrap();
        let names: Vec<_> = batch
            .iter()
            .map(|s| s.path.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["set-1.png", "set-2.png"]);
    }

    #[tokio::test]
    async fn loads_files_and_data_uris() {
        let http = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.png");
        std::fs::write(&path, PNG).unwrap();

        let from_file = load_input(&http, path.to_str().unwrap()).await.unwrap();
        assert_eq!(from_file.mime_type, "image/png");

        let uri = format!("data:image/png;base64,{}", BASE64.encode(PNG));
        assert_eq!(load_input(&http, &uri).await.unwrap(), from_file);

        std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        assert!(
            load_input(&http, dir.path().join("notes.txt").to_str().unwrap())
                .await
                .is_err()
        );
    }
}
