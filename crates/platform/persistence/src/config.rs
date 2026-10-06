use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

/// TLS mode for the Postgres connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    /// Plaintext; local development only.
    #[default]
    Disable,
    /// TLS with certificate verification against the system trust store.
    Require,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    /// Set only via the environment (`SANVI__DATABASE__PASSWORD`), never in
    /// committed files. Never appears in `Debug` or `Serialize` output —
    /// only `skip_serializing` here, so it still deserializes normally.
    #[serde(default, skip_serializing)]
    pub password: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub max_lifetime_secs: u64,
    /// Server-side cap on any one statement, applied as a per-connection
    /// startup parameter (TASK-021, FR-1110). A query that exceeds it is
    /// cancelled by Postgres and surfaces through
    /// [`crate::is_statement_timeout`] as the `request-timeout` problem, so a
    /// request outside its budget fails loudly instead of holding a pool
    /// connection. `0` disables the cap — the migration paths use that, since
    /// DDL against scale data legitimately outlives any request-shaped
    /// budget.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,
    /// `application_name` the pool's connections present to Postgres. Distinct
    /// per process (`sanvi-api` / `sanvi-worker`) so `pg_stat_activity`
    /// attributes connections — and pool demand — to the process holding them
    /// (TASK-021 11.2f: the input the per-process pool ceilings are measured
    /// from). Absent configs keep the historical `"sanvi"`.
    #[serde(default = "default_application_name")]
    pub application_name: String,
    pub ssl_mode: SslMode,
    pub migrations_dir: String,
}

/// 10 s by default: above every lab latency budget yet below the 30 s
/// request-budget middleware, so the database cancels first and the client
/// sees the statement-timeout problem rather than the middleware's generic
/// one. Long-running jobs override it per deployment.
fn default_statement_timeout_ms() -> u64 {
    10_000
}

fn default_application_name() -> String {
    "sanvi".to_string()
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 5432,
            name: "sanvi".to_string(),
            user: "sanvi_app".to_string(),
            password: String::new(),
            max_connections: 10,
            min_connections: 1,
            acquire_timeout_secs: 10,
            idle_timeout_secs: 300,
            max_lifetime_secs: 1800,
            statement_timeout_ms: default_statement_timeout_ms(),
            application_name: default_application_name(),
            ssl_mode: SslMode::Disable,
            migrations_dir: "./migrations".to_string(),
        }
    }
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("name", &self.name)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout_secs", &self.acquire_timeout_secs)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .field("max_lifetime_secs", &self.max_lifetime_secs)
            .field("statement_timeout_ms", &self.statement_timeout_ms)
            .field("application_name", &self.application_name)
            .field("ssl_mode", &self.ssl_mode)
            .field("migrations_dir", &self.migrations_dir)
            .finish()
    }
}

impl DatabaseConfig {
    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems: Vec<String> = Vec::new();
        if self.host.trim().is_empty() {
            problems.push("database.host must not be empty".to_string());
        }
        if self.port == 0 {
            problems.push("database.port must be a valid port".to_string());
        }
        if self.name.trim().is_empty() {
            problems.push("database.name must not be empty".to_string());
        }
        if self.user.trim().is_empty() {
            problems.push("database.user must not be empty".to_string());
        }
        if self.max_connections < self.min_connections {
            problems.push("database.max_connections must be >= min_connections".to_string());
        }
        if self.max_connections == 0 {
            problems.push("database.max_connections must be >= 1".to_string());
        }
        if self.acquire_timeout_secs == 0 {
            problems.push("database.acquire_timeout_secs must be >= 1".to_string());
        }
        if self.application_name.trim().is_empty() {
            problems.push("database.application_name must not be empty".to_string());
        }
        if self.migrations_dir.trim().is_empty() {
            problems.push("database.migrations_dir must not be empty".to_string());
        }
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid database configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(())
    }

    pub fn acquire_timeout(&self) -> Duration {
        Duration::from_secs(self.acquire_timeout_secs)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }

    pub fn max_lifetime(&self) -> Duration {
        Duration::from_secs(self.max_lifetime_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        DatabaseConfig::default().validate().unwrap();
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let config = DatabaseConfig {
            password: "hunter2-ultra-secret".to_string(),
            ..DatabaseConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2-ultra-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn serialized_output_never_contains_the_password() {
        let config = DatabaseConfig {
            password: "hunter2-ultra-secret".to_string(),
            ..DatabaseConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("hunter2-ultra-secret"));
        assert!(!json.contains("\"password\""));
    }

    #[test]
    fn validation_reports_all_problems_at_once() {
        let config = DatabaseConfig {
            host: String::new(),
            port: 0,
            max_connections: 0,
            acquire_timeout_secs: 0,
            ..DatabaseConfig::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("database.host"));
        assert!(err.contains("database.port"));
        assert!(err.contains("database.max_connections"));
        assert!(err.contains("database.acquire_timeout_secs"));
    }

    #[test]
    fn statement_timeout_defaults_when_the_config_omits_it() {
        // Overlays (staging, production) carry only the keys they change; the
        // field must not become required, or every one of them breaks.
        let json = r#"{
            "host": "db",
            "port": 5432,
            "name": "sanvi",
            "user": "sanvi_app",
            "max_connections": 10,
            "min_connections": 1,
            "acquire_timeout_secs": 10,
            "idle_timeout_secs": 300,
            "max_lifetime_secs": 1800,
            "ssl_mode": "disable",
            "migrations_dir": "./migrations"
        }"#;
        let config: DatabaseConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.statement_timeout_ms, 10_000);
        // Same overlay-compatibility rule for the application name: a config
        // that predates the field (or omits it) keeps the historical value.
        assert_eq!(config.application_name, "sanvi");

        let mut disabled: serde_json::Value = serde_json::from_str(json).unwrap();
        disabled["statement_timeout_ms"] = serde_json::json!(0);
        let disabled: DatabaseConfig = serde_json::from_value(disabled).unwrap();
        assert_eq!(disabled.statement_timeout_ms, 0);
    }

    #[test]
    fn ssl_mode_parses_lowercase() {
        assert_eq!(
            serde_json::from_str::<SslMode>("\"disable\"").unwrap(),
            SslMode::Disable
        );
        assert_eq!(
            serde_json::from_str::<SslMode>("\"require\"").unwrap(),
            SslMode::Require
        );
    }
}
