//! ACL adapter: convert [`mini_app_core::MiniAppError`] into [`rmcp::ErrorData`].
//!
//! `impl From<MiniAppError> for rmcp::ErrorData` cannot live in either crate
//! because both types are foreign (Rust orphan rule, RFC 1023). This module
//! provides the conversion as a `pub(crate)` free function instead — the
//! canonical ACL adapter pattern for one-way `mcp → core` dependency layers.
//!
//! See Outline `rust` book §5-1-10 K-orphan-rule:
//! "api層の外部crate型変換はprivate fn — From impl は孤児ルールで不可"

use mini_app_core::error::MiniAppError;
use rmcp::ErrorData as McpError;

/// Convert a [`MiniAppError`] into an [`McpError`] (i.e. `rmcp::ErrorData`).
///
/// Every conversion produces a `data` field containing a JSON object with at
/// minimum `{ "code": "<CODE>", "message": "<human text>" }`. This satisfies
/// the Crux "structured JSON error" constraint: no plain-text-only error path
/// exists.
///
/// Validation errors include a `"field"` key so Agents can identify which
/// field caused the failure without parsing the message string.
///
/// `TableNotFound` errors include a `"table"` key so Agents can identify which
/// table name caused the failure.
pub(crate) fn miniapp_error_to_mcp_error(e: MiniAppError) -> McpError {
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

#[cfg(test)]
mod tests {
    use super::*;
    use mini_app_core::error::codes;
    use serde_json::Value;

    fn conv(e: MiniAppError) -> McpError {
        miniapp_error_to_mcp_error(e)
    }

    #[test]
    fn validation_error_has_structured_data() {
        let mcp = conv(MiniAppError::Validation {
            field: "title".to_string(),
            reason: "required field missing".to_string(),
        });
        let data = mcp.data.expect("data must be Some for Validation");
        assert_eq!(data["code"], Value::String("VALIDATION_ERROR".to_string()));
        assert_eq!(data["field"], Value::String("title".to_string()));
        assert!(data["message"].is_string());
    }

    #[test]
    fn not_found_error_has_structured_data() {
        let mcp = conv(MiniAppError::NotFound {
            id: "abc-123".to_string(),
        });
        let data = mcp.data.expect("data must be Some for NotFound");
        assert_eq!(data["code"], Value::String("NOT_FOUND".to_string()));
        assert_eq!(data["id"], Value::String("abc-123".to_string()));
    }

    #[test]
    fn schema_error_has_structured_data() {
        let mcp = conv(MiniAppError::Schema("bad yaml".to_string()));
        let data = mcp.data.expect("data must be Some for Schema");
        assert_eq!(data["code"], Value::String("SCHEMA_ERROR".to_string()));
    }

    #[test]
    fn config_error_has_structured_data() {
        let mcp = conv(MiniAppError::Config("MINI_APP_DB not set".to_string()));
        let data = mcp.data.expect("data must be Some for Config");
        assert_eq!(data["code"], Value::String("CONFIG_ERROR".to_string()));
    }

    #[test]
    fn table_not_found_error_has_structured_data_with_table_field() {
        let mcp = conv(MiniAppError::TableNotFound {
            table: "my_table".to_string(),
        });
        let data = mcp.data.expect("data must be Some for TableNotFound");
        assert_eq!(data["code"], Value::String("TABLE_NOT_FOUND".to_string()));
        assert_eq!(data["table"], Value::String("my_table".to_string()));
    }

    #[test]
    fn table_required_error_has_structured_data() {
        let mcp = conv(MiniAppError::TableRequired);
        let data = mcp.data.expect("data must be Some for TableRequired");
        assert_eq!(data["code"], Value::String("TABLE_REQUIRED".to_string()));
    }

    #[test]
    fn schema_exists_error_has_structured_data() {
        let mcp = conv(MiniAppError::SchemaExists {
            table: "orders".to_string(),
        });
        let data = mcp.data.expect("data must be Some for SchemaExists");
        assert_eq!(data["code"], Value::String("SCHEMA_EXISTS".to_string()));
        assert_eq!(data["table"], Value::String("orders".to_string()));
    }

    #[test]
    fn batch_aborted_error_has_structured_data() {
        let mcp = conv(MiniAppError::BatchAborted {
            op_index: 3,
            reason: "table not found".to_string(),
        });
        let data = mcp.data.expect("data must be Some for BatchAborted");
        assert_eq!(data["code"], Value::String("BATCH_ABORTED".to_string()));
        assert_eq!(data["op_index"], serde_json::json!(3_usize));
        assert_eq!(data["reason"], Value::String("table not found".to_string()));
    }

    #[test]
    fn materialize_dest_relative_has_path_field() {
        let mcp = conv(MiniAppError::MaterializeDestRelative {
            path: "some/relative".to_string(),
        });
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], "MATERIALIZE_DEST_RELATIVE");
        assert_eq!(data["path"], "some/relative");
    }

    #[test]
    fn materialize_field_unknown_has_field_name() {
        let mcp = conv(MiniAppError::MaterializeFieldUnknown {
            field: "nonexistent_col".to_string(),
        });
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], "MATERIALIZE_FIELD_UNKNOWN");
        assert_eq!(data["field"], "nonexistent_col");
    }

    #[test]
    fn alias_not_found_error_has_structured_data() {
        let mcp = conv(MiniAppError::AliasNotFound {
            name: "recent_open".to_string(),
        });
        let data = mcp.data.expect("data must be Some");
        assert_eq!(data["code"], Value::String("ALIAS_NOT_FOUND".to_string()));
        assert_eq!(data["name"], Value::String("recent_open".to_string()));
    }

    #[test]
    fn ambiguous_id_error_has_structured_data() {
        let mcp = conv(MiniAppError::AmbiguousId {
            id_prefix: "abc1".to_string(),
            candidates: vec!["abc1-1".to_string(), "abc1-2".to_string()],
        });
        let data = mcp.data.expect("data must be Some for AmbiguousId");
        assert_eq!(data["code"], Value::String("AMBIGUOUS_ID".to_string()));
        assert_eq!(data["id_prefix"], Value::String("abc1".to_string()));
        let candidates = data["candidates"].as_array().expect("candidates array");
        assert_eq!(candidates.len(), 2);
    }

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
            MiniAppError::Aggregator("empty sources".into()),
        ];
        for err in errs {
            let mcp = conv(err);
            assert!(
                mcp.data.is_some(),
                "data field must be Some — plain-text-only errors violate Crux #3"
            );
            let data = mcp.data.unwrap();
            assert!(
                data.get("code").is_some(),
                "data.code must be present for Agent parsing"
            );
        }
        // Sanity check: codes module is accessible
        let _ = codes::VALIDATION_ERROR;
    }
}
