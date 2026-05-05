/// Runtime configuration for mini-app-mcp.
///
/// Configuration is read from the environment (optionally loaded from a
/// `.mini-app-mcp.env` file via [`dotenvy`]).
///
/// # Modes
///
/// **Legacy mode** (single-table): both `MINI_APP_SCHEMA` and `MINI_APP_DB`
/// must be set.  Directory scan is skipped.  The single table described by the
/// schema file is registered as the default table.
///
/// **Multi-table mode**: `MINI_APP_USER_DIR` and/or `MINI_APP_PROJECT_DIR` are
/// used to discover tables from directory layout.  The legacy env vars remain
/// available; when both legacy and dir env vars are set at the same time, the
/// legacy single-table entry is also included in the registry (duplicate table
/// names are resolved with legacy taking precedence, and a warning is logged).
use std::env;
use std::path::PathBuf;

use crate::error::MiniAppError;

/// Runtime configuration for the mini-app-mcp server.
///
/// Constructed exclusively via [`Config::load`]; there is no public constructor
/// so callers always go through the validated loading path.
///
/// # Fields
/// - `schema_path` / `db_path`: legacy single-table paths (set when
///   `MINI_APP_SCHEMA` and `MINI_APP_DB` are present).
/// - `user_dir`: base directory for User-scope tables
///   (`~/.mini-app/` by default, overridden by `MINI_APP_USER_DIR`).
/// - `project_dir`: override directory for Project-scope tables
///   (`./.mini-app/` by default, overridden by `MINI_APP_PROJECT_DIR`).
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the `schema.yaml` file that defines the table's field schema.
    ///
    /// Set via the `MINI_APP_SCHEMA` environment variable.
    /// `None` when only directory-based multi-table configuration is used.
    pub schema_path: Option<PathBuf>,

    /// Path to the SQLite database file.
    ///
    /// Set via the `MINI_APP_DB` environment variable.
    /// `None` when only directory-based multi-table configuration is used.
    pub db_path: Option<PathBuf>,

    /// Base directory for User-scope table definitions.
    ///
    /// Defaults to `~/.mini-app/` when `MINI_APP_USER_DIR` is not set.
    /// Each subdirectory `<table>/` is expected to contain `schema.yaml` and
    /// `<table>.db`.
    pub user_dir: Option<PathBuf>,

    /// Override directory for Project-scope table definitions.
    ///
    /// Defaults to `./.mini-app/` (current working directory) when
    /// `MINI_APP_PROJECT_DIR` is not set.
    /// Same subdirectory layout as `user_dir`; Project entries override
    /// User entries with the same table name (file-level swap, not
    /// field-level merge).
    pub project_dir: Option<PathBuf>,

    /// Number of backup pairs (`{table}.{ts}.yaml` + `{table}.{ts}.db`) to
    /// retain in `_backup/` before the oldest are purged.
    ///
    /// Defaults to `10` when `MINI_APP_BACKUP_RETENTION` is not set.
    /// Set via the `MINI_APP_BACKUP_RETENTION` environment variable
    /// (must be a positive integer; non-parsable values are silently ignored
    /// and the default of `10` is used).
    pub backup_retention: Option<usize>,

    /// Number of snapshot files (`{table}.{ts}.db`) to retain in `_snapshots/`
    /// before the oldest are purged.
    ///
    /// Defaults to `10` when `MINI_APP_SNAPSHOT_RETENTION` is not set.
    /// Set via the `MINI_APP_SNAPSHOT_RETENTION` environment variable
    /// (must be a positive integer; non-parsable values are silently ignored
    /// and the default of `10` is used).
    ///
    /// This field is intentionally separate from `backup_retention` to enforce
    /// the snapshot retention isolation Crux constraint: snapshot and backup
    /// lifecycles are managed independently.
    pub snapshot_retention: Option<usize>,
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// Attempts to read `.mini-app-mcp.env` from the current directory first
    /// (via [`dotenvy`]).  Variables already set in the process environment take
    /// precedence over values from the file (dotenvy's default behaviour).
    ///
    /// # Environment variables
    ///
    /// | Variable | Required | Description |
    /// |---|---|---|
    /// | `MINI_APP_SCHEMA` | Legacy mode only | Path to `schema.yaml` |
    /// | `MINI_APP_DB` | Legacy mode only | Path to the SQLite database file |
    /// | `MINI_APP_USER_DIR` | Optional | User-scope table directory (default `~/.mini-app/`) |
    /// | `MINI_APP_PROJECT_DIR` | Optional | Project-scope table directory (default `./.mini-app/`) |
    /// | `MINI_APP_BACKUP_RETENTION` | Optional | Number of backup pairs to keep (default `10`) |
    /// | `MINI_APP_SNAPSHOT_RETENTION` | Optional | Number of snapshot files to keep (default `10`) |
    ///
    /// At least one of the legacy pair (`MINI_APP_SCHEMA` + `MINI_APP_DB`) or
    /// a directory env must resolve to a usable table configuration. When
    /// neither legacy env vars nor a discoverable directory exist the server
    /// can still start (directory scan skips missing dirs with a warning).
    ///
    /// # Returns
    ///
    /// A [`Config`] on success.  The presence of legacy fields vs directory
    /// fields determines which mode the server operates in.
    ///
    /// # Errors
    ///
    /// Returns [`MiniAppError::Config`] only if an env var is present but
    /// cannot be read (which is unusual — the standard env API only fails on
    /// invalid UTF-8).  Does **not** panic.
    pub fn load() -> Result<Self, MiniAppError> {
        // Best-effort: the env file is optional.
        if let Err(e) = dotenvy::from_filename(".mini-app-mcp.env") {
            tracing::debug!(error = %e, "no .mini-app-mcp.env file found, relying on process env");
        }

        // Legacy single-table env vars — both optional at the Config level;
        // TableRegistry::mount_legacy enforces that both are present together.
        let schema_path = env::var("MINI_APP_SCHEMA").ok().map(PathBuf::from);
        let db_path = env::var("MINI_APP_DB").ok().map(PathBuf::from);

        // User-scope dir: explicit env or default to ~/.mini-app/
        let user_dir = match env::var("MINI_APP_USER_DIR") {
            Ok(v) => Some(PathBuf::from(v)),
            Err(_) => dirs::home_dir().map(|h| h.join(".mini-app")),
        };

        // Project-scope dir: explicit env or default to ./.mini-app/
        let project_dir = match env::var("MINI_APP_PROJECT_DIR") {
            Ok(v) => Some(PathBuf::from(v)),
            Err(_) => Some(PathBuf::from(".mini-app")),
        };

        // Backup retention: explicit env or default to None (caller uses 10).
        let backup_retention = env::var("MINI_APP_BACKUP_RETENTION")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());

        // Snapshot retention: explicit env or default to None (caller uses 10).
        // Intentionally separate from backup_retention (Crux: retention isolation).
        let snapshot_retention = env::var("MINI_APP_SNAPSHOT_RETENTION")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());

        Ok(Config {
            schema_path,
            db_path,
            user_dir,
            project_dir,
            backup_retention,
            snapshot_retention,
        })
    }

    /// Returns `true` when both legacy single-table env vars (`MINI_APP_SCHEMA`
    /// and `MINI_APP_DB`) are present in this config.
    ///
    /// # Returns
    ///
    /// `true` if and only if both `schema_path` and `db_path` are `Some`.
    pub fn has_legacy_env(&self) -> bool {
        self.schema_path.is_some() && self.db_path.is_some()
    }

    /// Returns the number of backup pairs to keep per table.
    ///
    /// Uses the value of `backup_retention` when set, otherwise defaults to
    /// `10`.  This default is documented in `MINI_APP_BACKUP_RETENTION`.
    ///
    /// # Returns
    ///
    /// The retention limit as a `usize`.  Always at least `1` (a value of `0`
    /// would delete all backups on every write).
    pub fn backup_retention(&self) -> usize {
        self.backup_retention.unwrap_or(10)
    }

    /// Returns the number of snapshot files to keep per table.
    ///
    /// Uses the value of `snapshot_retention` when set, otherwise defaults to
    /// `10`.  This default is documented in `MINI_APP_SNAPSHOT_RETENTION`.
    ///
    /// This getter is intentionally separate from [`backup_retention`] to
    /// enforce snapshot retention isolation: snapshot and backup lifecycles are
    /// managed via independent environment variables and independent retention
    /// counters (Crux: snapshot retention isolation).
    ///
    /// # Returns
    ///
    /// The retention limit as a `usize`.  Always at least `1` (a value of `0`
    /// would delete all snapshots on every write).
    ///
    /// [`backup_retention`]: Config::backup_retention
    pub fn snapshot_retention(&self) -> usize {
        self.snapshot_retention.unwrap_or(10)
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

    // T1: happy-path — legacy env vars set, schema_path / db_path populated
    #[test]
    fn load_success_legacy_vars() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", Some("./mini-app.db")),
                ("MINI_APP_USER_DIR", None),
                ("MINI_APP_PROJECT_DIR", None),
            ],
            || {
                let cfg = Config::load().expect("should succeed with legacy vars set");
                assert_eq!(cfg.schema_path, Some(PathBuf::from("./schema.yaml")));
                assert_eq!(cfg.db_path, Some(PathBuf::from("./mini-app.db")));
                assert!(cfg.has_legacy_env());
            },
        );
    }

    // T1: happy-path — user_dir / project_dir explicit env vars
    #[test]
    fn load_success_dir_env_vars() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", None),
                ("MINI_APP_DB", None),
                ("MINI_APP_USER_DIR", Some("/tmp/user-tables")),
                ("MINI_APP_PROJECT_DIR", Some("/tmp/project-tables")),
            ],
            || {
                let cfg = Config::load().expect("should succeed with dir vars set");
                assert_eq!(cfg.schema_path, None);
                assert_eq!(cfg.db_path, None);
                assert_eq!(cfg.user_dir, Some(PathBuf::from("/tmp/user-tables")));
                assert_eq!(cfg.project_dir, Some(PathBuf::from("/tmp/project-tables")));
                assert!(!cfg.has_legacy_env());
            },
        );
    }

    // T1: happy-path — both legacy and dir vars set simultaneously
    #[test]
    fn load_success_both_legacy_and_dir_vars() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", Some("./mini-app.db")),
                ("MINI_APP_USER_DIR", Some("/tmp/user-tables")),
                ("MINI_APP_PROJECT_DIR", Some("/tmp/project-tables")),
            ],
            || {
                let cfg = Config::load().expect("should succeed with all vars set");
                assert_eq!(cfg.schema_path, Some(PathBuf::from("./schema.yaml")));
                assert_eq!(cfg.db_path, Some(PathBuf::from("./mini-app.db")));
                assert_eq!(cfg.user_dir, Some(PathBuf::from("/tmp/user-tables")));
                assert_eq!(cfg.project_dir, Some(PathBuf::from("/tmp/project-tables")));
                assert!(cfg.has_legacy_env());
            },
        );
    }

    // T2: edge case — no env vars at all; load still succeeds (no required vars)
    #[test]
    fn load_no_env_vars_succeeds() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", None),
                ("MINI_APP_DB", None),
                ("MINI_APP_USER_DIR", None),
                ("MINI_APP_PROJECT_DIR", None),
            ],
            || {
                // Config::load() always succeeds; lack of env vars is OK —
                // the TableRegistry will handle missing dirs gracefully.
                let cfg = Config::load().expect("load must not fail with no env vars");
                assert_eq!(cfg.schema_path, None);
                assert_eq!(cfg.db_path, None);
                assert!(!cfg.has_legacy_env());
                // project_dir defaults to Some(./.mini-app/) when env is absent
                assert_eq!(cfg.project_dir, Some(PathBuf::from(".mini-app")));
            },
        );
    }

    // T2: edge case — only MINI_APP_SCHEMA set (no DB), has_legacy_env is false
    #[test]
    fn load_only_schema_env_has_legacy_env_false() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", None),
            ],
            || {
                let cfg = Config::load().expect("load must not fail");
                assert_eq!(cfg.schema_path, Some(PathBuf::from("./schema.yaml")));
                assert_eq!(cfg.db_path, None);
                assert!(!cfg.has_legacy_env());
            },
        );
    }

    // T3: error path — load does not panic with no env file
    #[test]
    fn load_does_not_panic_with_no_env_file() {
        // Even without a .mini-app-mcp.env file, load() must not panic.
        with_env(
            &[
                ("MINI_APP_SCHEMA", None),
                ("MINI_APP_DB", None),
                ("MINI_APP_USER_DIR", None),
                ("MINI_APP_PROJECT_DIR", None),
            ],
            || {
                // Must not panic — only returns Err on truly invalid env state.
                let result = Config::load();
                // With no env vars, Config::load() succeeds (empty config).
                assert!(result.is_ok());
            },
        );
    }

    // Backward-compatibility: original load_success test behaviour preserved
    #[test]
    fn load_success() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", Some("./mini-app.db")),
            ],
            || {
                let cfg = Config::load().expect("should succeed with both vars set");
                assert_eq!(cfg.schema_path, Some(PathBuf::from("./schema.yaml")));
                assert_eq!(cfg.db_path, Some(PathBuf::from("./mini-app.db")));
            },
        );
    }

    // Backward-compatibility: missing MINI_APP_SCHEMA → schema_path is None
    #[test]
    fn load_missing_schema_returns_none() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", None),
                ("MINI_APP_DB", Some("./mini-app.db")),
            ],
            || {
                let cfg =
                    Config::load().expect("load must succeed even with MINI_APP_SCHEMA absent");
                assert_eq!(cfg.schema_path, None);
                assert!(!cfg.has_legacy_env());
            },
        );
    }

    // Backward-compatibility: missing MINI_APP_DB → db_path is None
    #[test]
    fn load_missing_db_returns_none() {
        with_env(
            &[
                ("MINI_APP_SCHEMA", Some("./schema.yaml")),
                ("MINI_APP_DB", None),
            ],
            || {
                let cfg = Config::load().expect("load must succeed even with MINI_APP_DB absent");
                assert_eq!(cfg.db_path, None);
                assert!(!cfg.has_legacy_env());
            },
        );
    }

    // T1: backup_retention defaults to 10 when env var is absent
    #[test]
    fn backup_retention_defaults_to_10() {
        with_env(&[("MINI_APP_BACKUP_RETENTION", None)], || {
            let cfg = Config::load().expect("load must succeed");
            assert_eq!(cfg.backup_retention(), 10);
            assert_eq!(cfg.backup_retention, None);
        });
    }

    // T1: backup_retention reads from env var when set
    #[test]
    fn backup_retention_reads_from_env() {
        with_env(&[("MINI_APP_BACKUP_RETENTION", Some("5"))], || {
            let cfg = Config::load().expect("load must succeed");
            assert_eq!(cfg.backup_retention, Some(5));
            assert_eq!(cfg.backup_retention(), 5);
        });
    }

    // T2: backup_retention with non-parsable env var falls back to None / default 10
    #[test]
    fn backup_retention_non_parsable_env_falls_back_to_default() {
        with_env(
            &[("MINI_APP_BACKUP_RETENTION", Some("not-a-number"))],
            || {
                let cfg = Config::load().expect("load must succeed even with bad retention value");
                assert_eq!(cfg.backup_retention, None);
                assert_eq!(cfg.backup_retention(), 10);
            },
        );
    }

    // T3: backup_retention getter always returns value >= 1 even when field is None
    #[test]
    fn backup_retention_getter_never_zero() {
        let cfg = Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        };
        assert!(cfg.backup_retention() >= 1);
    }

    // T1: snapshot_retention defaults to 10 when env var is absent
    #[test]
    fn snapshot_retention_defaults_to_10() {
        with_env(&[("MINI_APP_SNAPSHOT_RETENTION", None)], || {
            let cfg = Config::load().expect("load must succeed");
            assert_eq!(cfg.snapshot_retention(), 10);
            assert_eq!(cfg.snapshot_retention, None);
        });
    }

    // T1: snapshot_retention reads from env var when set
    #[test]
    fn snapshot_retention_reads_from_env() {
        with_env(&[("MINI_APP_SNAPSHOT_RETENTION", Some("5"))], || {
            let cfg = Config::load().expect("load must succeed");
            assert_eq!(cfg.snapshot_retention, Some(5));
            assert_eq!(cfg.snapshot_retention(), 5);
        });
    }

    // T2: snapshot_retention with non-parsable env var falls back to None / default 10
    #[test]
    fn snapshot_retention_non_parsable_env_falls_back_to_default() {
        with_env(
            &[("MINI_APP_SNAPSHOT_RETENTION", Some("not-a-number"))],
            || {
                let cfg = Config::load().expect("load must succeed even with bad retention value");
                assert_eq!(cfg.snapshot_retention, None);
                assert_eq!(cfg.snapshot_retention(), 10);
            },
        );
    }

    // T3: snapshot_retention getter always returns value >= 1 even when field is None
    #[test]
    fn snapshot_retention_getter_never_zero() {
        let cfg = Config {
            schema_path: None,
            db_path: None,
            user_dir: None,
            project_dir: None,
            backup_retention: None,
            snapshot_retention: None,
        };
        assert!(cfg.snapshot_retention() >= 1);
    }

    // T3: snapshot_retention and backup_retention are independent — setting one
    // does not affect the other (retention isolation).
    #[test]
    fn snapshot_retention_independent_from_backup_retention() {
        with_env(
            &[
                ("MINI_APP_BACKUP_RETENTION", Some("3")),
                ("MINI_APP_SNAPSHOT_RETENTION", Some("7")),
            ],
            || {
                let cfg = Config::load().expect("load must succeed");
                assert_eq!(cfg.backup_retention(), 3);
                assert_eq!(cfg.snapshot_retention(), 7);
                // The two getters must return different values to confirm isolation.
                assert_ne!(cfg.backup_retention(), cfg.snapshot_retention());
            },
        );
    }
}
