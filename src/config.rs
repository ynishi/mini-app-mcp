/// Runtime configuration for mini-app-mcp.
///
/// Configuration is read from the environment (optionally loaded from a
/// `.mini-app-mcp.env` file via [`dotenvy`]).  Both required variables must be
/// present; missing either returns [`crate::error::MiniAppError::Config`].
use std::env;
use std::path::PathBuf;

use crate::error::MiniAppError;

/// Runtime configuration for the mini-app-mcp server.
///
/// Constructed exclusively via [`Config::load`]; there is no public constructor
/// so callers always go through the validated loading path.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the `schema.yaml` file that defines the table's field schema.
    ///
    /// Set via the `MINI_APP_SCHEMA` environment variable.
    pub schema_path: PathBuf,

    /// Path to the SQLite database file.
    ///
    /// Set via the `MINI_APP_DB` environment variable.
    pub db_path: PathBuf,
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// Attempts to read `.mini-app-mcp.env` from the current directory first
    /// (via [`dotenvy`]).  Variables already set in the process environment take
    /// precedence over values from the file (dotenvy's default behaviour).
    ///
    /// # Required environment variables
    ///
    /// | Variable | Description |
    /// |---|---|
    /// | `MINI_APP_SCHEMA` | Path to `schema.yaml` |
    /// | `MINI_APP_DB` | Path to the SQLite database file |
    ///
    /// # Errors
    ///
    /// Returns [`MiniAppError::Config`] if either required variable is absent.
    /// Does **not** panic.
    pub fn load() -> Result<Self, MiniAppError> {
        // Best-effort: the env file is optional. Log at debug level so the
        // absence is traceable without being noisy in normal operation.
        if let Err(e) = dotenvy::from_filename(".mini-app-mcp.env") {
            tracing::debug!(error = %e, "no .mini-app-mcp.env file found, relying on process env");
        }

        let schema_path = env::var("MINI_APP_SCHEMA")
            .map(PathBuf::from)
            .map_err(|_| MiniAppError::Config("MINI_APP_SCHEMA env var required".into()))?;

        let db_path = env::var("MINI_APP_DB")
            .map(PathBuf::from)
            .map_err(|_| MiniAppError::Config("MINI_APP_DB env var required".into()))?;

        Ok(Config {
            schema_path,
            db_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // Serialises all config tests that mutate env vars so they don't race with
    // each other in a multi-threaded test runner.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // Helper: lock ENV_LOCK, temporarily set env vars, run the closure, then
    // restore.  All config tests must use this helper.
    // SAFETY: env var mutation is inherently racy; the lock ensures only one
    // test is mutating the environment at a time.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");

        // Save originals
        let saved: Vec<(&str, Option<String>)> =
            vars.iter().map(|(k, _)| (*k, env::var(*k).ok())).collect();

        // Apply
        for (k, v) in vars {
            match v {
                Some(val) => unsafe { env::set_var(k, val) },
                None => unsafe { env::remove_var(k) },
            }
        }

        f();

        // Restore
        for (k, original) in &saved {
            match original {
                Some(val) => unsafe { env::set_var(k, val) },
                None => unsafe { env::remove_var(k) },
            }
        }
    }

    #[test]
    fn load_success() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", Some("./mini-app.db")),
            ],
            || {
                let cfg = Config::load().expect("should succeed with both vars set");
                assert_eq!(cfg.schema_path, PathBuf::from("./schema.yaml"));
                assert_eq!(cfg.db_path, PathBuf::from("./mini-app.db"));
            },
        );
    }

    #[test]
    fn load_missing_schema_returns_config_error() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", None),
                ("MINI_APP_DB", Some("./mini-app.db")),
            ],
            || {
                let err = Config::load().expect_err("should fail when MINI_APP_SCHEMA is absent");
                assert!(
                    matches!(err, MiniAppError::Config(_)),
                    "expected Config error, got: {err:?}"
                );
                if let MiniAppError::Config(msg) = err {
                    assert!(
                        msg.contains("MINI_APP_SCHEMA"),
                        "error message should mention the missing variable"
                    );
                }
            },
        );
    }

    #[test]
    fn load_missing_db_returns_config_error() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", None),
            ],
            || {
                let err = Config::load().expect_err("should fail when MINI_APP_DB is absent");
                assert!(
                    matches!(err, MiniAppError::Config(_)),
                    "expected Config error, got: {err:?}"
                );
                if let MiniAppError::Config(msg) = err {
                    assert!(
                        msg.contains("MINI_APP_DB"),
                        "error message should mention the missing variable"
                    );
                }
            },
        );
    }

    #[test]
    fn load_does_not_panic_with_no_env_file() {
        // Even without a .mini-app-mcp.env file, load() must not panic.
        // It will return Err if vars are absent, but never panic.
        with_env(&[("MINI_APP_SCHEMA", None), ("MINI_APP_DB", None)], || {
            let result = Config::load();
            assert!(result.is_err());
        });
    }
}
