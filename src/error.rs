/// Application-level error type for mini-app-mcp.
///
/// All public functions return `Result<T, MiniAppError>`. This enum is the
/// single error type shared across schema parsing, storage, validation, and
/// configuration layers.
///
/// # Crux compliance
/// Every variant maps to a unique `code` string constant (e.g.
/// `"VALIDATION_ERROR"`) so that the `From<MiniAppError> for McpError`
/// conversion always produces a structured JSON `data` object — satisfying the
/// "structured JSON error" Crux constraint.
use rmcp::ErrorData as McpError;
use thiserror::Error;

/// Structured error codes emitted in the `data.code` field of every MCP error
/// response. These are `&'static str` constants so callers can pattern-match
/// them programmatically.
pub mod codes {
    /// Returned when a required field is missing or a value has the wrong type.
    pub const VALIDATION_ERROR: &str = "VALIDATION_ERROR";
    /// Returned when a requested row does not exist.
    pub const NOT_FOUND: &str = "NOT_FOUND";
    /// Returned when `schema.yaml` cannot be parsed or is structurally invalid.
    pub const SCHEMA_ERROR: &str = "SCHEMA_ERROR";
    /// Returned when a SQLite operation fails.
    pub const STORAGE_ERROR: &str = "STORAGE_ERROR";
    /// Returned when an I/O operation (file open, read) fails.
    pub const IO_ERROR: &str = "IO_ERROR";
    /// Returned when environment-variable or `.env` configuration is invalid.
    pub const CONFIG_ERROR: &str = "CONFIG_ERROR";
    /// Returned when the requested table is not mounted in the registry.
    pub const TABLE_NOT_FOUND: &str = "TABLE_NOT_FOUND";
    /// Returned when `table` argument is required but was omitted.
    ///
    /// This occurs in multi-table mode when more than one table is mounted and
    /// no default table is configured.
    pub const TABLE_REQUIRED: &str = "TABLE_REQUIRED";
    /// Returned when a schema file already exists and `schema_create` would
    /// overwrite it.
    pub const SCHEMA_EXISTS: &str = "SCHEMA_EXISTS";
    /// Returned when a backup I/O or SQLite backup operation fails.
    pub const BACKUP_ERROR: &str = "BACKUP_ERROR";
    /// Returned when `schema_batch` is aborted because one of its ops fails.
    pub const BATCH_ABORTED: &str = "BATCH_ABORTED";
    /// Returned when a snapshot I/O or SQLite snapshot operation fails.
    pub const SNAPSHOT_ERROR: &str = "SNAPSHOT_ERROR";
    /// Returned when the `row_materialize` dest path is relative (absolute required).
    pub const MATERIALIZE_DEST_RELATIVE: &str = "MATERIALIZE_DEST_RELATIVE";
    /// Returned when the `row_materialize` dest path is invalid for another reason.
    pub const MATERIALIZE_DEST_INVALID: &str = "MATERIALIZE_DEST_INVALID";
    /// Returned when a file I/O error occurs during `row_materialize`.
    pub const MATERIALIZE_IO_ERROR: &str = "MATERIALIZE_IO_ERROR";
    /// Returned when SHA-256 computation fails during `row_materialize`.
    pub const MATERIALIZE_SHA256_ERROR: &str = "MATERIALIZE_SHA256_ERROR";
    /// Returned when the specified row id is not found during `row_materialize`.
    pub const MATERIALIZE_ROW_NOT_FOUND: &str = "MATERIALIZE_ROW_NOT_FOUND";
    /// Returned when the filter in `row_materialize` matches zero rows.
    pub const MATERIALIZE_EMPTY_RESULT: &str = "MATERIALIZE_EMPTY_RESULT";
    /// Returned when serialization to the requested format fails during `row_materialize`.
    pub const MATERIALIZE_FORMAT_ERROR: &str = "MATERIALIZE_FORMAT_ERROR";
    /// Returned when a projected field name is not present in the schema.
    pub const MATERIALIZE_FIELD_UNKNOWN: &str = "MATERIALIZE_FIELD_UNKNOWN";
    /// Returned when `row_materialize` parameters are structurally invalid.
    pub const MATERIALIZE_INVALID_PARAM: &str = "MATERIALIZE_INVALID_PARAM";
    /// Returned when a named query alias does not exist in `_aliases`.
    pub const ALIAS_NOT_FOUND: &str = "ALIAS_NOT_FOUND";
    /// Returned when `alias_create` is called but an alias with the same name
    /// already exists in the table's `_aliases` storage.
    pub const ALIAS_ALREADY_EXISTS: &str = "ALIAS_ALREADY_EXISTS";
    /// Returned when `alias_run` is called without `params` but the alias has
    /// a non-null `params_schema` (i.e. the alias requires parameter injection).
    pub const ALIAS_PARAMS_REQUIRED: &str = "ALIAS_PARAMS_REQUIRED";
    /// Returned when MiniJinja template rendering fails (syntax error or
    /// missing variable) during `alias_run`.
    pub const ALIAS_TEMPLATE_ERROR: &str = "ALIAS_TEMPLATE_ERROR";
    /// Returned when an id prefix matches more than one row and the caller
    /// must disambiguate by using a longer prefix or the full UUID.
    pub const AMBIGUOUS_ID: &str = "AMBIGUOUS_ID";
}

/// All errors that can arise inside mini-app-mcp.
///
/// # Variants
/// - `Validation` — field-level validation failure (missing required field or
///   type mismatch). Carries the offending `field` name and a human-readable
///   `reason`.
/// - `NotFound` — a row with the given `id` does not exist.
/// - `Schema` — `schema.yaml` could not be parsed or is structurally invalid.
/// - `Storage` — an underlying SQLite error (auto-converted via `#[from]`).
/// - `Io` — a filesystem / I/O error (auto-converted via `#[from]`).
/// - `Config` — an environment-variable or `.env` configuration error.
/// - `TableNotFound` — the requested table is not mounted in the registry.
/// - `TableRequired` — multi-table mode requires a `table` argument that was
///   omitted.
/// - `SchemaExists` — `schema_create` was called but the schema file already
///   exists for the given table.
/// - `Backup` — a backup I/O or SQLite backup operation failed.  The inner
///   `String` unifies errors from both `rusqlite::Error` and `io::Error`
///   origins (K-79: avoids multiple `#[from]` conflict with existing
///   `Storage` and `Io` variants).
/// - `BatchAborted` — `schema_batch` was aborted because op `op_index`
///   failed with the given `reason`.
/// - `Snapshot` — a snapshot I/O or SQLite snapshot operation failed.  The
///   inner `String` unifies errors from both `rusqlite::Error` and `io::Error`
///   origins (K-79: avoids multiple `#[from]` conflict with existing
///   `Storage` and `Io` variants).
/// - `MaterializeDestRelative` — the destination path supplied to
///   `row_materialize` is not absolute.  Absolute paths are required (Agent-First
///   trust model).
/// - `MaterializeDestInvalid` — the destination path is absolute but invalid
///   for another reason (e.g. parent directory cannot be created, or the path
///   already exists as a file when `write_mode = Error`).
/// - `MaterializeIo` — a filesystem I/O error occurred during `row_materialize`.
///   The inner `String` carries the error message (K-79: avoids conflict with
///   the existing `Io` variant).
/// - `MaterializeSha256` — SHA-256 computation failed during `row_materialize`
///   (e.g. `spawn_blocking` task panicked).
/// - `MaterializeRowNotFound` — the row id specified in a `ById` selector was
///   not found.
/// - `MaterializeEmptyResult` — a `ByFilter` selector matched zero rows and
///   `ignore_empty` is false.
/// - `MaterializeFormatError` — serialization to the requested format failed.
///   The inner `String` carries the serializer error message (K-79).
/// - `MaterializeFieldUnknown` — a projected field name is not present in the
///   table schema.
/// - `MaterializeInvalidParam` — `row_materialize` parameters are structurally
///   inconsistent (e.g. `concat=true` with a single-row `ById` selector).
#[derive(Error, Debug)]
pub enum MiniAppError {
    /// Validation failed for a specific field.
    ///
    /// # Fields
    /// - `field`: name of the offending field.
    /// - `reason`: human-readable description of the failure.
    #[error("validation error on field '{field}': {reason}")]
    Validation { field: String, reason: String },

    /// No row with the given `id` was found.
    ///
    /// # Fields
    /// - `id`: the row identifier that was not found.
    #[error("row not found: {id}")]
    NotFound { id: String },

    /// `schema.yaml` could not be parsed.
    ///
    /// The inner `String` carries the original parse-error message.
    /// `serde_yaml_bw::Error` is intentionally not used as `#[from]` here so
    /// that the YAML library does not leak into the public error type.
    #[error("schema parse error: {0}")]
    Schema(String),

    /// A SQLite storage error occurred.
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    /// A filesystem I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An environment-variable or `.env` configuration error occurred.
    ///
    /// The inner `String` carries a description of what is missing or invalid.
    #[error("config error: {0}")]
    Config(String),

    /// The requested table is not mounted in the registry.
    ///
    /// Returned when a tool call specifies a `table` argument that does not
    /// correspond to any table discovered during startup.
    ///
    /// # Fields
    /// - `table`: the name of the table that was not found.
    #[error("table not found: {table}")]
    TableNotFound { table: String },

    /// Multi-table mode requires a `table` argument that was omitted.
    ///
    /// Returned when the registry contains more than one table and no default
    /// table is configured, but the caller omitted the `table` argument.
    #[error("table argument is required in multi-table mode")]
    TableRequired,

    /// A schema file already exists for the given table.
    ///
    /// Returned by `schema_create` when calling it would overwrite an existing
    /// `schema.yaml`.  Use `schema_update` to modify an existing schema.
    ///
    /// # Fields
    /// - `table`: the table name whose schema already exists.
    #[error("schema already exists: {table}")]
    SchemaExists { table: String },

    /// A backup I/O or SQLite backup operation failed.
    ///
    /// The inner `String` unifies error messages from both `rusqlite::Error`
    /// and `std::io::Error` origins.  A dedicated string-tuple variant (rather
    /// than `#[from]` conversions) is used to avoid conflict with the existing
    /// `Storage` and `Io` variants (K-79).
    #[error("backup error: {0}")]
    Backup(String),

    /// A snapshot I/O or SQLite snapshot operation failed.
    ///
    /// The inner `String` unifies error messages from both `rusqlite::Error`
    /// and `std::io::Error` origins.  A dedicated string-tuple variant (rather
    /// than `#[from]` conversions) is used to avoid conflict with the existing
    /// `Storage` and `Io` variants (K-79).
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// `schema_batch` was aborted because one of its ops failed.
    ///
    /// # Fields
    /// - `op_index`: the zero-based index of the failing op inside `ops[]`.
    /// - `reason`: human-readable description of why the op failed.
    #[error("batch aborted at op #{op_index}: {reason}")]
    BatchAborted { op_index: usize, reason: String },

    /// The destination path supplied to `row_materialize` is not absolute.
    ///
    /// Agent-First trust model: absolute paths are required; relative paths are
    /// rejected at parameter validation time.
    ///
    /// # Fields
    /// - `path`: the relative path that was rejected.
    #[error("materialize dest must be absolute: {path}")]
    MaterializeDestRelative { path: String },

    /// The destination path is absolute but invalid for another reason.
    ///
    /// Examples: the parent directory cannot be created, or the path already
    /// exists as a file when `write_mode = Error`.
    ///
    /// # Fields
    /// - `path`: the offending path.
    /// - `reason`: human-readable description of the problem.
    #[error("materialize dest invalid '{path}': {reason}")]
    MaterializeDestInvalid { path: String, reason: String },

    /// A filesystem I/O error occurred during `row_materialize`.
    ///
    /// The inner `String` unifies error messages from `std::io::Error`.
    /// A dedicated string-tuple variant (rather than `#[from]` conversion) is
    /// used to avoid conflict with the existing `Io` variant (K-79).
    #[error("materialize io error: {0}")]
    MaterializeIo(String),

    /// SHA-256 computation failed during `row_materialize`.
    ///
    /// The inner `String` carries the error detail (e.g. `spawn_blocking` panic
    /// message).  A string-tuple variant avoids `#[from]` conflicts (K-79).
    #[error("materialize sha256 error: {0}")]
    MaterializeSha256(String),

    /// The row id specified in a `ById` selector was not found.
    ///
    /// # Fields
    /// - `id`: the row identifier that was not found.
    #[error("materialize row not found: {id}")]
    MaterializeRowNotFound { id: String },

    /// A `ByFilter` selector matched zero rows and `ignore_empty` is false.
    #[error("materialize filter matched zero rows")]
    MaterializeEmptyResult,

    /// Serialization to the requested output format failed.
    ///
    /// The inner `String` carries the serializer error message.  A string-tuple
    /// variant avoids `#[from]` conflicts (K-79).
    #[error("materialize format error: {0}")]
    MaterializeFormatError(String),

    /// A projected field name is not present in the table schema.
    ///
    /// # Fields
    /// - `field`: the unknown field name.
    #[error("materialize unknown field: {field}")]
    MaterializeFieldUnknown { field: String },

    /// `row_materialize` parameters are structurally inconsistent.
    ///
    /// Examples: `concat=true` combined with a single-row `ById` selector.
    ///
    /// # Fields
    /// - `field`: the parameter name that is invalid.
    /// - `reason`: human-readable description of the inconsistency.
    #[error("materialize invalid param '{field}': {reason}")]
    MaterializeInvalidParam { field: String, reason: String },

    /// No query alias with the given `name` was found in `_aliases`.
    ///
    /// # Fields
    /// - `name`: the alias name that was not found.
    #[error("alias not found: {name}")]
    AliasNotFound { name: String },

    /// An alias with the given `name` already exists in `_aliases`.
    ///
    /// Returned by `alias_create` when the name is already registered.  Use
    /// `alias_delete` first, or choose a different name.
    ///
    /// # Fields
    /// - `name`: the duplicate alias name.
    #[error("alias already exists: {name}")]
    AliasAlreadyExists { name: String },

    /// `alias_run` was called without `params` but the alias requires parameter
    /// injection (its `params_schema` is non-null).
    ///
    /// # Fields
    /// - `name`: the alias name that requires parameters.
    #[error("alias '{name}' requires params but none were provided")]
    AliasParamsRequired { name: String },

    /// MiniJinja template rendering failed during `alias_run`.
    ///
    /// The inner `String` carries the MiniJinja error message (template syntax
    /// error, missing variable, type error, etc.).  A string-tuple variant
    /// avoids `#[from]` conflicts with other error origins (K-79).
    #[error("alias template render error: {0}")]
    AliasTemplateError(String),

    /// An id prefix matched more than one row.
    ///
    /// The caller must use a longer prefix or the full UUID to disambiguate.
    ///
    /// # Fields
    /// - `id_prefix`: the prefix that was supplied.
    /// - `candidates`: the full UUIDs of all matching rows.
    #[error("ambiguous id prefix '{id_prefix}': {n} candidates", n = candidates.len())]
    AmbiguousId {
        id_prefix: String,
        candidates: Vec<String>,
    },
}

impl MiniAppError {
    /// Returns the machine-readable error code for this variant.
    ///
    /// The returned value is always one of the constants in [`codes`] and is
    /// safe to embed in JSON responses for Agent-side parsing.
    ///
    /// # Returns
    ///
    /// A `&'static str` code constant from [`codes`].
    pub fn code(&self) -> &'static str {
        match self {
            MiniAppError::Validation { .. } => codes::VALIDATION_ERROR,
            MiniAppError::NotFound { .. } => codes::NOT_FOUND,
            MiniAppError::Schema(_) => codes::SCHEMA_ERROR,
            MiniAppError::Storage(_) => codes::STORAGE_ERROR,
            MiniAppError::Io(_) => codes::IO_ERROR,
            MiniAppError::Config(_) => codes::CONFIG_ERROR,
            MiniAppError::TableNotFound { .. } => codes::TABLE_NOT_FOUND,
            MiniAppError::TableRequired => codes::TABLE_REQUIRED,
            MiniAppError::SchemaExists { .. } => codes::SCHEMA_EXISTS,
            MiniAppError::Backup(_) => codes::BACKUP_ERROR,
            MiniAppError::Snapshot(_) => codes::SNAPSHOT_ERROR,
            MiniAppError::BatchAborted { .. } => codes::BATCH_ABORTED,
            MiniAppError::MaterializeDestRelative { .. } => codes::MATERIALIZE_DEST_RELATIVE,
            MiniAppError::MaterializeDestInvalid { .. } => codes::MATERIALIZE_DEST_INVALID,
            MiniAppError::MaterializeIo(_) => codes::MATERIALIZE_IO_ERROR,
            MiniAppError::MaterializeSha256(_) => codes::MATERIALIZE_SHA256_ERROR,
            MiniAppError::MaterializeRowNotFound { .. } => codes::MATERIALIZE_ROW_NOT_FOUND,
            MiniAppError::MaterializeEmptyResult => codes::MATERIALIZE_EMPTY_RESULT,
            MiniAppError::MaterializeFormatError(_) => codes::MATERIALIZE_FORMAT_ERROR,
            MiniAppError::MaterializeFieldUnknown { .. } => codes::MATERIALIZE_FIELD_UNKNOWN,
            MiniAppError::MaterializeInvalidParam { .. } => codes::MATERIALIZE_INVALID_PARAM,
            MiniAppError::AliasNotFound { .. } => codes::ALIAS_NOT_FOUND,
            MiniAppError::AliasAlreadyExists { .. } => codes::ALIAS_ALREADY_EXISTS,
            MiniAppError::AliasParamsRequired { .. } => codes::ALIAS_PARAMS_REQUIRED,
            MiniAppError::AliasTemplateError(_) => codes::ALIAS_TEMPLATE_ERROR,
            MiniAppError::AmbiguousId { .. } => codes::AMBIGUOUS_ID,
        }
    }
}

/// Converts a [`MiniAppError`] into an [`McpError`] (i.e. `rmcp::ErrorData`).
///
/// Every conversion produces a `data` field containing a JSON object with at
/// minimum `{ "code": "<CODE>", "message": "<human text>" }`.  This satisfies
/// the Crux "structured JSON error" constraint: no plain-text-only error path
/// exists.
///
/// Validation errors also include a `"field"` key so Agents can identify which
/// field caused the failure without parsing the message string.
///
/// `TableNotFound` errors include a `"table"` key so Agents can identify which
/// table name caused the failure.
impl From<MiniAppError> for McpError {
    fn from(e: MiniAppError) -> Self {
        let code = e.code();
        let message = e.to_string();

        let data = match &e {
            MiniAppError::Validation { field, .. } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "field": field,
                })
            }
            MiniAppError::NotFound { id } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "id": id,
                })
            }
            MiniAppError::TableNotFound { table } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "table": table,
                })
            }
            MiniAppError::SchemaExists { table } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "table": table,
                })
            }
            MiniAppError::BatchAborted { op_index, reason } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "op_index": op_index,
                    "reason": reason,
                })
            }
            MiniAppError::MaterializeDestRelative { path } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "path": path,
                })
            }
            MiniAppError::MaterializeDestInvalid { path, reason } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "path": path,
                    "reason": reason,
                })
            }
            MiniAppError::MaterializeRowNotFound { id } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "id": id,
                })
            }
            MiniAppError::MaterializeFieldUnknown { field } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "field": field,
                })
            }
            MiniAppError::MaterializeInvalidParam { field, reason } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "field": field,
                    "reason": reason,
                })
            }
            MiniAppError::AliasNotFound { name } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "name": name,
                })
            }
            MiniAppError::AliasAlreadyExists { name } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "name": name,
                })
            }
            MiniAppError::AliasParamsRequired { name } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "name": name,
                })
            }
            MiniAppError::AmbiguousId {
                id_prefix,
                candidates,
            } => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "id_prefix": id_prefix,
                    "candidates": candidates,
                })
            }
            _ => {
                serde_json::json!({
                    "code": code,
                    "message": message,
                })
            }
        };

        McpError::internal_error(message, Some(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // T1: happy-path — every variant converts to McpError with a data object
    #[test]
    fn validation_error_has_structured_data() {
        let err = MiniAppError::Validation {
            field: "title".to_string(),
            reason: "required field missing".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for Validation");
        assert_eq!(data["code"], Value::String("VALIDATION_ERROR".to_string()));
        assert_eq!(data["field"], Value::String("title".to_string()));
        assert!(data["message"].is_string());
    }

    #[test]
    fn not_found_error_has_structured_data() {
        let err = MiniAppError::NotFound {
            id: "abc-123".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for NotFound");
        assert_eq!(data["code"], Value::String("NOT_FOUND".to_string()));
        assert_eq!(data["id"], Value::String("abc-123".to_string()));
    }

    #[test]
    fn schema_error_has_structured_data() {
        let err = MiniAppError::Schema("bad yaml".to_string());
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for Schema");
        assert_eq!(data["code"], Value::String("SCHEMA_ERROR".to_string()));
    }

    #[test]
    fn config_error_has_structured_data() {
        let err = MiniAppError::Config("MINI_APP_DB not set".to_string());
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for Config");
        assert_eq!(data["code"], Value::String("CONFIG_ERROR".to_string()));
    }

    // T1: new variants — TableNotFound and TableRequired have structured data
    #[test]
    fn table_not_found_error_has_structured_data_with_table_field() {
        let err = MiniAppError::TableNotFound {
            table: "my_table".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for TableNotFound");
        assert_eq!(data["code"], Value::String("TABLE_NOT_FOUND".to_string()));
        assert_eq!(data["table"], Value::String("my_table".to_string()));
        assert!(data["message"].is_string());
    }

    #[test]
    fn table_required_error_has_structured_data() {
        let err = MiniAppError::TableRequired;
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for TableRequired");
        assert_eq!(data["code"], Value::String("TABLE_REQUIRED".to_string()));
        assert!(data["message"].is_string());
    }

    // T2: edge case — empty strings are still valid structured errors
    #[test]
    fn validation_error_empty_field_name() {
        let err = MiniAppError::Validation {
            field: String::new(),
            reason: String::new(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some even for empty field");
        assert_eq!(data["code"], "VALIDATION_ERROR");
        // field key must still be present
        assert!(data.get("field").is_some());
    }

    // T2: edge case — empty table name in TableNotFound
    #[test]
    fn table_not_found_empty_table_name() {
        let err = MiniAppError::TableNotFound {
            table: String::new(),
        };
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some even for empty table name");
        assert_eq!(data["code"], "TABLE_NOT_FOUND");
        assert!(data.get("table").is_some());
    }

    // T3: error path — code() returns the right constant for all variants
    #[test]
    fn error_code_all_variants() {
        let cases: Vec<(&str, MiniAppError)> = vec![
            (
                codes::VALIDATION_ERROR,
                MiniAppError::Validation {
                    field: "f".into(),
                    reason: "r".into(),
                },
            ),
            (codes::NOT_FOUND, MiniAppError::NotFound { id: "x".into() }),
            (codes::SCHEMA_ERROR, MiniAppError::Schema("s".into())),
            (
                codes::IO_ERROR,
                MiniAppError::Io(std::io::Error::other("e")),
            ),
            (codes::CONFIG_ERROR, MiniAppError::Config("c".into())),
            (
                codes::TABLE_NOT_FOUND,
                MiniAppError::TableNotFound { table: "t".into() },
            ),
            (codes::TABLE_REQUIRED, MiniAppError::TableRequired),
            (
                codes::SCHEMA_EXISTS,
                MiniAppError::SchemaExists {
                    table: "my_table".into(),
                },
            ),
            (
                codes::BACKUP_ERROR,
                MiniAppError::Backup("disk full".into()),
            ),
            (
                codes::SNAPSHOT_ERROR,
                MiniAppError::Snapshot("snapshot failed".into()),
            ),
            (
                codes::BATCH_ABORTED,
                MiniAppError::BatchAborted {
                    op_index: 2,
                    reason: "schema not found".into(),
                },
            ),
            (
                codes::MATERIALIZE_DEST_RELATIVE,
                MiniAppError::MaterializeDestRelative {
                    path: "relative/path".into(),
                },
            ),
            (
                codes::MATERIALIZE_DEST_INVALID,
                MiniAppError::MaterializeDestInvalid {
                    path: "/bad/path".into(),
                    reason: "parent dir not writable".into(),
                },
            ),
            (
                codes::MATERIALIZE_IO_ERROR,
                MiniAppError::MaterializeIo("write failed".into()),
            ),
            (
                codes::MATERIALIZE_SHA256_ERROR,
                MiniAppError::MaterializeSha256("task panicked".into()),
            ),
            (
                codes::MATERIALIZE_ROW_NOT_FOUND,
                MiniAppError::MaterializeRowNotFound { id: "row-1".into() },
            ),
            (
                codes::MATERIALIZE_EMPTY_RESULT,
                MiniAppError::MaterializeEmptyResult,
            ),
            (
                codes::MATERIALIZE_FORMAT_ERROR,
                MiniAppError::MaterializeFormatError("yaml error".into()),
            ),
            (
                codes::MATERIALIZE_FIELD_UNKNOWN,
                MiniAppError::MaterializeFieldUnknown {
                    field: "unknown_field".into(),
                },
            ),
            (
                codes::MATERIALIZE_INVALID_PARAM,
                MiniAppError::MaterializeInvalidParam {
                    field: "concat".into(),
                    reason: "concat=true requires ByFilter selector".into(),
                },
            ),
            (
                codes::ALIAS_NOT_FOUND,
                MiniAppError::AliasNotFound {
                    name: "my_alias".into(),
                },
            ),
            (
                codes::ALIAS_ALREADY_EXISTS,
                MiniAppError::AliasAlreadyExists {
                    name: "my_alias".into(),
                },
            ),
            (
                codes::ALIAS_PARAMS_REQUIRED,
                MiniAppError::AliasParamsRequired {
                    name: "my_alias".into(),
                },
            ),
            (
                codes::ALIAS_TEMPLATE_ERROR,
                MiniAppError::AliasTemplateError("template syntax error".into()),
            ),
            (
                codes::AMBIGUOUS_ID,
                MiniAppError::AmbiguousId {
                    id_prefix: "abc".into(),
                    candidates: vec!["abc-1".into(), "abc-2".into()],
                },
            ),
        ];
        for (expected_code, err) in cases {
            assert_eq!(
                err.code(),
                expected_code,
                "wrong code for variant containing code {}",
                expected_code
            );
        }
    }

    // T3: all variants produce Some(data) — no plain-text-only path
    #[test]
    fn all_variants_produce_some_data() {
        let errs: Vec<MiniAppError> = vec![
            MiniAppError::Validation {
                field: "f".into(),
                reason: "r".into(),
            },
            MiniAppError::NotFound { id: "id".into() },
            MiniAppError::Schema("s".into()),
            MiniAppError::Io(std::io::Error::other("io")),
            MiniAppError::Config("c".into()),
            MiniAppError::TableNotFound { table: "t".into() },
            MiniAppError::TableRequired,
            MiniAppError::SchemaExists {
                table: "tbl".into(),
            },
            MiniAppError::Backup("err".into()),
            MiniAppError::Snapshot("err".into()),
            MiniAppError::BatchAborted {
                op_index: 0,
                reason: "reason".into(),
            },
            MiniAppError::MaterializeDestRelative {
                path: "relative/path".into(),
            },
            MiniAppError::MaterializeDestInvalid {
                path: "/bad/path".into(),
                reason: "parent not writable".into(),
            },
            MiniAppError::MaterializeIo("write failed".into()),
            MiniAppError::MaterializeSha256("task panicked".into()),
            MiniAppError::MaterializeRowNotFound { id: "row-1".into() },
            MiniAppError::MaterializeEmptyResult,
            MiniAppError::MaterializeFormatError("yaml error".into()),
            MiniAppError::MaterializeFieldUnknown {
                field: "unknown_field".into(),
            },
            MiniAppError::MaterializeInvalidParam {
                field: "concat".into(),
                reason: "requires ByFilter".into(),
            },
            MiniAppError::AliasNotFound {
                name: "my_alias".into(),
            },
            MiniAppError::AliasAlreadyExists {
                name: "my_alias".into(),
            },
            MiniAppError::AliasParamsRequired {
                name: "my_alias".into(),
            },
            MiniAppError::AliasTemplateError("template syntax error".into()),
            MiniAppError::AmbiguousId {
                id_prefix: "abc".into(),
                candidates: vec!["abc-1".into(), "abc-2".into()],
            },
        ];
        for err in errs {
            let mcp: McpError = err.into();
            assert!(
                mcp.data.is_some(),
                "data field must be Some — plain-text-only errors violate Crux #3"
            );
            // SAFETY: asserted is_some() above; unwrap is safe inside test.
            let data = mcp.data.unwrap();
            // Every structured error must carry a "code" key
            assert!(
                data.get("code").is_some(),
                "data.code must be present for Agent parsing"
            );
        }
    }

    // T1: SchemaExists variant produces structured data with table field
    #[test]
    fn schema_exists_error_has_structured_data() {
        let err = MiniAppError::SchemaExists {
            table: "orders".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for SchemaExists");
        assert_eq!(data["code"], Value::String("SCHEMA_EXISTS".to_string()));
        assert_eq!(data["table"], Value::String("orders".to_string()));
        assert!(data["message"].is_string());
    }

    // T1: Backup variant produces structured data
    #[test]
    fn backup_error_has_structured_data() {
        let err = MiniAppError::Backup("disk full while writing backup".to_string());
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for Backup");
        assert_eq!(data["code"], Value::String("BACKUP_ERROR".to_string()));
        assert!(data["message"].is_string());
        // Backup uses the default arm: only code + message, no extra fields
        assert!(data.get("table").is_none());
    }

    // T1: BatchAborted variant produces structured data with op_index and reason
    #[test]
    fn batch_aborted_error_has_structured_data() {
        let err = MiniAppError::BatchAborted {
            op_index: 3,
            reason: "table not found".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for BatchAborted");
        assert_eq!(data["code"], Value::String("BATCH_ABORTED".to_string()));
        assert_eq!(data["op_index"], serde_json::json!(3_usize));
        assert_eq!(data["reason"], Value::String("table not found".to_string()));
        assert!(data["message"].is_string());
    }

    // T2: SchemaExists with empty table name still produces valid structured error
    #[test]
    fn schema_exists_empty_table_name() {
        let err = MiniAppError::SchemaExists {
            table: String::new(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], "SCHEMA_EXISTS");
        assert!(data.get("table").is_some());
    }

    // T2: BatchAborted at op_index 0 (first op fails)
    #[test]
    fn batch_aborted_at_first_op() {
        let err = MiniAppError::BatchAborted {
            op_index: 0,
            reason: "validation failed".to_string(),
        };
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], "BATCH_ABORTED");
        assert_eq!(data["op_index"], serde_json::json!(0_usize));
    }

    // T3: Backup error code is BACKUP_ERROR (not STORAGE_ERROR or IO_ERROR)
    #[test]
    fn backup_error_code_is_not_storage_or_io() {
        let err = MiniAppError::Backup("some rusqlite error".to_string());
        assert_eq!(err.code(), codes::BACKUP_ERROR);
        assert_ne!(err.code(), codes::STORAGE_ERROR);
        assert_ne!(err.code(), codes::IO_ERROR);
    }

    // --- Materialize variants: 9 individual tests ---

    // T1: MaterializeDestRelative carries the path field in structured data
    #[test]
    fn materialize_dest_relative_has_path_field() {
        let err = MiniAppError::MaterializeDestRelative {
            path: "some/relative".to_string(),
        };
        assert_eq!(err.code(), codes::MATERIALIZE_DEST_RELATIVE);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeDestRelative");
        assert_eq!(data["code"], "MATERIALIZE_DEST_RELATIVE");
        assert_eq!(data["path"], "some/relative");
        assert!(data["message"].is_string());
    }

    // T1: MaterializeDestInvalid carries path and reason fields
    #[test]
    fn materialize_dest_invalid_has_path_and_reason_fields() {
        let err = MiniAppError::MaterializeDestInvalid {
            path: "/no/such/parent".to_string(),
            reason: "parent dir not writable".to_string(),
        };
        assert_eq!(err.code(), codes::MATERIALIZE_DEST_INVALID);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeDestInvalid");
        assert_eq!(data["code"], "MATERIALIZE_DEST_INVALID");
        assert_eq!(data["path"], "/no/such/parent");
        assert_eq!(data["reason"], "parent dir not writable");
        assert!(data["message"].is_string());
    }

    // T1: MaterializeIo produces structured data with code and message
    #[test]
    fn materialize_io_error_has_structured_data() {
        let err = MiniAppError::MaterializeIo("write failed: disk full".to_string());
        assert_eq!(err.code(), codes::MATERIALIZE_IO_ERROR);
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for MaterializeIo");
        assert_eq!(data["code"], "MATERIALIZE_IO_ERROR");
        assert!(data["message"].is_string());
        // Falls through to default arm — no extra fields
        assert!(data.get("path").is_none());
    }

    // T1: MaterializeSha256 produces structured data with code and message
    #[test]
    fn materialize_sha256_error_has_structured_data() {
        let err = MiniAppError::MaterializeSha256("spawn_blocking panicked".to_string());
        assert_eq!(err.code(), codes::MATERIALIZE_SHA256_ERROR);
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for MaterializeSha256");
        assert_eq!(data["code"], "MATERIALIZE_SHA256_ERROR");
        assert!(data["message"].is_string());
    }

    // T1: MaterializeRowNotFound carries the id field
    #[test]
    fn materialize_row_not_found_has_id_field() {
        let err = MiniAppError::MaterializeRowNotFound {
            id: "row-abc".to_string(),
        };
        assert_eq!(err.code(), codes::MATERIALIZE_ROW_NOT_FOUND);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeRowNotFound");
        assert_eq!(data["code"], "MATERIALIZE_ROW_NOT_FOUND");
        assert_eq!(data["id"], "row-abc");
        assert!(data["message"].is_string());
    }

    // T2: MaterializeEmptyResult (unit variant) produces structured data
    #[test]
    fn materialize_empty_result_has_structured_data() {
        let err = MiniAppError::MaterializeEmptyResult;
        assert_eq!(err.code(), codes::MATERIALIZE_EMPTY_RESULT);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeEmptyResult");
        assert_eq!(data["code"], "MATERIALIZE_EMPTY_RESULT");
        assert!(data["message"].is_string());
    }

    // T3: MaterializeFormatError code is distinct from SCHEMA_ERROR
    #[test]
    fn materialize_format_error_has_structured_data() {
        let err = MiniAppError::MaterializeFormatError("yaml: unexpected key".to_string());
        assert_eq!(err.code(), codes::MATERIALIZE_FORMAT_ERROR);
        assert_ne!(err.code(), codes::SCHEMA_ERROR);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeFormatError");
        assert_eq!(data["code"], "MATERIALIZE_FORMAT_ERROR");
        assert!(data["message"].is_string());
    }

    // T1: MaterializeFieldUnknown carries the field name
    #[test]
    fn materialize_field_unknown_has_field_name() {
        let err = MiniAppError::MaterializeFieldUnknown {
            field: "nonexistent_col".to_string(),
        };
        assert_eq!(err.code(), codes::MATERIALIZE_FIELD_UNKNOWN);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeFieldUnknown");
        assert_eq!(data["code"], "MATERIALIZE_FIELD_UNKNOWN");
        assert_eq!(data["field"], "nonexistent_col");
        assert!(data["message"].is_string());
    }

    // T1: MaterializeInvalidParam carries field and reason
    #[test]
    fn materialize_invalid_param_has_field_and_reason() {
        let err = MiniAppError::MaterializeInvalidParam {
            field: "concat".to_string(),
            reason: "concat=true requires ByFilter selector".to_string(),
        };
        assert_eq!(err.code(), codes::MATERIALIZE_INVALID_PARAM);
        let mcp: McpError = err.into();
        let data = mcp
            .data
            .expect("data must be Some for MaterializeInvalidParam");
        assert_eq!(data["code"], "MATERIALIZE_INVALID_PARAM");
        assert_eq!(data["field"], "concat");
        assert_eq!(data["reason"], "concat=true requires ByFilter selector");
        assert!(data["message"].is_string());
    }

    // T1: AliasNotFound has code ALIAS_NOT_FOUND and carries name field
    #[test]
    fn alias_not_found_error_has_structured_data() {
        let err = MiniAppError::AliasNotFound {
            name: "recent_open".to_string(),
        };
        assert_eq!(err.code(), codes::ALIAS_NOT_FOUND);
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for AliasNotFound");
        assert_eq!(data["code"], Value::String("ALIAS_NOT_FOUND".to_string()));
        assert_eq!(data["name"], Value::String("recent_open".to_string()));
        assert!(data["message"].is_string());
    }

    // T1: AliasAlreadyExists has code ALIAS_ALREADY_EXISTS and carries name field
    #[test]
    fn alias_already_exists_error_has_structured_data() {
        let err = MiniAppError::AliasAlreadyExists {
            name: "recent_open".to_string(),
        };
        assert_eq!(err.code(), codes::ALIAS_ALREADY_EXISTS);
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for AliasAlreadyExists");
        assert_eq!(
            data["code"],
            Value::String("ALIAS_ALREADY_EXISTS".to_string())
        );
        assert_eq!(data["name"], Value::String("recent_open".to_string()));
        assert!(data["message"].is_string());
    }

    // T1: AmbiguousId has code AMBIGUOUS_ID, carries id_prefix and candidates array
    #[test]
    fn ambiguous_id_error_has_structured_data() {
        let err = MiniAppError::AmbiguousId {
            id_prefix: "abc1".to_string(),
            candidates: vec![
                "abc1def2-0000-0000-0000-000000000001".to_string(),
                "abc1def2-0000-0000-0000-000000000002".to_string(),
            ],
        };
        assert_eq!(err.code(), codes::AMBIGUOUS_ID);
        let mcp: McpError = err.into();
        let data = mcp.data.expect("data must be Some for AmbiguousId");
        assert_eq!(data["code"], Value::String("AMBIGUOUS_ID".to_string()));
        assert_eq!(data["id_prefix"], Value::String("abc1".to_string()));
        assert!(data["message"].is_string());
        // candidates must be a JSON array
        let candidates = data["candidates"]
            .as_array()
            .expect("candidates must be a JSON array");
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0],
            Value::String("abc1def2-0000-0000-0000-000000000001".to_string())
        );
        assert_eq!(
            candidates[1],
            Value::String("abc1def2-0000-0000-0000-000000000002".to_string())
        );
    }
}
