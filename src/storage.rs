//! Where generated images and saved logins are kept: local files, or Postgres when a
//! database URL is configured.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde::Serialize;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;

use crate::auth::{Issuer, TokenFile, TokenStore};
use crate::config::{Config, Paths};
use crate::image::{GeneratedImage, ImageSettings, extension_for};
use crate::media::save_images;

/// Connections a server process keeps open; image generation is not database-heavy.
const MAX_CONNECTIONS: u32 = 5;

/// The storage backend for one server process.
#[derive(Debug, Clone)]
pub enum Storage {
    Files { paths: Paths, output_dir: PathBuf },
    Postgres(PgPool),
}

/// Where one saved image can be found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageLocation {
    /// A file on disk.
    Path(PathBuf),
    /// A row in the `images` table.
    Id(i64),
}

impl ImageLocation {
    /// The key the dashboard serves this image under.
    pub fn key(&self) -> String {
        match self {
            Self::Path(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Self::Id(id) => id.to_string(),
        }
    }
}

impl fmt::Display for ImageLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => write!(f, "{}", path.display()),
            Self::Id(id) => write!(f, "image {id} in Postgres"),
        }
    }
}

/// What a generation was, stored next to its images.
#[derive(Debug, Clone)]
pub struct ImageRecord<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub prompt: &'a str,
    pub settings: &'a ImageSettings,
}

/// A saved image as listed in the gallery.
#[derive(Debug, Clone, Serialize)]
pub struct StoredImage {
    pub key: String,
    pub name: String,
    pub bytes: u64,
    /// Unix seconds.
    pub created: i64,
}

impl Storage {
    /// Connects to Postgres and applies migrations when a database URL is configured,
    /// otherwise uses local files.
    pub async fn open(config: &Config, paths: &Paths) -> anyhow::Result<Self> {
        let Some(url) = config.database_url() else {
            return Ok(Self::Files {
                paths: paths.clone(),
                output_dir: config.output.dir(),
            });
        };
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await
            .context("connecting to Postgres")?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .context("running database migrations")?;
        Ok(Self::Postgres(pool))
    }

    /// Where `ai-image-mcp login` keeps the grant for `issuer`.
    pub fn login_store(&self, issuer: Issuer) -> TokenStore {
        match self {
            Self::Files { paths, .. } => TokenStore::File(TokenFile::Own(paths.token_file(issuer))),
            Self::Postgres(pool) => TokenStore::Postgres {
                pool: pool.clone(),
                issuer,
            },
        }
    }

    /// A short description of where images are saved.
    pub fn describe(&self) -> String {
        match self {
            Self::Files { output_dir, .. } => output_dir.display().to_string(),
            Self::Postgres(_) => "Postgres".to_owned(),
        }
    }

    /// Saves generated images. `dir` and `name` choose the folder and file name and only
    /// apply to file storage.
    pub async fn save(
        &self,
        record: &ImageRecord<'_>,
        dir: Option<&Path>,
        name: Option<&str>,
        images: &[GeneratedImage],
    ) -> anyhow::Result<Vec<ImageLocation>> {
        match self {
            Self::Files { output_dir, .. } => {
                let saved =
                    save_images(dir.unwrap_or(output_dir), name, record.prompt, images).await?;
                Ok(saved.into_iter().map(ImageLocation::Path).collect())
            }
            Self::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let mut locations = Vec::with_capacity(images.len());
                for image in images {
                    let id: i64 = sqlx::query_scalar(
                        "INSERT INTO images (provider, model, prompt, revised_prompt, settings, mime_type, data) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
                    )
                    .bind(record.provider)
                    .bind(record.model)
                    .bind(record.prompt)
                    .bind(&image.revised_prompt)
                    .bind(Json(record.settings))
                    .bind(&image.mime_type)
                    .bind(&image.data)
                    .fetch_one(&mut *tx)
                    .await?;
                    locations.push(ImageLocation::Id(id));
                }
                tx.commit().await?;
                Ok(locations)
            }
        }
    }

    /// The most recent images, newest first.
    pub async fn list(&self, limit: usize) -> anyhow::Result<Vec<StoredImage>> {
        match self {
            Self::Files { output_dir, .. } => list_files(output_dir, limit).await,
            Self::Postgres(pool) => {
                let rows: Vec<(i64, String, String, i64, i64)> = sqlx::query_as(
                    "SELECT id, prompt, mime_type, octet_length(data)::bigint, \
                     extract(epoch FROM created_at)::bigint \
                     FROM images ORDER BY created_at DESC, id DESC LIMIT $1",
                )
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .fetch_all(pool)
                .await?;
                Ok(rows
                    .into_iter()
                    .map(|(id, prompt, mime_type, bytes, created)| StoredImage {
                        key: id.to_string(),
                        name: format!("{id}-{}.{}", short(&prompt), extension_for(&mime_type)),
                        bytes: u64::try_from(bytes).unwrap_or_default(),
                        created,
                    })
                    .collect())
            }
        }
    }

    /// Reads one image by the key [`StoredImage::key`] or [`ImageLocation::key`] gave it.
    /// Returns its MIME type and bytes, or `None` when there is no such image.
    pub async fn read(&self, key: &str) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        match self {
            Self::Files { output_dir, .. } => {
                let Some(mime) = image_mime(key).filter(|_| is_plain_file_name(key)) else {
                    return Ok(None);
                };
                match tokio::fs::read(output_dir.join(key)).await {
                    Ok(data) => Ok(Some((mime.to_owned(), data))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
            Self::Postgres(pool) => {
                let Ok(id) = key.parse::<i64>() else {
                    return Ok(None);
                };
                Ok(
                    sqlx::query_as("SELECT mime_type, data FROM images WHERE id = $1")
                        .bind(id)
                        .fetch_optional(pool)
                        .await?,
                )
            }
        }
    }
}

async fn list_files(dir: &Path, limit: usize) -> anyhow::Result<Vec<StoredImage>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut images = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = entry.metadata().await?;
        if !metadata.is_file() || image_mime(&name).is_none() {
            continue;
        }
        let created = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or_default();
        images.push(StoredImage {
            key: name.clone(),
            name,
            bytes: metadata.len(),
            created,
        });
    }
    images.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.name.cmp(&a.name)));
    images.truncate(limit);
    Ok(images)
}

/// A file name with no directory parts, so it cannot escape the output folder.
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && !name.starts_with('.')
}

/// The MIME type for a file name with an image extension this server writes.
fn image_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    ["image/png", "image/jpeg", "image/webp", "image/gif"]
        .into_iter()
        .find(|mime| extension_for(mime) == ext || (ext == "jpeg" && *mime == "image/jpeg"))
}

/// The first few words of a prompt, for a readable gallery name.
fn short(prompt: &str) -> String {
    prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
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

    #[test]
    fn file_keys_cannot_leave_the_output_folder() {
        assert!(is_plain_file_name("fox.png"));
        assert!(!is_plain_file_name("../fox.png"));
        assert!(!is_plain_file_name("a/b.png"));
        assert!(!is_plain_file_name(".hidden.png"));
    }

    #[test]
    fn locations_have_stable_keys() {
        assert_eq!(ImageLocation::Path("/out/fox.png".into()).key(), "fox.png");
        assert_eq!(ImageLocation::Id(42).key(), "42");
        assert_eq!(ImageLocation::Id(42).to_string(), "image 42 in Postgres");
    }
}
