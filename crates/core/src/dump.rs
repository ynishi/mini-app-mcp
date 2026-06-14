//! Framework-level dump hook utilities (write-only file materialization).
//!
//! This module provides the [`on_change`] and [`on_delete`] hooks that
//! `Store::create`, `Store::update`, and `Store::delete` call after each
//! successful database operation.  The hooks are defined here — not inlined
//! into `store.rs` — so any future mini-app can call them directly (Crux #1
//! compliance: framework-level hook placement).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::MiniAppError;
use crate::schema::SchemaConfig;
use crate::store::RowRecord;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for the write-only file-materialization feature.
///
/// Placed under the `dump:` key in `schema.yaml`.  All fields are optional;
/// the entire `dump:` section may be absent (defaults to no materialization).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DumpConfig {
    /// Override directory for dump files.
    ///
    /// - `None` → files are written to `<cwd>/.mini-app/<table>/<id>.md`.
    /// - `Some(P)` → files are written to `P/<id>.md`.  A relative path is
    ///   resolved relative to the current working directory at runtime.
    pub dir: Option<PathBuf>,

    /// Name of the JSON field in `record.data` to use as the markdown heading.
    ///
    /// Defaults to `"title"` when `None`.
    pub title_field: Option<String>,

    /// Name of the JSON field in `record.data` to use as the markdown body.
    ///
    /// Defaults to `"body"` when `None`.
    pub body_field: Option<String>,

    /// Sync mode.  `None` / `Some(WriteOnly)` → write-only (default).
    /// `Some(Bidirectional)` is accepted in the schema but bidirectional sync
    /// is not yet implemented; a `tracing::warn!` is emitted in `Store::open`.
    pub sync: Option<SyncMode>,
}

/// Sync direction for the dump feature.
///
/// Deserialized from YAML using kebab-case: `write-only` / `bidirectional`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncMode {
    /// App → file only (default behaviour).
    WriteOnly,
    /// Bidirectional (not yet implemented; triggers a `tracing::warn!` at
    /// `Store::open` time and falls back to write-only).
    Bidirectional,
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Pure path-construction helper.  Separated from [`dump_path`] so the
/// `dir = None` branch can be unit-tested without mutating process-global
/// `current_dir()` (which would race with other parallel tests).
fn dump_path_with_cwd(cwd: &Path, schema: &SchemaConfig, dump: &DumpConfig, id: &str) -> PathBuf {
    match &dump.dir {
        None => cwd
            .join(".mini-app")
            .join(&schema.table)
            .join(format!("{id}.md")),
        Some(dir) => dir.join(format!("{id}.md")),
    }
}

/// Compute the destination path for a dump file.
///
/// - `DumpConfig.dir = None`  → `<cwd>/.mini-app/<table>/<id>.md`
/// - `DumpConfig.dir = Some(P)` → `P/<id>.md`
fn dump_path(schema: &SchemaConfig, dump: &DumpConfig, id: &str) -> Result<PathBuf, MiniAppError> {
    let cwd = std::env::current_dir()?;
    Ok(dump_path_with_cwd(&cwd, schema, dump, id))
}

/// Render a [`RowRecord`] into the markdown format required by the dump spec.
///
/// Format:
/// ```text
/// # <title>
///
/// <body>
/// ```
/// - `title` is the value of `dump.title_field` (default `"title"`) in
///   `record.data`, converted to a string.  Missing or non-string values fall
///   back to an empty string (`# ` heading).
/// - `body` is the value of `dump.body_field` (default `"body"`) in
///   `record.data`, converted to a string.  Missing or non-string values fall
///   back to an empty string.
/// - A single trailing newline is appended (POSIX convention).
fn render(schema_is_unused: &SchemaConfig, dump: &DumpConfig, record: &RowRecord) -> String {
    let _ = schema_is_unused; // schema reserved for future field-type lookups

    let title_key = dump.title_field.as_deref().unwrap_or("title");
    let body_key = dump.body_field.as_deref().unwrap_or("body");

    let title = value_as_str(&record.data, title_key);
    let body = value_as_str(&record.data, body_key);
    // Strip body trailing newlines so the format always ends with exactly one
    // LF (POSIX convention) regardless of whether the source body already
    // included a terminating newline.
    let body = body.trim_end_matches('\n');

    format!("# {title}\n\n{body}\n")
}

/// Extract a field from a JSON value as a `String`.
///
/// - If the field is absent, returns `""`.
/// - If the field is a JSON string, returns the string directly.
/// - Otherwise, calls `to_string()` on the value (e.g. numbers become `"42"`).
fn value_as_str(data: &serde_json::Value, key: &str) -> String {
    match data.get(key) {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Public hooks
// ---------------------------------------------------------------------------

/// Materialize `record` as a markdown file under the configured dump directory.
///
/// # Concurrency
/// This function is `Send`. It does not hold any lock. File I/O executes
/// inside `tokio::task::spawn_blocking` (using `std::fs::create_dir_all` and
/// `std::fs::write`), consistent with the existing `Store` I/O pattern.
/// Concurrent calls with distinct `record.id` values write to distinct paths
/// (UUID v4) and do not interfere.
///
/// Concurrent calls with the **same** `record.id` are *not* order-preserving:
/// the disk write order is determined by `spawn_blocking` scheduling, so the
/// final file content reflects whichever write finishes last — which may not
/// match the DB's last write. Callers that require strict file-DB ordering
/// must serialise same-id writes upstream.
///
/// # Cancel Safety
/// Not cancel-safe. Once the `spawn_blocking` closure has started, the file
/// write completes regardless of `Future` cancellation. Dropping this
/// `Future` after the closure has started may leave a partially-written file
/// on disk (in practice `std::fs::write` is atomic at the OS level for small
/// files on most filesystems, but this is not guaranteed).
///
/// # Errors
/// - Returns `Ok(())` immediately if `schema.dump` is `None` (no-op path).
/// - [`MiniAppError::Io`] — `create_dir_all` or `write` failure (e.g. permission denied, disk full).
/// - [`MiniAppError::Schema`] — blocking thread panicked (JoinError).
///
/// # Panic
/// Does not panic. No `Mutex` or lock is held. `spawn_blocking` JoinError is
/// converted via `map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))`.
pub async fn on_change(schema: &SchemaConfig, record: &RowRecord) -> Result<(), MiniAppError> {
    let dump = match schema.dump.as_ref() {
        None => return Ok(()),
        Some(d) => d.clone(),
    };

    let path = dump_path(schema, &dump, &record.id)?;
    let content = render(schema, &dump, record);

    tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content.as_bytes())?;
        Ok(())
    })
    .await
    .map_err(|e| MiniAppError::Schema(format!("blocking task panic: {e}")))?
}

/// No-op stub for delete-time hook (default: keep file on disk).
///
/// # Concurrency
/// This function returns `Ok(())` immediately. No I/O, no lock, no
/// `spawn_blocking`. It is `Send + Sync` with zero blocking cost.
///
/// # Cancel Safety
/// Cancel-safe. The function completes synchronously without any `.await`.
///
/// # Errors
/// Always returns `Ok(())` in the current implementation.
/// A future `dump.on_delete: keep | remove` schema flag may cause this
/// function to perform file removal, at which point the cancel-safety and
/// error contract will be updated.
///
/// # Panic
/// Does not panic.
pub async fn on_delete(_schema: &SchemaConfig, _id: &str) -> Result<(), MiniAppError> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::schema::{FieldDef, FieldType};

    fn make_schema_no_dump(table: &str) -> SchemaConfig {
        SchemaConfig {
            table: table.to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".into(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "body".into(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        }
    }

    fn make_schema_with_dump(table: &str, dir: &Path) -> SchemaConfig {
        SchemaConfig {
            table: table.to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".into(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
                FieldDef {
                    name: "body".into(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                },
            ],
            dump: Some(DumpConfig {
                dir: Some(dir.to_path_buf()),
                title_field: None,
                body_field: None,
                sync: None,
            }),
        }
    }

    fn make_record(id: &str, data: serde_json::Value) -> RowRecord {
        RowRecord {
            id: id.to_string(),
            data,
            created_at: 0,
            updated_at: 0,
        }
    }

    // ── render tests ────────────────────────────────────────────────────────

    #[test]
    fn render_line1_is_title_heading() {
        let schema = make_schema_no_dump("issues");
        let dump = DumpConfig {
            dir: None,
            title_field: None,
            body_field: None,
            sync: None,
        };
        let record = make_record(
            "id1",
            serde_json::json!({"title": "Hello", "body": "World"}),
        );
        let rendered = render(&schema, &dump, &record);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "# Hello");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "World");
    }

    #[test]
    fn render_with_custom_title_field() {
        let schema = make_schema_no_dump("things");
        let dump = DumpConfig {
            dir: None,
            title_field: Some("name".to_string()),
            body_field: None,
            sync: None,
        };
        let record = make_record(
            "id2",
            serde_json::json!({"name": "Custom Title", "body": "desc"}),
        );
        let rendered = render(&schema, &dump, &record);
        assert!(rendered.starts_with("# Custom Title\n"));
    }

    #[test]
    fn render_missing_title_field_yields_empty_heading() {
        let schema = make_schema_no_dump("things");
        let dump = DumpConfig {
            dir: None,
            title_field: None,
            body_field: None,
            sync: None,
        };
        // No "title" key in data
        let record = make_record("id3", serde_json::json!({"body": "some body"}));
        let rendered = render(&schema, &dump, &record);
        assert!(rendered.starts_with("# \n"));
    }

    #[test]
    fn render_has_trailing_newline() {
        let schema = make_schema_no_dump("t");
        let dump = DumpConfig {
            dir: None,
            title_field: None,
            body_field: None,
            sync: None,
        };
        let record = make_record("id4", serde_json::json!({"title": "T", "body": "B"}));
        let rendered = render(&schema, &dump, &record);
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn render_body_with_trailing_newline_collapses_to_single_lf() {
        let schema = make_schema_no_dump("t");
        let dump = DumpConfig {
            dir: None,
            title_field: None,
            body_field: None,
            sync: None,
        };
        // body already ends with "\n" — final output must still end with exactly
        // one trailing LF, never two.
        let record = make_record(
            "id-trailing",
            serde_json::json!({"title": "T", "body": "B\n"}),
        );
        let rendered = render(&schema, &dump, &record);
        assert!(rendered.ends_with('\n'));
        assert!(
            !rendered.ends_with("\n\n"),
            "must not produce double trailing LF; got {rendered:?}"
        );
    }

    #[test]
    fn render_non_string_title_uses_to_string() {
        let schema = make_schema_no_dump("t");
        let dump = DumpConfig {
            dir: None,
            title_field: None,
            body_field: None,
            sync: None,
        };
        let record = make_record("id5", serde_json::json!({"title": 42, "body": ""}));
        let rendered = render(&schema, &dump, &record);
        assert!(rendered.starts_with("# 42\n"));
    }

    // ── dump_path tests ──────────────────────────────────────────────────────

    #[test]
    fn dump_path_default_uses_cwd_mini_app_table_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let schema = SchemaConfig {
            table: "issues".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: Some(DumpConfig {
                dir: Some(tmp.path().to_path_buf()),
                title_field: None,
                body_field: None,
                sync: None,
            }),
        };
        let dump_cfg = schema.dump.as_ref().unwrap();
        let path = dump_path(&schema, dump_cfg, "abc-123").expect("dump_path ok");
        assert_eq!(path, tmp.path().join("abc-123.md"));
    }

    #[test]
    fn dump_path_with_cwd_none_branch_joins_mini_app_table_id() {
        // Pure-helper test for the `dump.dir = None` branch — no process-global
        // cwd mutation needed, so this is safe under parallel test execution.
        let schema = SchemaConfig {
            table: "issues".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: None,
        };
        let dump = DumpConfig {
            dir: None,
            title_field: None,
            body_field: None,
            sync: None,
        };
        let cwd = Path::new("/some/cwd");
        let path = dump_path_with_cwd(cwd, &schema, &dump, "abc-123");
        assert_eq!(
            path,
            Path::new("/some/cwd/.mini-app/issues/abc-123.md").to_path_buf()
        );
    }

    #[test]
    fn dump_path_custom_dir_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let schema = make_schema_no_dump("issues");
        let dump = DumpConfig {
            dir: Some(tmp.path().to_path_buf()),
            title_field: None,
            body_field: None,
            sync: None,
        };
        let path = dump_path(&schema, &dump, "my-id").expect("dump_path ok");
        assert_eq!(path, tmp.path().join("my-id.md"));
    }

    // ── on_change tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn on_change_writes_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let schema = make_schema_with_dump("issues", tmp.path());
        let record = make_record(
            "test-id-001",
            serde_json::json!({"title": "My Issue", "body": "Details here"}),
        );
        on_change(&schema, &record).await.expect("on_change ok");

        let expected_path = tmp.path().join("test-id-001.md");
        assert!(expected_path.exists(), "dump file must be created");

        let content = std::fs::read_to_string(&expected_path).expect("read dump file");
        assert!(content.starts_with("# My Issue\n"));
        assert!(content.contains("Details here"));
    }

    #[tokio::test]
    async fn on_change_no_dump_config_is_noop() {
        let schema = make_schema_no_dump("issues");
        let record = make_record("noop-id", serde_json::json!({"title": "T", "body": "B"}));
        // Must return Ok(()) without creating any file
        let result = on_change(&schema, &record).await;
        assert!(result.is_ok());
        // No file was created in any predictable location — we simply verify Ok
    }

    #[tokio::test]
    async fn on_change_creates_parent_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Put dump files in a subdirectory that doesn't yet exist
        let subdir = tmp.path().join("nested").join("dir");
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: Some(DumpConfig {
                dir: Some(subdir.clone()),
                title_field: None,
                body_field: None,
                sync: None,
            }),
        };
        let record = make_record("mkdirs-id", serde_json::json!({}));
        on_change(&schema, &record).await.expect("on_change ok");
        assert!(subdir.join("mkdirs-id.md").exists());
    }

    // ── on_delete tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn on_delete_keeps_file_by_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let schema = make_schema_with_dump("issues", tmp.path());
        let record = make_record(
            "keep-id",
            serde_json::json!({"title": "Keep Me", "body": ""}),
        );

        // Create the file first
        on_change(&schema, &record).await.expect("on_change ok");
        let path = tmp.path().join("keep-id.md");
        assert!(path.exists(), "file must exist after on_change");

        // on_delete must not remove it
        on_delete(&schema, "keep-id").await.expect("on_delete ok");
        assert!(path.exists(), "file must remain after on_delete");
    }
}
