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
}

impl MiniAppError {
    /// Returns the machine-readable error code for this variant.
    ///
    /// The returned value is always one of the constants in [`codes`] and is
    /// safe to embed in JSON responses for Agent-side parsing.
    pub fn code(&self) -> &'static str {
        match self {
            MiniAppError::Validation { .. } => codes::VALIDATION_ERROR,
            MiniAppError::NotFound { .. } => codes::NOT_FOUND,
            MiniAppError::Schema(_) => codes::SCHEMA_ERROR,
            MiniAppError::Storage(_) => codes::STORAGE_ERROR,
            MiniAppError::Io(_) => codes::IO_ERROR,
            MiniAppError::Config(_) => codes::CONFIG_ERROR,
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

    // T3: error path — code() returns the right constant for all 6 variants
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
        ];
        for err in errs {
            let mcp: McpError = err.into();
            assert!(
                mcp.data.is_some(),
                "data field must be Some — plain-text-only errors violate Crux #3"
            );
            let data = mcp.data.unwrap();
            // Every structured error must carry a "code" key
            assert!(
                data.get("code").is_some(),
                "data.code must be present for Agent parsing"
            );
        }
    }
}
