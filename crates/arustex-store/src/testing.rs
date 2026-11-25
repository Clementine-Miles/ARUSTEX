//! Disposable databases for integration tests.
//!
//! Set `ARUSTEX_TEST_DATABASE_URL` to an administrative connection URL (for example a
//! throwaway PostgreSQL container). Each [`TestDb`] creates a uniquely named database,
//! migrates it, and drops it on [`TestDb::drop_db`]. When the variable is unset the
//! helper returns `None` so tests can skip; `ARUSTEX_REQUIRE_PG=1` turns that into a
//! failure (used in CI).

use arustex_core::{CoreError, CoreResult};
use sqlx::Connection;
use sqlx::postgres::PgConnection;

use crate::postgres::{Store, StoreOptions, db_err};

pub const TEST_DATABASE_ENV: &str = "ARUSTEX_TEST_DATABASE_URL";
pub const REQUIRE_PG_ENV: &str = "ARUSTEX_REQUIRE_PG";

#[derive(Debug)]
pub struct TestDb {
    admin_url: String,
    pub name: String,
    pub url: String,
    pub store: Store,
}

fn with_database(url: &str, name: &str) -> CoreResult<String> {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let scheme_end = base.find("://").ok_or_else(|| CoreError::invalid("invalid database url"))? + 3;
    let path_start = base[scheme_end..].find('/').map(|i| i + scheme_end);
    let host = match path_start {
        Some(i) => &base[..i],
        None => base,
    };
    Ok(match query {
        Some(q) => format!("{host}/{name}?{q}"),
        None => format!("{host}/{name}"),
    })
}

impl TestDb {
    /// Creates and migrates a fresh database, or returns `None` when no test
    /// database is configured (and it is not required).
    pub async fn create() -> Option<Self> {
        let admin_url = match std::env::var(TEST_DATABASE_ENV) {
            Ok(url) if !url.is_empty() => url,
            _ => {
                assert!(
                    std::env::var(REQUIRE_PG_ENV).as_deref() != Ok("1"),
                    "{TEST_DATABASE_ENV} must be set when {REQUIRE_PG_ENV}=1"
                );
                eprintln!("skipping: {TEST_DATABASE_ENV} is not set");
                return None;
            }
        };
        Some(Self::create_at(&admin_url).await.expect("create test database"))
    }

    pub async fn create_at(admin_url: &str) -> CoreResult<Self> {
        let name = format!("arustex_test_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(admin_url).await.map_err(db_err)?;
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut admin)
            .await
            .map_err(db_err)?;
        admin.close().await.map_err(db_err)?;
        let url = with_database(admin_url, &name)?;
        let options = StoreOptions { max_connections: 8, ..StoreOptions::default() };
        let store = Store::connect(&url, &options).await?;
        store.migrate().await?;
        Ok(Self { admin_url: admin_url.to_owned(), name, url, store })
    }

    /// Closes the pool and drops the database.
    pub async fn drop_db(self) {
        self.store.close().await;
        if let Ok(mut admin) = PgConnection::connect(&self.admin_url).await {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.name
            )))
            .execute(&mut admin)
            .await;
            let _ = admin.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_database;

    #[test]
    fn database_name_is_replaced() {
        assert_eq!(
            with_database("postgres://u@h:5432/postgres", "x").unwrap(),
            "postgres://u@h:5432/x"
        );
        assert_eq!(
            with_database("postgres://u@h/postgres?sslmode=disable", "x").unwrap(),
            "postgres://u@h/x?sslmode=disable"
        );
        assert_eq!(with_database("postgres://h", "x").unwrap(), "postgres://h/x");
    }
}
