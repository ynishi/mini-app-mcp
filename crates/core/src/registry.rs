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
/// Each [`TableRegistry`] snapshot is immutable. The active registry held by
/// the server is replaced atomically via `ArcSwap` on `reload` tool
/// invocation; in-flight requests continue against their captured snapshot.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::alias_storage::GlobalAliasStorage;
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
    /// Global alias storage (Phase 2). `None` in legacy single-table mode
    /// where no `user_dir` / `project_dir` is available. Holds the
    /// Project + User scope `_global.db` handles, lookup precedence
    /// Project → User.
    global_aliases: Option<Arc<GlobalAliasStorage>>,
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
        // Honour the "missing dir = skip with warn" policy uniformly
        // (scan_and_mount applies the same filter internally; the
        // global storage open path also needs the existence guard so
        // create_dir_all does not fail on a read-only / inaccessible
        // parent).
        let user_dir = user_dir.filter(|p| p.exists());
        let project_dir = project_dir.filter(|p| p.exists());

        let mut entries: HashMap<String, TableEntry> = HashMap::new();

        // Open the global alias storage upfront so we can route the
        // per-scope migrations through their respective destinations
        // (User scope for user_dir-origin tables, Project scope for
        // project_dir-origin tables — preserves the "user-scope alias
        // follows the user across projects" intent).
        let global_aliases = if user_dir.is_some() || project_dir.is_some() {
            Some(Arc::new(GlobalAliasStorage::open(project_dir, user_dir)?))
        } else {
            None
        };

        // Phase 1: User scope scan + per-scope migration. The user
        // scan runs BEFORE the project scan so user-origin entries
        // are still present in `entries` at this point — that lets us
        // pull each user-origin store's connection even for tables
        // that the project scan will later override (override would
        // otherwise replace the entry and we would lose the
        // user-origin Store handle).
        if let Some(dir) = user_dir {
            scan_and_mount(dir, &mut entries).await?;
            if let Some(g) = global_aliases.as_ref() {
                migrate_per_dir_subset(g, crate::alias_storage::AliasScope::User, dir, &entries)
                    .await?;
            }
        }

        // Phase 2: Project scope scan (override layer) + Project-scope
        // migration. After this point any user-scope alias whose table
        // was overridden by a project entry is already safely written
        // to the User scope above; the project-side `_aliases` rows
        // land in the Project scope here.
        if let Some(dir) = project_dir {
            scan_and_mount(dir, &mut entries).await?;
            if let Some(g) = global_aliases.as_ref() {
                migrate_per_dir_subset(g, crate::alias_storage::AliasScope::Project, dir, &entries)
                    .await?;
            }
        }

        Ok(TableRegistry {
            entries,
            default_table: None,
            global_aliases,
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
            global_aliases: None,
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

    /// Returns the global alias storage handle if available.
    ///
    /// `Some` when this registry was built via
    /// [`TableRegistry::mount_from_dirs`] with at least one of
    /// `user_dir` / `project_dir` populated (Phase 2 multi-table mode).
    /// `None` in legacy single-table mode and in test-only
    /// [`TableRegistry::from_entries`] / [`TableRegistry::from_single`]
    /// constructors.
    pub fn global_aliases(&self) -> Option<&Arc<GlobalAliasStorage>> {
        self.global_aliases.as_ref()
    }

    /// Returns an immutable reference to the entries map.
    ///
    /// Provides read-only access to all mounted [`TableEntry`] values, keyed by
    /// table name.  This is used by `rebuild_registry()` to diff old and new
    /// registries, and by schema CRUD tools to look up an entry's `schema_path`
    /// and `store` for backup / row-count operations.
    ///
    /// **No mutation API is exposed** — the entries HashMap is always accessed
    /// via immutable reference so existing registry invariants are preserved.
    pub fn entries(&self) -> &HashMap<String, TableEntry> {
        &self.entries
    }

    /// Build a registry from a pre-constructed entry map and optional default.
    ///
    /// This constructor is intended for use in tests where stores are created
    /// directly (e.g. in-memory SQLite) without going through the directory
    /// scan path.
    ///
    /// # Arguments
    ///
    /// - `entries`: map of table names to [`TableEntry`] values.
    /// - `default_table`: optional default table name (set for legacy compat).
    pub fn from_entries(
        entries: HashMap<String, TableEntry>,
        default_table: Option<String>,
    ) -> Self {
        TableRegistry {
            entries,
            default_table,
            global_aliases: None,
        }
    }

    /// Build a single-entry registry from a pre-opened [`Store`] and schema.
    ///
    /// Sets `default_table` to `table_name` so callers can omit the `table`
    /// argument (crux #2 legacy adapter).
    ///
    /// # Arguments
    ///
    /// - `store`: the already-opened [`Store`].
    /// - `schema`: the parsed [`SchemaConfig`].
    /// - `schema_path`: filesystem path to `schema.yaml`.
    /// - `table_name`: the name to register this table under and set as default.
    pub fn from_single(
        store: Store,
        schema: SchemaConfig,
        schema_path: PathBuf,
        table_name: String,
    ) -> Self {
        let entry = TableEntry {
            store: Arc::new(store),
            schema: Arc::new(schema),
            schema_path: Arc::new(schema_path),
        };
        let mut entries = HashMap::new();
        entries.insert(table_name.clone(), entry);
        TableRegistry {
            entries,
            default_table: Some(table_name),
            global_aliases: None,
        }
    }

    /// Merge a legacy single-table configuration into an existing registry.
    ///
    /// Loads the schema from `schema_path`, opens the SQLite database at
    /// `db_path`, and inserts the resulting entry into `self`. If the table
    /// name is already present in the registry it is **replaced** (legacy
    /// env takes precedence) and a `tracing::warn!` is emitted.  Also sets
    /// `default_table` to the legacy table name so callers can omit the
    /// `table` argument (crux #2 legacy adapter).
    ///
    /// # Arguments
    ///
    /// - `registry`: the registry to merge into (consumed and returned).
    /// - `schema_path`: path to the `schema.yaml` file.
    /// - `db_path`: path to the SQLite database file.
    ///
    /// # Errors
    ///
    /// - [`MiniAppError::Io`] — if `schema_path` cannot be read.
    /// - [`MiniAppError::Schema`] — if `schema.yaml` is malformed.
    /// - [`MiniAppError::Storage`] — if the SQLite database cannot be opened.
    pub async fn mount_legacy_into(
        mut registry: TableRegistry,
        schema_path: &Path,
        db_path: &Path,
    ) -> Result<TableRegistry, MiniAppError> {
        let schema = schema::load_from_path(schema_path)?;
        let table_name = schema.table.clone();

        if registry.entries.contains_key(&table_name) {
            tracing::warn!(
                table = %table_name,
                "legacy table name conflicts with a dir-scanned table; legacy env takes precedence"
            );
        }

        let store = Store::open(db_path, schema.clone()).await?;
        let entry = TableEntry {
            store: Arc::new(store),
            schema: Arc::new(schema),
            schema_path: Arc::new(schema_path.to_path_buf()),
        };
        registry.entries.insert(table_name.clone(), entry);
        registry.default_table = Some(table_name);
        Ok(registry)
    }
}

// =============================================================================
// Private helpers
// =============================================================================

/// Migrate the legacy per-table `_aliases` rows belonging to the
/// tables that live directly under `dir` into the chosen `scope` of
/// the already-open global alias storage.
///
/// Lossless + idempotent: only the rows present in each per-table
/// `_aliases` are read, and `INSERT OR IGNORE` skips any name already
/// in the destination scope so multiple registry rebuilds are safe.
///
/// "Tables directly under `dir`" is computed by scanning `dir` for
/// immediate subdirectories whose name is also present in `entries`
/// (i.e. they were successfully mounted by the preceding
/// `scan_and_mount` pass and their `Store` handle is still in the
/// merged entries map). This is what gives Phase 2 ST3-review
/// finding #2 its fix: user-origin tables route their aliases to
/// User scope, and project-origin tables route to Project scope.
async fn migrate_per_dir_subset(
    storage: &Arc<GlobalAliasStorage>,
    scope: crate::alias_storage::AliasScope,
    dir: &Path,
    entries: &HashMap<String, TableEntry>,
) -> Result<(), MiniAppError> {
    let mut subset_names: Vec<String> = Vec::new();
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            // The exists() filter in mount_from_dirs should have
            // caught this, but the dir could disappear in the window
            // between the check and the read. Treat as "no tables to
            // migrate" rather than failing the entire mount.
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "migrate_per_dir_subset could not read dir; skipping"
            );
            return Ok(());
        }
    };
    for dir_entry in read_dir.flatten() {
        let Ok(meta) = dir_entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = dir_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if entries.contains_key(&name) {
            subset_names.push(name);
        }
    }
    let per_table: Vec<(String, Arc<std::sync::Mutex<rusqlite::Connection>>)> = subset_names
        .iter()
        .filter_map(|name| entries.get(name).map(|e| (name.clone(), e.store.conn())))
        .collect();
    if per_table.is_empty() {
        return Ok(());
    }
    let migrated = storage.migrate_from_per_table(scope, per_table).await?;
    if migrated > 0 {
        tracing::info!(
            migrated_rows = migrated,
            scope = ?scope,
            "migrated legacy per-table _aliases rows into _global_aliases"
        );
    }
    Ok(())
}

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

    // (rmcp-dependent variant→McpError conversion tests live in
    // `crates/mcp/src/error_conv.rs` to honor the one-way `mcp → core` dep
    // boundary, Outline rust book §5-1-10 K-orphan-rule.)

    // ── Phase 2 (ST1b): global alias storage integration ─────────────────

    // mount_from_dirs opens GlobalAliasStorage and exposes it via the
    // global_aliases() accessor.
    #[tokio::test]
    async fn mount_from_dirs_attaches_global_alias_storage() {
        let user_dir = TempDir::new().expect("tempdir");
        create_table_dir(
            &user_dir,
            "rows",
            "  - name: f\n    type: string\n    required: false\n",
        );
        let registry = TableRegistry::mount_from_dirs(Some(user_dir.path()), None)
            .await
            .expect("mount must succeed");
        assert!(
            registry.global_aliases().is_some(),
            "mount_from_dirs with user_dir must attach GlobalAliasStorage"
        );
    }

    // mount_from_dirs(None, None) → no global alias storage.
    #[tokio::test]
    async fn mount_from_dirs_without_any_dir_has_no_global_alias_storage() {
        let registry = TableRegistry::mount_from_dirs(None, None)
            .await
            .expect("mount must succeed even with no dirs");
        assert!(
            registry.global_aliases().is_none(),
            "mount_from_dirs(None, None) must not attach GlobalAliasStorage"
        );
    }

    // Legacy single-table mode does not own a global alias storage.
    #[tokio::test]
    async fn mount_legacy_has_no_global_alias_storage() {
        let dir = TempDir::new().expect("tempdir");
        let schema_path = dir.path().join("schema.yaml");
        std::fs::write(
            &schema_path,
            "table: notes\nfields:\n  - name: title\n    type: string\n    required: true\n",
        )
        .expect("write schema");
        let db_path = dir.path().join("notes.db");
        let registry = TableRegistry::mount_legacy(&schema_path, &db_path)
            .await
            .expect("mount_legacy must succeed");
        assert!(
            registry.global_aliases().is_none(),
            "mount_legacy must not attach GlobalAliasStorage"
        );
    }

    // Per-scope migration routing (Phase 2 review fix #2): user-origin
    // tables migrate to User scope, project-origin tables migrate to
    // Project scope — preserving the "user-scope alias follows user
    // across projects" intent.
    #[tokio::test]
    async fn mount_from_dirs_routes_per_table_aliases_to_origin_scope() {
        use crate::alias_storage::{AliasScope, LEGACY_PER_TABLE_ALIASES_SQL};

        let user_dir = TempDir::new().expect("tempdir");
        let project_dir = TempDir::new().expect("tempdir");

        // User-origin table `user_only` carrying a legacy alias.
        let user_table = create_table_dir(
            &user_dir,
            "user_only",
            "  - name: f\n    type: string\n    required: false\n",
        );
        let user_db = user_table.join("user_only.db");
        let conn = rusqlite::Connection::open(&user_db).expect("open user db");
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "from_user",
                "{}",
                Some(7i64),
                Some("user-scope alias".to_string()),
                Option::<String>::None
            ],
        )
        .unwrap();
        drop(conn);

        // Project-origin table `proj_only` carrying a legacy alias.
        let proj_table = create_table_dir(
            &project_dir,
            "proj_only",
            "  - name: f\n    type: string\n    required: false\n",
        );
        let proj_db = proj_table.join("proj_only.db");
        let conn = rusqlite::Connection::open(&proj_db).expect("open proj db");
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "from_project",
                "{}",
                Some(11i64),
                Some("project-scope alias".to_string()),
                Option::<String>::None
            ],
        )
        .unwrap();
        drop(conn);

        let registry =
            TableRegistry::mount_from_dirs(Some(user_dir.path()), Some(project_dir.path()))
                .await
                .expect("mount must succeed");
        let global = registry
            .global_aliases()
            .expect("global storage must be attached");

        // user-origin alias must land in User scope.
        let user_alias = global
            .alias_get_scope(AliasScope::User, "from_user")
            .await
            .expect("user alias_get_scope ok")
            .expect("user alias must be present in User scope");
        assert_eq!(user_alias.description.as_deref(), Some("user-scope alias"));
        // and NOT in Project scope.
        let user_in_project = global
            .alias_get_scope(AliasScope::Project, "from_user")
            .await
            .expect("project alias_get_scope ok");
        assert!(
            user_in_project.is_none(),
            "user-origin alias must NOT leak into Project scope (silent inversion fix)"
        );

        // project-origin alias must land in Project scope.
        let proj_alias = global
            .alias_get_scope(AliasScope::Project, "from_project")
            .await
            .expect("project alias_get_scope ok")
            .expect("project alias must be present in Project scope");
        assert_eq!(
            proj_alias.description.as_deref(),
            Some("project-scope alias")
        );
        // and NOT in User scope.
        let proj_in_user = global
            .alias_get_scope(AliasScope::User, "from_project")
            .await
            .expect("user alias_get_scope ok");
        assert!(
            proj_in_user.is_none(),
            "project-origin alias must NOT leak into User scope"
        );
    }

    // Same name in both scopes: user-origin → User, project-origin →
    // Project. alias_get returns Project (precedence rule), but each
    // scope independently keeps its own row so a later project-less
    // mount can still see the User entry.
    #[tokio::test]
    async fn mount_from_dirs_preserves_user_alias_when_project_overrides_table() {
        use crate::alias_storage::{AliasScope, LEGACY_PER_TABLE_ALIASES_SQL};

        let user_dir = TempDir::new().expect("tempdir");
        let project_dir = TempDir::new().expect("tempdir");

        // user_dir/foo with alias "shared" (description: "user").
        let user_table = create_table_dir(
            &user_dir,
            "foo",
            "  - name: f\n    type: string\n    required: false\n",
        );
        let user_db = user_table.join("foo.db");
        let conn = rusqlite::Connection::open(&user_db).expect("open user foo db");
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "shared",
                "{}",
                Option::<i64>::None,
                Some("user".to_string()),
                Option::<String>::None
            ],
        )
        .unwrap();
        drop(conn);

        // project_dir/foo (overrides user_dir/foo) with alias "shared" (description: "project").
        let proj_table = create_table_dir(
            &project_dir,
            "foo",
            "  - name: f\n    type: string\n    required: false\n",
        );
        let proj_db = proj_table.join("foo.db");
        let conn = rusqlite::Connection::open(&proj_db).expect("open project foo db");
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL).unwrap();
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "shared",
                "{}",
                Option::<i64>::None,
                Some("project".to_string()),
                Option::<String>::None
            ],
        )
        .unwrap();
        drop(conn);

        let registry =
            TableRegistry::mount_from_dirs(Some(user_dir.path()), Some(project_dir.path()))
                .await
                .expect("mount must succeed");
        let global = registry
            .global_aliases()
            .expect("global storage must be attached");

        // User scope row preserved with user-origin description.
        let user_row = global
            .alias_get_scope(AliasScope::User, "shared")
            .await
            .unwrap()
            .expect("user shared alias preserved");
        assert_eq!(user_row.description.as_deref(), Some("user"));

        // Project scope row has project-origin description.
        let project_row = global
            .alias_get_scope(AliasScope::Project, "shared")
            .await
            .unwrap()
            .expect("project shared alias present");
        assert_eq!(project_row.description.as_deref(), Some("project"));

        // alias_get applies the Project → User precedence rule.
        let merged = global.alias_get("shared").await.unwrap();
        assert_eq!(merged.description.as_deref(), Some("project"));
        assert_eq!(merged.scope, Some(AliasScope::Project));
    }

    // mount_from_dirs auto-migrates any pre-existing per-table _aliases
    // rows into the project-scope _global_aliases (lossless, the rows
    // become visible through alias_list with sources=Single(<table>)).
    #[tokio::test]
    async fn mount_from_dirs_auto_migrates_per_table_aliases() {
        use crate::alias_storage::LEGACY_PER_TABLE_ALIASES_SQL;

        let project_dir = TempDir::new().expect("tempdir");
        let table_dir = create_table_dir(
            &project_dir,
            "rows",
            "  - name: f\n    type: string\n    required: false\n",
        );

        // Pre-create the rows.db with a legacy _aliases row before mount
        // so the auto-migration sees something to copy.
        let db_path = table_dir.join("rows.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open per-table db");
        conn.execute_batch(LEGACY_PER_TABLE_ALIASES_SQL)
            .expect("create _aliases");
        conn.execute(
            "INSERT INTO _aliases (name, filter, default_limit, description, params_schema) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "legacy_alias",
                "{}",
                Some(10i64),
                Some("preserved".to_string()),
                Option::<String>::None
            ],
        )
        .expect("seed legacy alias");
        drop(conn);

        let registry = TableRegistry::mount_from_dirs(None, Some(project_dir.path()))
            .await
            .expect("mount must succeed");
        let global = registry
            .global_aliases()
            .expect("global storage must be attached");
        let all = global.alias_list().await.expect("alias_list");
        assert_eq!(all.len(), 1, "exactly one alias should be migrated");
        let rec = &all[0];
        assert_eq!(rec.name, "legacy_alias");
        assert!(
            matches!(&rec.sources, crate::aggregator::SourceSpec::Single(t) if t == "rows"),
            "migrated row must have sources=Single(rows), got {:?}",
            rec.sources
        );
        assert!(rec.aggregator.is_none());
        assert_eq!(rec.default_limit, Some(10));
        assert_eq!(rec.description.as_deref(), Some("preserved"));
    }
}
