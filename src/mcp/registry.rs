/// Multi-table registry for mini-app-mcp.
///
/// [`TableRegistry`] holds all mounted tables discovered from the User-scope
/// and Project-scope directories, as well as any legacy single-table
/// configuration provided via `MINI_APP_SCHEMA` / `MINI_APP_DB` environment
/// variables.
///
/// # Crux constraints enforced here
///
/// - **crux #1 (User→Project schema chain merge)**: [`TableRegistry::mount_from_dirs`]
///   always accepts both `user_dir` and `project_dir` paths. It first scans
///   `user_dir` (User scope), then scans `project_dir` (Project scope), with
///   the Project scan overriding same-named tables at the file level. Neither
///   directory can be silently skipped in the API; both are explicit parameters.
///
/// - **crux #2 (table argument API + single-table backward compat)**:
///   [`TableRegistry::mount_legacy`] registers a single table and sets
///   `default_table`. When a `default_table` is set, callers may omit the
///   `table` argument and [`TableRegistry::resolve`] returns the default store.
///
/// # Thread safety
///
/// Once built, a [`TableRegistry`] is immutable and shared via
/// `Arc<TableRegistry>`. There is no interior mutability; runtime add/remove
/// of tables is not supported.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::MiniAppError;
use crate::schema::{self, SchemaConfig};
use crate::store::Store;

// =============================================================================
// TableRegistry
// =============================================================================

/// Resolved entry for a single mounted table.
///
/// Holds the `Arc`-wrapped store, schema, and schema file path so consumers
/// can access all three without a separate map lookup per field.
pub struct TableEntry {
    /// The running store for this table.
    pub store: Arc<Store>,
    /// The parsed schema configuration.
    pub schema: Arc<SchemaConfig>,
    /// Filesystem path to `schema.yaml` (used for lazy schema resource reads).
    pub schema_path: Arc<PathBuf>,
}

/// Registry of all mounted tables for the current server instance.
///
/// Build with [`TableRegistry::mount_from_dirs`] (multi-table) or
/// [`TableRegistry::mount_legacy`] (single-table legacy mode). After
/// construction the registry is immutable.
pub struct TableRegistry {
    /// All mounted tables keyed by table name.
    entries: HashMap<String, TableEntry>,
    /// The default table name, set only in legacy single-table mode.
    default_table: Option<String>,
}

impl TableRegistry {
    /// Mount tables discovered from the User-scope and Project-scope directories.
    ///
    /// This is the **crux #1** entry point. It performs a two-phase scan:
    /// 1. Scan `user_dir` (base layer): every subdirectory `<table>/` that
    ///    contains both `schema.yaml` and `<table>.db` is mounted.
    /// 2. Scan `project_dir` (override layer): same discovery, but any table
    ///    name already present from the User scan is **replaced** (file-level
    ///    swap, not field-level merge).
    ///
    /// Either argument may be `None` (e.g. if the directory does not exist or
    /// was not configured). A non-existent directory is skipped with a
    /// `tracing::warn!`; it is not a fatal error.
    ///
    /// # Arguments
    ///
    /// - `user_dir`: path to the User-scope directory (e.g. `~/.mini-app/`).
    ///   Subdirectories represent table names.
    /// - `project_dir`: path to the Project-scope directory (e.g.
    ///   `./.mini-app/`). Overrides same-named User tables.
    ///
    /// # Returns
    ///
    /// A [`TableRegistry`] with all discovered tables mounted and
    /// `default_table = None` (no default in multi-table mode).
    ///
    /// # Errors
    ///
    /// Returns [`MiniAppError::Io`] if a directory can be opened but
    /// `read_dir` or file reads fail. Missing directories are skipped, not
    /// treated as errors.
    pub async fn mount_from_dirs(
        user_dir: Option<&Path>,
        project_dir: Option<&Path>,
    ) -> Result<Self, MiniAppError> {
        let mut entries: HashMap<String, TableEntry> = HashMap::new();

        // Phase 1: User scope (base layer)
        if let Some(dir) = user_dir {
            scan_and_mount(dir, &mut entries).await?;
        }

        // Phase 2: Project scope (override layer — same-named tables replace User)
        if let Some(dir) = project_dir {
            scan_and_mount(dir, &mut entries).await?;
        }

        Ok(TableRegistry {
            entries,
            default_table: None,
        })
    }

    /// Mount a single legacy table from explicit `schema_path` and `db_path`.
    ///
    /// This is the **crux #2** entry point. It registers the table described by
    /// `schema_path` and sets `default_table` to that table's name so callers
    /// can omit the `table` argument when using [`TableRegistry::resolve`].
    ///
    /// # Arguments
    ///
    /// - `schema_path`: path to the `schema.yaml` file.
    /// - `db_path`: path to the SQLite database file.
    ///
    /// # Returns
    ///
    /// A [`TableRegistry`] with a single table mounted and
    /// `default_table = Some(<table_name>)`.
    ///
    /// # Errors
    ///
    /// - [`MiniAppError::Io`] — if `schema_path` cannot be read.
    /// - [`MiniAppError::Schema`] — if `schema.yaml` is malformed.
    /// - [`MiniAppError::Storage`] — if the SQLite database cannot be opened.
    pub async fn mount_legacy(schema_path: &Path, db_path: &Path) -> Result<Self, MiniAppError> {
        let schema = schema::load_from_path(schema_path)?;
        let table_name = schema.table.clone();
        let store = Store::open(db_path, schema.clone()).await?;

        let entry = TableEntry {
            store: Arc::new(store),
            schema: Arc::new(schema),
            schema_path: Arc::new(schema_path.to_path_buf()),
        };

        let mut entries = HashMap::new();
        entries.insert(table_name.clone(), entry);

        Ok(TableRegistry {
            entries,
            default_table: Some(table_name),
        })
    }

    /// Resolve a table by name, falling back to `default_table` when `name` is
    /// `None`.
    ///
    /// This is the **crux #2** runtime entry point. When a single-table legacy
    /// env is set and `default_table` is `Some`, `name = None` is allowed and
    /// returns the default entry. In multi-table mode (`default_table = None`)
    /// `name` must be `Some`.
    ///
    /// # Arguments
    ///
    /// - `name`: the requested table name, or `None` to use the default.
    ///
    /// # Returns
    ///
    /// A reference to the [`TableEntry`] for the resolved table.
    ///
    /// # Errors
    ///
    /// - [`MiniAppError::TableRequired`] — `name` is `None` and no default
    ///   table is configured (multi-table mode with `table` argument omitted).
    /// - [`MiniAppError::TableNotFound`] — `name` is `Some` but the named
    ///   table is not in the registry.
    pub fn resolve(&self, name: Option<&str>) -> Result<&TableEntry, MiniAppError> {
        let key = match name {
            Some(n) => n,
            None => match &self.default_table {
                Some(d) => d.as_str(),
                None => return Err(MiniAppError::TableRequired),
            },
        };

        self.entries
            .get(key)
            .ok_or_else(|| MiniAppError::TableNotFound {
                table: key.to_string(),
            })
    }

    /// Returns the default table name, if any.
    ///
    /// `Some` when this registry was built via [`mount_legacy`] (single-table
    /// mode). `None` in multi-table mode.
    ///
    /// [`mount_legacy`]: TableRegistry::mount_legacy
    ///
    /// # Returns
    ///
    /// `Some(&str)` with the default table name, or `None`.
    pub fn default_table(&self) -> Option<&str> {
        self.default_table.as_deref()
    }

    /// Returns the number of tables currently mounted in the registry.
    ///
    /// # Returns
    ///
    /// The count of mounted tables.
    pub fn table_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns an iterator over all mounted table names.
    ///
    /// The iteration order is not guaranteed.
    ///
    /// # Returns
    ///
    /// An iterator yielding `&str` table names.
    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(|k| k.as_str())
    }
}

// =============================================================================
// Private helpers
// =============================================================================

/// Scan `dir` for table subdirectories and mount each into `entries`.
///
/// Expected layout:
/// ```text
/// <dir>/
///   <table>/
///     schema.yaml
///     <table>.db
/// ```
///
/// Subdirectories that do not contain both `schema.yaml` and `<table>.db` are
/// skipped with a `tracing::warn!`. I/O errors from `read_dir` itself propagate
/// as [`MiniAppError::Io`].
///
/// # Arguments
///
/// - `dir`: the directory to scan.
/// - `entries`: the map to insert/overwrite table entries in.
///
/// # Errors
///
/// Returns [`MiniAppError::Io`] if the directory cannot be read.
async fn scan_and_mount(
    dir: &Path,
    entries: &mut HashMap<String, TableEntry>,
) -> Result<(), MiniAppError> {
    if !dir.exists() {
        tracing::warn!(
            dir = %dir.display(),
            "directory does not exist, skipping"
        );
        return Ok(());
    }

    let read_dir = std::fs::read_dir(dir)?;

    for dir_entry_result in read_dir {
        let dir_entry = dir_entry_result?;
        let metadata = dir_entry.metadata()?;

        if !metadata.is_dir() {
            continue;
        }

        let table_dir = dir_entry.path();
        let table_name = match table_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => {
                tracing::warn!(
                    path = %table_dir.display(),
                    "skipping subdirectory with non-UTF-8 name"
                );
                continue;
            }
        };

        let schema_path = table_dir.join("schema.yaml");
        let db_path = table_dir.join(format!("{table_name}.db"));

        if !schema_path.exists() {
            tracing::warn!(
                table = %table_name,
                path = %schema_path.display(),
                "skipping table: schema.yaml not found"
            );
            continue;
        }

        if !db_path.exists() {
            // db file doesn't exist yet — Store::open will create it.
            // This is intentional: we allow the db to be absent on first run.
            tracing::debug!(
                table = %table_name,
                path = %db_path.display(),
                "db file absent, will be created by Store::open"
            );
        }

        let schema = match schema::load_from_path(&schema_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    table = %table_name,
                    error = %e,
                    "skipping table: failed to parse schema.yaml"
                );
                continue;
            }
        };

        let store = match Store::open(&db_path, schema.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    table = %table_name,
                    error = %e,
                    "skipping table: failed to open store"
                );
                continue;
            }
        };

        tracing::debug!(
            table = %table_name,
            schema_path = %schema_path.display(),
            db_path = %db_path.display(),
            "mounted table"
        );

        entries.insert(
            table_name,
            TableEntry {
                store: Arc::new(store),
                schema: Arc::new(schema),
                schema_path: Arc::new(schema_path),
            },
        );
    }

    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // Helper: create a table subdirectory with a minimal schema.yaml.
    // Returns (table_dir, schema_path).  No .db file is created; Store::open
    // will create it.
    fn create_table_dir(parent: &TempDir, table_name: &str, fields_yaml: &str) -> PathBuf {
        let table_dir = parent.path().join(table_name);
        std::fs::create_dir_all(&table_dir).expect("create table dir");
        let schema_path = table_dir.join("schema.yaml");
        let yaml = format!("table: {table_name}\nfields:\n{fields_yaml}\n");
        let mut f = std::fs::File::create(&schema_path).expect("create schema.yaml");
        f.write_all(yaml.as_bytes()).expect("write schema.yaml");
        table_dir
    }

    // ── T1: happy-path tests ──────────────────────────────────────────────

    // T1: User scope only — 2 tables are mounted
    #[tokio::test]
    async fn user_scope_only_mounts_two_tables() {
        let user_dir = TempDir::new().expect("tempdir");
        create_table_dir(
            &user_dir,
            "notes",
            "  - name: title\n    type: string\n    required: true\n",
        );
        create_table_dir(
            &user_dir,
            "tasks",
            "  - name: body\n    type: string\n    required: false\n",
        );

        let registry = TableRegistry::mount_from_dirs(Some(user_dir.path()), None)
            .await
            .expect("mount must succeed");

        assert_eq!(registry.table_count(), 2);
        assert!(registry.resolve(Some("notes")).is_ok());
        assert!(registry.resolve(Some("tasks")).is_ok());
        assert_eq!(registry.default_table(), None);
    }

    // T1: User + Project scopes — A (User only), B (Project override), C (Project only)
    #[tokio::test]
    async fn user_and_project_scopes_merge_with_project_override() {
        let user_dir = TempDir::new().expect("tempdir");
        let project_dir = TempDir::new().expect("tempdir");

        // User: table_a and table_b
        create_table_dir(
            &user_dir,
            "table_a",
            "  - name: f\n    type: string\n    required: false\n",
        );
        create_table_dir(
            &user_dir,
            "table_b",
            "  - name: user_field\n    type: string\n    required: false\n",
        );

        // Project: table_b (override) and table_c
        create_table_dir(
            &project_dir,
            "table_b",
            "  - name: project_field\n    type: string\n    required: true\n",
        );
        create_table_dir(
            &project_dir,
            "table_c",
            "  - name: g\n    type: number\n    required: false\n",
        );

        let registry =
            TableRegistry::mount_from_dirs(Some(user_dir.path()), Some(project_dir.path()))
                .await
                .expect("mount must succeed");

        assert_eq!(registry.table_count(), 3);

        // table_a comes from User
        let entry_a = registry
            .resolve(Some("table_a"))
            .expect("table_a must exist");
        assert_eq!(entry_a.schema.table, "table_a");

        // table_b comes from Project (overrides User)
        let entry_b = registry
            .resolve(Some("table_b"))
            .expect("table_b must exist");
        // Project's schema has "project_field" as required=true; User had "user_field"
        assert!(
            entry_b
                .schema
                .fields
                .iter()
                .any(|f| f.name == "project_field"),
            "table_b should use Project schema (project_field), not User schema (user_field)"
        );
        assert!(
            !entry_b.schema.fields.iter().any(|f| f.name == "user_field"),
            "table_b must not retain User's user_field after Project override"
        );

        // table_c comes from Project only
        assert!(registry.resolve(Some("table_c")).is_ok());
    }

    // T1: Project override is file-level swap (not field merge)
    #[tokio::test]
    async fn project_override_is_file_level_swap_not_field_merge() {
        let user_dir = TempDir::new().expect("tempdir");
        let project_dir = TempDir::new().expect("tempdir");

        // User: same_table with field_a + field_b
        create_table_dir(
            &user_dir,
            "same_table",
            "  - name: field_a\n    type: string\n    required: false\n  - name: field_b\n    type: string\n    required: false\n",
        );
        // Project: same_table with only field_c
        create_table_dir(
            &project_dir,
            "same_table",
            "  - name: field_c\n    type: number\n    required: true\n",
        );

        let registry =
            TableRegistry::mount_from_dirs(Some(user_dir.path()), Some(project_dir.path()))
                .await
                .expect("mount must succeed");

        let entry = registry
            .resolve(Some("same_table"))
            .expect("same_table must exist");
        // Only Project fields — User fields must not appear
        assert_eq!(entry.schema.fields.len(), 1);
        assert_eq!(entry.schema.fields[0].name, "field_c");
    }

    // T1: legacy env mode — 1 table + default_table set
    #[tokio::test]
    async fn legacy_mode_mounts_one_table_with_default() {
        let dir = TempDir::new().expect("tempdir");
        let schema_path = dir.path().join("schema.yaml");
        let db_path = dir.path().join("legacy.db");

        let yaml =
            "table: legacy_table\nfields:\n  - name: title\n    type: string\n    required: true\n";
        std::fs::write(&schema_path, yaml).expect("write schema.yaml");

        let registry = TableRegistry::mount_legacy(&schema_path, &db_path)
            .await
            .expect("mount_legacy must succeed");

        assert_eq!(registry.table_count(), 1);
        assert_eq!(registry.default_table(), Some("legacy_table"));

        // Resolving with None uses the default
        let entry = registry
            .resolve(None)
            .expect("default resolve must succeed");
        assert_eq!(entry.schema.table, "legacy_table");

        // Resolving with explicit name also works
        let entry2 = registry
            .resolve(Some("legacy_table"))
            .expect("explicit resolve must succeed");
        assert_eq!(entry2.schema.table, "legacy_table");
    }

    // ── T2: boundary / edge-case tests ───────────────────────────────────

    // T2: empty user_dir + empty project_dir → 0 tables mounted
    #[tokio::test]
    async fn empty_dirs_mount_zero_tables() {
        let user_dir = TempDir::new().expect("tempdir");
        let project_dir = TempDir::new().expect("tempdir");

        let registry =
            TableRegistry::mount_from_dirs(Some(user_dir.path()), Some(project_dir.path()))
                .await
                .expect("mount must not fail for empty dirs");

        assert_eq!(registry.table_count(), 0);
    }

    // T2: both dirs are None → 0 tables, no error
    #[tokio::test]
    async fn both_dirs_none_mounts_zero_tables() {
        let registry = TableRegistry::mount_from_dirs(None, None)
            .await
            .expect("mount must not fail when both dirs are None");

        assert_eq!(registry.table_count(), 0);
    }

    // T2: non-existent dir is skipped with warn, not fatal
    #[tokio::test]
    async fn nonexistent_dir_is_skipped_not_fatal() {
        let user_dir = TempDir::new().expect("tempdir");
        create_table_dir(
            &user_dir,
            "table_a",
            "  - name: f\n    type: string\n    required: false\n",
        );

        let nonexistent = PathBuf::from("/nonexistent/path/that/does/not/exist");
        let registry = TableRegistry::mount_from_dirs(Some(user_dir.path()), Some(&nonexistent))
            .await
            .expect("mount must succeed even when project_dir does not exist");

        // Only table_a from user_dir should be mounted
        assert_eq!(registry.table_count(), 1);
        assert!(registry.resolve(Some("table_a")).is_ok());
    }

    // T2: subdir with no schema.yaml is skipped
    #[tokio::test]
    async fn subdir_without_schema_yaml_is_skipped() {
        let user_dir = TempDir::new().expect("tempdir");
        // Create a subdir but no schema.yaml
        std::fs::create_dir(user_dir.path().join("no_schema")).expect("create dir");
        // Also create a valid table
        create_table_dir(
            &user_dir,
            "valid_table",
            "  - name: f\n    type: string\n    required: false\n",
        );

        let registry = TableRegistry::mount_from_dirs(Some(user_dir.path()), None)
            .await
            .expect("mount must succeed");

        assert_eq!(registry.table_count(), 1);
        assert!(registry.resolve(Some("valid_table")).is_ok());
    }

    // ── T3: error-path tests ─────────────────────────────────────────────

    // T3: resolve with None and no default → TableRequired
    #[tokio::test]
    async fn resolve_none_without_default_returns_table_required() {
        let user_dir = TempDir::new().expect("tempdir");
        create_table_dir(
            &user_dir,
            "table_a",
            "  - name: f\n    type: string\n    required: false\n",
        );
        create_table_dir(
            &user_dir,
            "table_b",
            "  - name: g\n    type: string\n    required: false\n",
        );

        let registry = TableRegistry::mount_from_dirs(Some(user_dir.path()), None)
            .await
            .expect("mount must succeed");

        let result = registry.resolve(None);
        assert!(
            result.is_err(),
            "resolve(None) must fail with no default table"
        );
        // SAFETY: we just asserted is_err() so this will not panic
        if let Err(err) = result {
            assert!(
                matches!(err, MiniAppError::TableRequired),
                "expected TableRequired, got: {err:?}"
            );
        }
    }

    // T3: resolve unknown table name → TableNotFound with correct table name
    #[tokio::test]
    async fn resolve_unknown_table_returns_table_not_found() {
        let user_dir = TempDir::new().expect("tempdir");
        create_table_dir(
            &user_dir,
            "table_a",
            "  - name: f\n    type: string\n    required: false\n",
        );

        let registry = TableRegistry::mount_from_dirs(Some(user_dir.path()), None)
            .await
            .expect("mount must succeed");

        let result = registry.resolve(Some("nonexistent"));
        assert!(result.is_err(), "resolve(nonexistent) must fail");
        // SAFETY: we just asserted is_err() so this will not panic
        if let Err(err) = result {
            match err {
                MiniAppError::TableNotFound { table } => {
                    assert_eq!(table, "nonexistent");
                }
                other => panic!("expected TableNotFound, got: {other:?}"),
            }
        }
    }

    // T3: MiniAppError::TableNotFound converts to McpError with table field
    #[test]
    fn table_not_found_converts_to_mcp_error_with_table_field() {
        use rmcp::ErrorData as McpError;
        let err = MiniAppError::TableNotFound {
            table: "my_table".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], "TABLE_NOT_FOUND");
        assert_eq!(data["table"], "my_table");
    }

    // T3: MiniAppError::TableRequired converts to McpError with TABLE_REQUIRED code
    #[test]
    fn table_required_converts_to_mcp_error_with_code() {
        use rmcp::ErrorData as McpError;
        let err = MiniAppError::TableRequired;
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], "TABLE_REQUIRED");
    }
}
