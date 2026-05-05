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
}
