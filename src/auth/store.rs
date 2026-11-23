//! Where an OAuth grant lives: a local file or a row in Postgres.

use std::future::Future;

use anyhow::{Context, bail};
use sqlx::PgPool;
use sqlx::types::Json;

use super::Issuer;
use super::tokens::{OAuthTokens, TokenFile};

/// Persistence for one OAuth grant.
#[derive(Debug, Clone)]
pub enum TokenStore {
    File(TokenFile),
    /// The `oauth_grants` row for `issuer`.
    Postgres {
        pool: PgPool,
        issuer: Issuer,
    },
}

impl TokenStore {
    pub async fn exists(&self) -> anyhow::Result<bool> {
        match self {
            Self::File(file) => Ok(file.path().is_file()),
            Self::Postgres { pool, issuer } => Ok(sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM oauth_grants WHERE issuer = $1)",
            )
            .bind(issuer.id())
            .fetch_one(pool)
            .await?),
        }
    }

    pub async fn save(&self, tokens: &OAuthTokens) -> anyhow::Result<()> {
        match self {
            Self::File(file) => file.save(tokens),
            Self::Postgres { pool, issuer } => {
                upsert(pool, *issuer, tokens).await?;
                Ok(())
            }
        }
    }

    /// Removes the grant. Returns false when there was none.
    pub async fn delete(&self) -> anyhow::Result<bool> {
        match self {
            Self::File(TokenFile::Own(path)) => match std::fs::remove_file(path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
            },
            Self::File(TokenFile::CodexCli(_)) => {
                bail!("the Codex CLI owns this login; run `codex logout`")
            }
            Self::Postgres { pool, issuer } => {
                let result = sqlx::query("DELETE FROM oauth_grants WHERE issuer = $1")
                    .bind(issuer.id())
                    .execute(pool)
                    .await?;
                Ok(result.rows_affected() > 0)
            }
        }
    }

    /// Loads the grant and passes it to `update`; a returned replacement is written back.
    ///
    /// In Postgres the row stays locked until the write, so several server processes
    /// sharing one database never spend the same rotating refresh token twice.
    pub async fn update<F, Fut>(&self, update: F) -> anyhow::Result<OAuthTokens>
    where
        F: FnOnce(OAuthTokens) -> Fut,
        Fut: Future<Output = anyhow::Result<Option<OAuthTokens>>>,
    {
        match self {
            Self::File(file) => {
                let tokens = file.load()?;
                match update(tokens.clone()).await? {
                    Some(next) => {
                        file.save(&next)?;
                        Ok(next)
                    }
                    None => Ok(tokens),
                }
            }
            Self::Postgres { pool, issuer } => {
                let mut tx = pool.begin().await?;
                let Json(tokens): Json<OAuthTokens> = sqlx::query_scalar(
                    "SELECT tokens FROM oauth_grants WHERE issuer = $1 FOR UPDATE",
                )
                .bind(issuer.id())
                .fetch_optional(&mut *tx)
                .await?
                .with_context(|| format!("no saved {} login", issuer.display_name()))?;
                let Some(next) = update(tokens.clone()).await? else {
                    tx.commit().await?;
                    return Ok(tokens);
                };
                upsert(&mut *tx, *issuer, &next).await?;
                tx.commit().await?;
                Ok(next)
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::File(file) => file.path().display().to_string(),
            Self::Postgres { .. } => "Postgres".to_owned(),
        }
    }
}

async fn upsert<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    issuer: Issuer,
    tokens: &OAuthTokens,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO oauth_grants (issuer, tokens, updated_at) VALUES ($1, $2, now()) \
         ON CONFLICT (issuer) DO UPDATE SET tokens = EXCLUDED.tokens, updated_at = now()",
    )
    .bind(issuer.id())
    .bind(Json(tokens))
    .execute(executor)
    .await?;
    Ok(())
}
