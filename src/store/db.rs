use crate::config::DatabaseConfig;
use sqlx::AnyPool;
use sqlx::Connection;
use sqlx::postgres::PgConnection;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::info;

const SCHEMA_VERSION: i32 = 1;

pub async fn init_db(driver: &str, dsn: &str) -> Result<AnyPool, sqlx::Error> {
    if driver == "sqlite" {
        if let Some(parent) = Path::new(dsn).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let pool = AnyPool::connect(&format!("sqlite:{}?mode=rwc", dsn)).await?;
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&pool)
            .await
            .ok();
        Ok(pool)
    } else {
        let pool = AnyPool::connect(dsn).await?;
        Ok(pool)
    }
}

pub async fn ensure_postgres_database(cfg: &DatabaseConfig) -> Result<(), String> {
    if cfg.driver() != "postgres" || cfg.has_explicit_dsn() {
        return Ok(());
    }

    if !Path::new("/.dockerenv").exists() {
        start_compose_postgres()?;
    } else {
        info!("DATABASE_DSN not set, using compose postgres service");
    }

    wait_for_postgres(cfg).await?;
    create_database_if_missing(cfg).await?;
    Ok(())
}

pub async fn migrate(pool: &AnyPool, driver: &str) -> Result<(), sqlx::Error> {
    // Fast path: if schema_migrations records the current version, skip everything.
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)")
        .execute(pool)
        .await?;
    let applied: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM schema_migrations WHERE version >= {}",
        SCHEMA_VERSION
    ))
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if applied > 0 {
        return Ok(());
    }

    let schema = if driver == "sqlite" {
        SQLITE_SCHEMA
    } else {
        PG_SCHEMA
    };
    for stmt in schema.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt).execute(pool).await?;
    }

    // api_tokens 表 — create before column-existence probing so both tables are present.
    let token_schema = if driver == "sqlite" {
        SQLITE_TOKENS_SCHEMA
    } else {
        PG_TOKENS_SCHEMA
    };
    for stmt in token_schema.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt).execute(pool).await?;
    }

    // 增量迁移 — only ALTER columns that are actually missing, so remote-DB startups
    // don't pay ~20 round-trips for ALTERs that would otherwise fail with "column
    // already exists" and get swallowed by .ok().
    let ts_type = if driver == "sqlite" {
        "TEXT"
    } else {
        "TIMESTAMPTZ"
    };
    let json_type = if driver == "sqlite" { "TEXT" } else { "JSONB" };
    let cols = existing_columns(pool, driver, "accounts").await;

    let pending: [(&str, String); 15] = [
        (
            "billing_mode",
            "ALTER TABLE accounts ADD COLUMN billing_mode TEXT NOT NULL DEFAULT 'strip'".into(),
        ),
        (
            "usage_data",
            format!(
                "ALTER TABLE accounts ADD COLUMN usage_data {} NOT NULL DEFAULT '{{}}'",
                json_type
            ),
        ),
        (
            "usage_fetched_at",
            format!(
                "ALTER TABLE accounts ADD COLUMN usage_fetched_at {}",
                ts_type
            ),
        ),
        (
            "auth_type",
            "ALTER TABLE accounts ADD COLUMN auth_type TEXT NOT NULL DEFAULT 'setup_token'".into(),
        ),
        (
            "access_token",
            "ALTER TABLE accounts ADD COLUMN access_token TEXT NOT NULL DEFAULT ''".into(),
        ),
        (
            "refresh_token",
            "ALTER TABLE accounts ADD COLUMN refresh_token TEXT NOT NULL DEFAULT ''".into(),
        ),
        (
            "oauth_expires_at",
            format!(
                "ALTER TABLE accounts ADD COLUMN oauth_expires_at {}",
                ts_type
            ),
        ),
        (
            "oauth_refreshed_at",
            format!(
                "ALTER TABLE accounts ADD COLUMN oauth_refreshed_at {}",
                ts_type
            ),
        ),
        (
            "auth_error",
            "ALTER TABLE accounts ADD COLUMN auth_error TEXT NOT NULL DEFAULT ''".into(),
        ),
        (
            "account_uuid",
            "ALTER TABLE accounts ADD COLUMN account_uuid TEXT".into(),
        ),
        (
            "organization_uuid",
            "ALTER TABLE accounts ADD COLUMN organization_uuid TEXT".into(),
        ),
        (
            "subscription_type",
            "ALTER TABLE accounts ADD COLUMN subscription_type TEXT".into(),
        ),
        (
            "disable_reason",
            "ALTER TABLE accounts ADD COLUMN disable_reason TEXT NOT NULL DEFAULT ''".into(),
        ),
        (
            "auto_telemetry",
            "ALTER TABLE accounts ADD COLUMN auto_telemetry INTEGER NOT NULL DEFAULT 0".into(),
        ),
        (
            "telemetry_count",
            "ALTER TABLE accounts ADD COLUMN telemetry_count INTEGER NOT NULL DEFAULT 0".into(),
        ),
    ];
    for (name, sql) in pending.iter() {
        if !cols.contains(*name) {
            sqlx::query(sql).execute(pool).await.ok();
        }
    }

    // Fix column types for existing PG databases that may have TEXT instead of TIMESTAMPTZ/JSONB.
    // Only run when the current data_type doesn't already match.
    if driver != "sqlite" {
        let types = column_types(pool, "accounts").await;
        let needs_type = |col: &str, want: &str| {
            types
                .get(col)
                .map(|t| !t.eq_ignore_ascii_case(want))
                .unwrap_or(false)
        };
        if needs_type("usage_data", "jsonb") {
            sqlx::query(
                "ALTER TABLE accounts ALTER COLUMN usage_data TYPE JSONB USING usage_data::JSONB",
            )
            .execute(pool)
            .await
            .ok();
        }
        if needs_type("usage_fetched_at", "timestamp with time zone") {
            sqlx::query("ALTER TABLE accounts ALTER COLUMN usage_fetched_at TYPE TIMESTAMPTZ USING usage_fetched_at::TIMESTAMPTZ")
                .execute(pool)
                .await
                .ok();
        }
        if needs_type("oauth_expires_at", "timestamp with time zone") {
            sqlx::query("ALTER TABLE accounts ALTER COLUMN oauth_expires_at TYPE TIMESTAMPTZ USING oauth_expires_at::TIMESTAMPTZ")
                .execute(pool)
                .await
                .ok();
        }
        if needs_type("oauth_refreshed_at", "timestamp with time zone") {
            sqlx::query("ALTER TABLE accounts ALTER COLUMN oauth_refreshed_at TYPE TIMESTAMPTZ USING oauth_refreshed_at::TIMESTAMPTZ")
                .execute(pool)
                .await
                .ok();
        }
    }

    // Stamp the version last so a partial failure above causes a clean retry next boot.
    sqlx::query(&format!(
        "INSERT INTO schema_migrations (version) VALUES ({}) ON CONFLICT DO NOTHING",
        SCHEMA_VERSION
    ))
    .execute(pool)
    .await
    .ok();

    Ok(())
}

async fn existing_columns(pool: &AnyPool, driver: &str, table: &str) -> HashSet<String> {
    let sql = if driver == "sqlite" {
        format!(
            "SELECT name FROM pragma_table_info('{}')",
            table.replace('\'', "''")
        )
    } else {
        format!(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = '{}'",
            table.replace('\'', "''")
        )
    };
    sqlx::query_scalar::<_, String>(&sql)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

async fn column_types(pool: &AnyPool, table: &str) -> std::collections::HashMap<String, String> {
    let sql = format!(
        "SELECT column_name, data_type FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = '{}'",
        table.replace('\'', "''")
    );
    match sqlx::query_as::<_, (String, String)>(&sql)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows.into_iter().collect(),
        Err(_) => std::collections::HashMap::new(),
    }
}

const SQLITE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL DEFAULT '',
    email           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    token           TEXT NOT NULL,
    auth_type       TEXT NOT NULL DEFAULT 'setup_token',
    access_token    TEXT NOT NULL DEFAULT '',
    refresh_token   TEXT NOT NULL DEFAULT '',
    oauth_expires_at    TEXT,
    oauth_refreshed_at  TEXT,
    auth_error      TEXT NOT NULL DEFAULT '',
    proxy_url       TEXT NOT NULL DEFAULT '',
    device_id       TEXT NOT NULL,
    canonical_env   TEXT NOT NULL DEFAULT '{}',
    canonical_prompt_env TEXT NOT NULL DEFAULT '{}',
    canonical_process    TEXT NOT NULL DEFAULT '{}',
    billing_mode    TEXT NOT NULL DEFAULT 'strip',
    concurrency     INTEGER NOT NULL DEFAULT 3,
    priority        INTEGER NOT NULL DEFAULT 50,
    rate_limited_at      TEXT,
    rate_limit_reset_at  TEXT,
    account_uuid         TEXT,
    organization_uuid    TEXT,
    subscription_type    TEXT,
    disable_reason       TEXT NOT NULL DEFAULT '',
    auto_telemetry       INTEGER NOT NULL DEFAULT 0,
    telemetry_count      INTEGER NOT NULL DEFAULT 0,
    usage_data           TEXT NOT NULL DEFAULT '{}',
    usage_fetched_at     TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

"#;

const PG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id              BIGSERIAL PRIMARY KEY,
    name            TEXT NOT NULL DEFAULT '',
    email           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    token           TEXT NOT NULL,
    auth_type       TEXT NOT NULL DEFAULT 'setup_token',
    access_token    TEXT NOT NULL DEFAULT '',
    refresh_token   TEXT NOT NULL DEFAULT '',
    oauth_expires_at    TIMESTAMPTZ,
    oauth_refreshed_at  TIMESTAMPTZ,
    auth_error      TEXT NOT NULL DEFAULT '',
    proxy_url       TEXT NOT NULL DEFAULT '',
    device_id       TEXT NOT NULL,
    canonical_env   JSONB NOT NULL DEFAULT '{}',
    canonical_prompt_env JSONB NOT NULL DEFAULT '{}',
    canonical_process    JSONB NOT NULL DEFAULT '{}',
    billing_mode    TEXT NOT NULL DEFAULT 'strip',
    concurrency     INT NOT NULL DEFAULT 3,
    priority        INT NOT NULL DEFAULT 50,
    rate_limited_at      TIMESTAMPTZ,
    rate_limit_reset_at  TIMESTAMPTZ,
    account_uuid         TEXT,
    organization_uuid    TEXT,
    subscription_type    TEXT,
    disable_reason       TEXT NOT NULL DEFAULT '',
    auto_telemetry       INT NOT NULL DEFAULT 0,
    telemetry_count      BIGINT NOT NULL DEFAULT 0,
    usage_data           JSONB NOT NULL DEFAULT '{}',
    usage_fetched_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

"#;

const SQLITE_TOKENS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_tokens (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    name                TEXT NOT NULL DEFAULT '',
    token               TEXT NOT NULL UNIQUE,
    allowed_accounts    TEXT NOT NULL DEFAULT '',
    blocked_accounts    TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'active',
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
)
"#;

const PG_TOKENS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_tokens (
    id                  BIGSERIAL PRIMARY KEY,
    name                TEXT NOT NULL DEFAULT '',
    token               TEXT NOT NULL UNIQUE,
    allowed_accounts    TEXT NOT NULL DEFAULT '',
    blocked_accounts    TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'active',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
)
"#;

fn start_compose_postgres() -> Result<(), String> {
    info!("DATABASE_DSN not set, starting postgres via docker compose");
    let output = Command::new("docker")
        .args(["compose", "up", "-d", "postgres"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|err| format!("failed to run docker compose: {err}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(format!(
        "docker compose up -d postgres failed: {}",
        if !stderr.is_empty() { stderr } else { stdout }
    ))
}

async fn wait_for_postgres(cfg: &DatabaseConfig) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let admin_dsn = cfg.admin_dsn();
    let mut last_error = String::new();

    while Instant::now() < deadline {
        match PgConnection::connect(&admin_dsn).await {
            Ok(_) => {
                info!("postgres is ready at {}:{}", cfg.host, cfg.port);
                return Ok(());
            }
            Err(err) => {
                last_error = err.to_string();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    Err(format!(
        "postgres did not become ready within 60s ({}:{}){}",
        cfg.host,
        cfg.port,
        if last_error.is_empty() {
            String::new()
        } else {
            format!(": {last_error}")
        }
    ))
}

async fn create_database_if_missing(cfg: &DatabaseConfig) -> Result<(), String> {
    let mut conn = PgConnection::connect(&cfg.admin_dsn())
        .await
        .map_err(|err| format!("failed to connect to postgres admin database: {err}"))?;

    let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(&cfg.dbname)
        .fetch_optional(&mut conn)
        .await
        .map_err(|err| format!("failed to check database existence: {err}"))?
        .is_some();

    if exists {
        info!("postgres database {} already exists", cfg.dbname);
        return Ok(());
    }

    let create_sql = format!("CREATE DATABASE \"{}\"", cfg.dbname.replace('"', "\"\""));
    sqlx::query(&create_sql)
        .execute(&mut conn)
        .await
        .map_err(|err| format!("failed to create database {}: {err}", cfg.dbname))?;
    info!("created postgres database {}", cfg.dbname);
    Ok(())
}
