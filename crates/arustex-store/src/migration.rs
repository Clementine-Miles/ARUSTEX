use arustex_core::{CoreError, CoreResult};
use serde::Serialize;
use sqlx::migrate::Migrator;

use crate::postgres::{Store, db_err};

/// Versioned SQL migrations embedded from the workspace `migrations/` directory.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationState {
    pub version: i64,
    pub description: String,
    pub applied: bool,
}

impl Store {
    /// Applies pending migrations inside the migrator's own advisory lock.
    pub async fn migrate(&self) -> CoreResult<()> {
        MIGRATOR
            .run(self.pool())
            .await
            .map_err(|e| CoreError::Storage(format!("migration failed: {e}")))
    }

    /// Lists every known migration and whether it has been applied.
    pub async fn migration_status(&self) -> CoreResult<Vec<MigrationState>> {
        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = '_sqlx_migrations')",
        )
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let applied: Vec<i64> = if table_exists {
            sqlx::query_scalar(
                "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version",
            )
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?
        } else {
            Vec::new()
        };
        Ok(MIGRATOR
            .iter()
            .map(|m| MigrationState {
                version: m.version,
                description: m.description.to_string(),
                applied: applied.contains(&m.version),
            })
            .collect())
    }

    /// Fails unless every embedded migration has been applied.
    pub async fn ensure_migrated(&self) -> CoreResult<()> {
        let pending: Vec<_> = self
            .migration_status()
            .await?
            .into_iter()
            .filter(|m| !m.applied)
            .map(|m| m.version.to_string())
            .collect();
        if pending.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Unavailable(format!(
                "database schema is not up to date (pending migrations: {}); run `arustex db migrate`",
                pending.join(", ")
            )))
        }
    }
}
