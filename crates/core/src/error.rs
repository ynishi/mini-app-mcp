/// Application-level error type for mini-app-mcp.
///
/// All public functions return `Result<T, MiniAppError>`. This enum is the
/// single error type shared across schema parsing, storage, validation, and
/// configuration layers.
///
/// # Crux compliance
/// Every variant maps to a unique `code` string constant (e.g.
/// `"VALIDATION_ERROR"`) so that downstream MCP transport layers can build a
/// structured JSON `data` object — satisfying the "structured JSON error" Crux
/// constraint. The actual `rmcp::ErrorData` conversion lives in the mcp crate
/// (`crates/mcp/src/error_conv.rs`) as a `pub(crate)` free function (ACL
/// adapter, Outline rust book §5-1-10 K-orphan-rule).
use serde::Serialize;
use thiserror::Error;

/// A single occurrence of `old_str` found inside a string field, used to
/// populate the `candidates` array in an `AMBIGUOUS_MATCH` error response.
#[derive(Debug, Clone, Serialize)]
pub struct MatchCandidate {
    /// 1-indexed line number where the match starts.
    pub line: u32,
    /// 1-indexed column (byte offset within the line + 1) where the match starts.
    pub col: u32,
    /// Short surrounding context (~30 chars) for display in error responses.
    pub snippet: String,
}

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
    /// Returned when the `query_aggregate` tool receives a structurally
    /// inconsistent request (empty sources, ATTACH-limit exceeded,
    /// inner-without-group-by, etc.) — distinct from per-field validation
    /// errors (those use `VALIDATION_ERROR`) and from raw SQLite failures
    /// (those use `STORAGE_ERROR`).
    pub const AGGREGATOR_ERROR: &str = "AGGREGATOR_ERROR";
    /// Returned when a partial-edit tool (`content_view` / `content_replace` /
    /// `content_insert`) targets a field whose schema type is not `String`.
    pub const TYPE_ERROR: &str = "TYPE_ERROR";
    /// Returned when `content_replace` finds zero occurrences of `old_str` in
    /// the target field (or within the scoped `view_range`).
    pub const STRING_NOT_FOUND: &str = "STRING_NOT_FOUND";
    /// Returned when `content_replace` finds two or more occurrences of
    /// `old_str` and `replace_all` was not set — caller must scope with
    /// `view_range` or pass `replace_all=true`.
    pub const AMBIGUOUS_MATCH: &str = "AMBIGUOUS_MATCH";
    /// Returned when `content_insert` receives a `line` value that exceeds
    /// `total_lines + 1`.
    pub const OUT_OF_RANGE: &str = "OUT_OF_RANGE";
    /// Returned when `data_snapshot` is called with `upload=true` but the
    /// server binary was built without the `s3-upload` feature, or the
    /// `MINI_APP_S3_*` environment variables are incomplete.
    pub const UPLOAD_NOT_CONFIGURED: &str = "UPLOAD_NOT_CONFIGURED";
    /// Returned when an S3-compatible upload operation fails (network error,
    /// auth rejection, or local snapshot file read failure).
    pub const UPLOAD_FAILED: &str = "UPLOAD_FAILED";
}

/// All errors that can arise inside mini-app-core.
#[derive(Error, Debug)]
pub enum MiniAppError {
    /// Validation failed for a specific field.
    #[error("validation error on field '{field}': {reason}")]
    Validation { field: String, reason: String },

    /// No row with the given `id` was found.
    #[error("row not found: {id}")]
    NotFound { id: String },

    /// `schema.yaml` could not be parsed.
    #[error("schema parse error: {0}")]
    Schema(String),

    /// A SQLite storage error occurred.
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    /// A filesystem I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An environment-variable or `.env` configuration error occurred.
    #[error("config error: {0}")]
    Config(String),

    /// The requested table is not mounted in the registry.
    #[error("table not found: {table}")]
    TableNotFound { table: String },

    /// Multi-table mode requires a `table` argument that was omitted.
    #[error("table argument is required in multi-table mode")]
    TableRequired,

    /// A schema file already exists for the given table.
    #[error("schema already exists: {table}")]
    SchemaExists { table: String },

    /// A backup I/O or SQLite backup operation failed.
    #[error("backup error: {0}")]
    Backup(String),

    /// A snapshot I/O or SQLite snapshot operation failed.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// `schema_batch` was aborted because one of its ops failed.
    #[error("batch aborted at op #{op_index}: {reason}")]
    BatchAborted { op_index: usize, reason: String },

    /// The destination path supplied to `row_materialize` is not absolute.
    #[error("materialize dest must be absolute: {path}")]
    MaterializeDestRelative { path: String },

    /// The destination path is absolute but invalid for another reason.
    #[error("materialize dest invalid '{path}': {reason}")]
    MaterializeDestInvalid { path: String, reason: String },

    /// A filesystem I/O error occurred during `row_materialize`.
    #[error("materialize io error: {0}")]
    MaterializeIo(String),

    /// SHA-256 computation failed during `row_materialize`.
    #[error("materialize sha256 error: {0}")]
    MaterializeSha256(String),

    /// The row id specified in a `ById` selector was not found.
    #[error("materialize row not found: {id}")]
    MaterializeRowNotFound { id: String },

    /// A `ByFilter` selector matched zero rows and `ignore_empty` is false.
    #[error("materialize filter matched zero rows")]
    MaterializeEmptyResult,

    /// Serialization to the requested output format failed.
    #[error("materialize format error: {0}")]
    MaterializeFormatError(String),

    /// A projected field name is not present in the table schema.
    #[error("materialize unknown field: {field}")]
    MaterializeFieldUnknown { field: String },

    /// `row_materialize` parameters are structurally inconsistent.
    #[error("materialize invalid param '{field}': {reason}")]
    MaterializeInvalidParam { field: String, reason: String },

    /// No query alias with the given `name` was found in `_aliases`.
    #[error("alias not found: {name}")]
    AliasNotFound { name: String },

    /// An alias with the given `name` already exists in `_aliases`.
    #[error("alias already exists: {name}")]
    AliasAlreadyExists { name: String },

    /// `alias_run` was called without `params` but the alias requires parameter
    /// injection (its `params_schema` is non-null).
    #[error("alias '{name}' requires params but none were provided")]
    AliasParamsRequired { name: String },

    /// MiniJinja template rendering failed during `alias_run`.
    #[error("alias template render error: {0}")]
    AliasTemplateError(String),

    /// An id prefix matched more than one row.
    #[error("ambiguous id prefix '{id_prefix}': {n} candidates", n = candidates.len())]
    AmbiguousId {
        id_prefix: String,
        candidates: Vec<String>,
    },

    /// A structural inconsistency was detected in a `query_aggregate` request
    /// (empty sources, ATTACH-limit exceeded, inner-without-group-by, etc.).
    #[error("aggregator error: {0}")]
    Aggregator(String),

    /// A partial-edit tool targeted a field whose schema type is not `String`.
    #[error("field '{field}' is not a string field (type: {actual_type})")]
    FieldTypeError { field: String, actual_type: String },

    /// `content_replace` found zero occurrences of `old_str`.
    #[error("old_str not found in field '{field}'")]
    StringNotFound { field: String },

    /// `content_replace` found multiple occurrences of `old_str` and
    /// `replace_all` was not requested.
    #[error("ambiguous match: {matches} occurrences of old_str in field '{field}'")]
    AmbiguousMatch {
        field: String,
        matches: u32,
        candidates: Vec<MatchCandidate>,
    },

    /// `content_insert` received a `line` that exceeds `total_lines + 1`.
    #[error("line {line} is out of range (field has {total_lines} lines)")]
    LineOutOfRange { line: u32, total_lines: u32 },

    /// `data_snapshot` was asked to upload but the upload backend is not
    /// available (feature disabled or `MINI_APP_S3_*` env incomplete).
    #[error("upload not configured: {0}")]
    UploadNotConfigured(String),

    /// An S3-compatible upload operation failed.
    #[error("upload failed: {0}")]
    Upload(String),
}

impl MiniAppError {
    /// Returns the machine-readable error code for this variant.
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
            MiniAppError::Aggregator(_) => codes::AGGREGATOR_ERROR,
            MiniAppError::FieldTypeError { .. } => codes::TYPE_ERROR,
            MiniAppError::StringNotFound { .. } => codes::STRING_NOT_FOUND,
            MiniAppError::AmbiguousMatch { .. } => codes::AMBIGUOUS_MATCH,
            MiniAppError::LineOutOfRange { .. } => codes::OUT_OF_RANGE,
            MiniAppError::UploadNotConfigured(_) => codes::UPLOAD_NOT_CONFIGURED,
            MiniAppError::Upload(_) => codes::UPLOAD_FAILED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            (
                codes::AGGREGATOR_ERROR,
                MiniAppError::Aggregator("empty sources".into()),
            ),
            (
                codes::TYPE_ERROR,
                MiniAppError::FieldTypeError {
                    field: "count".into(),
                    actual_type: "Number".into(),
                },
            ),
            (
                codes::STRING_NOT_FOUND,
                MiniAppError::StringNotFound {
                    field: "body".into(),
                },
            ),
            (
                codes::AMBIGUOUS_MATCH,
                MiniAppError::AmbiguousMatch {
                    field: "body".into(),
                    matches: 2,
                    candidates: vec![],
                },
            ),
            (
                codes::OUT_OF_RANGE,
                MiniAppError::LineOutOfRange {
                    line: 100,
                    total_lines: 50,
                },
            ),
            (
                codes::UPLOAD_NOT_CONFIGURED,
                MiniAppError::UploadNotConfigured("missing MINI_APP_S3_BUCKET".into()),
            ),
            (
                codes::UPLOAD_FAILED,
                MiniAppError::Upload("put rejected".into()),
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

    #[test]
    fn backup_error_code_is_not_storage_or_io() {
        let err = MiniAppError::Backup("some rusqlite error".to_string());
        assert_eq!(err.code(), codes::BACKUP_ERROR);
        assert_ne!(err.code(), codes::STORAGE_ERROR);
        assert_ne!(err.code(), codes::IO_ERROR);
    }
}
