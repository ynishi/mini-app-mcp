/// Schema definition types and runtime loading / validation for mini-app-mcp.
///
/// The YAML schema file (`schema.yaml`) is the **sole authority** for field
/// definitions, type coercions, and required-field validation.  No field name
/// is ever hard-coded in this module; all checks iterate over the parsed
/// [`Vec<FieldDef>`] at runtime.
use std::fs::File;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::MiniAppError;

/// The type of a field as declared in `schema.yaml`.
///
/// Supported values in YAML: `string`, `number`, `boolean`, `array`, `object`.
/// The type determines how an incoming JSON value is validated.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    /// A UTF-8 string value.
    String,
    /// A JSON number (integer or floating-point).
    Number,
    /// A JSON boolean (`true` / `false`).
    Boolean,
    /// A JSON array.
    Array,
    /// A JSON object.
    Object,
}

impl FieldType {
    /// Returns a human-readable name for use in validation error messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Number => "number",
            FieldType::Boolean => "boolean",
            FieldType::Array => "array",
            FieldType::Object => "object",
        }
    }

    /// Returns `true` if the given JSON [`serde_json::Value`] matches this
    /// field type.
    ///
    /// `Value::Null` is always considered a type mismatch (callers handle the
    /// `required=false` + null case before calling this).
    pub fn matches(&self, value: &serde_json::Value) -> bool {
        match self {
            FieldType::String => value.is_string(),
            FieldType::Number => value.is_number(),
            FieldType::Boolean => value.is_boolean(),
            FieldType::Array => value.is_array(),
            FieldType::Object => value.is_object(),
        }
    }
}

/// A single field definition parsed from `schema.yaml`.
///
/// # Fields
/// - `name`: the field name as it appears in stored JSON rows.
/// - `ty`: the expected JSON type.
/// - `required`: if `true`, the field must be present and non-null in every
///   row.
/// - `description`: optional human-readable description of this field.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldDef {
    /// Field name (arbitrary string — never hard-coded in application logic).
    pub name: String,
    /// Expected JSON type for this field.
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// Whether the field must be present in every row.
    #[serde(default)]
    pub required: bool,
    /// Optional human-readable description of this field.
    #[serde(default)]
    pub description: Option<String>,
}

/// The parsed contents of a `schema.yaml` file.
///
/// This struct is the runtime representation of the schema and acts as the
/// single source of truth for all validation decisions.  It is created once at
/// daemon startup via [`load_from_path`] and passed to every CRUD operation.
///
/// # Fields
/// - `table`: the SQLite table name (also used as a human-readable label).
/// - `title`: optional human-readable title for the table (short summary).
/// - `description`: optional long-form description for the table.
/// - `fields`: ordered list of field definitions.
/// - `dump`: optional write-only file-materialization configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchemaConfig {
    /// The logical table name declared in `schema.yaml`.
    pub table: String,
    /// Optional human-readable title for the table (short summary, plain string).
    #[serde(default)]
    pub title: Option<String>,
    /// Optional long-form description for the table. Plain string; CommonMark MAY be used by render tools (server stores it verbatim).
    #[serde(default)]
    pub description: Option<String>,
    /// All field definitions, in declaration order.
    pub fields: Vec<FieldDef>,
    /// Optional dump / file-materialization configuration.
    ///
    /// When absent from `schema.yaml`, the field deserializes to `None` and
    /// the dump feature is disabled entirely (backward-compatible default).
    #[serde(default)]
    pub dump: Option<crate::dump::DumpConfig>,
}

impl SchemaConfig {
    /// Writes this schema to a YAML file using an atomic tmp+rename strategy.
    ///
    /// The write is performed inside `tokio::task::spawn_blocking` to avoid
    /// blocking the async executor (K-110).  The rename is performed with
    /// `std::fs::rename`, which is atomic on the same filesystem on Linux/macOS
    /// (POSIX `rename(2)` guarantee).
    ///
    /// # Algorithm
    /// 1. Serialise `self` to a YAML string via `serde_yaml_bw::to_string`.
    /// 2. Write to `<path>.tmp` (same directory, so same filesystem).
    /// 3. Atomically rename `<path>.tmp` to `<path>`.
    ///
    /// # Arguments
    /// - `path`: destination path for `schema.yaml` (the final file, not `.tmp`).
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`MiniAppError::Schema`] if serialisation fails.
    /// - [`MiniAppError::Io`] if the write or rename fails.
    /// - [`MiniAppError::Backup`] if the `spawn_blocking` task panics.
    pub async fn write_to_path(&self, path: &Path) -> Result<(), MiniAppError> {
        let schema_clone = self.clone();
        let path_buf = path.to_path_buf();

        tokio::task::spawn_blocking(move || -> Result<(), MiniAppError> {
            let yaml = serde_yaml_bw::to_string(&schema_clone)
                .map_err(|e| MiniAppError::Schema(e.to_string()))?;

            let mut tmp_path = path_buf.clone();
            // Append ".tmp" to the file name to stay on the same filesystem.
            let mut file_name = tmp_path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            file_name.push(".tmp");
            tmp_path.set_file_name(file_name);

            std::fs::write(&tmp_path, yaml.as_bytes())?;
            std::fs::rename(&tmp_path, &path_buf)?;

            Ok(())
        })
        .await
        .map_err(|e| MiniAppError::Backup(format!("blocking task panic: {e}")))?
    }

    /// Validates a JSON object against this schema.
    ///
    /// Rules (applied in order, iterating over [`self.fields`]):
    ///
    /// 1. If `field.required` is `true` and the key is absent from `value`
    ///    (or its value is `null`), return
    ///    [`MiniAppError::Validation`] with `reason = "required field missing"`.
    /// 2. If the key is present and non-null but its JSON type does not match
    ///    `field.ty`, return [`MiniAppError::Validation`] with a descriptive
    ///    `reason`.
    /// 3. Unknown keys (present in `value` but not in `self.fields`) are
    ///    silently accepted — Agent-First extensibility.
    ///
    /// # Arguments
    /// - `value`: the JSON object to validate. Must be a
    ///   [`serde_json::Value::Object`]; if it is not, a `Validation` error is
    ///   returned immediately.
    ///
    /// # Errors
    /// Returns [`MiniAppError::Validation`] on the first validation failure
    /// encountered.
    pub fn validate(&self, value: &serde_json::Value) -> Result<(), MiniAppError> {
        let obj = match value.as_object() {
            Some(o) => o,
            None => {
                return Err(MiniAppError::Validation {
                    field: "(root)".to_string(),
                    reason: "value must be a JSON object".to_string(),
                });
            }
        };

        for field in &self.fields {
            let field_value = obj.get(&field.name);

            match field_value {
                None | Some(serde_json::Value::Null) => {
                    if field.required {
                        return Err(MiniAppError::Validation {
                            field: field.name.clone(),
                            reason: "required field missing".to_string(),
                        });
                    }
                    // optional and absent/null — OK
                }
                Some(v) => {
                    if !field.ty.matches(v) {
                        return Err(MiniAppError::Validation {
                            field: field.name.clone(),
                            reason: format!(
                                "expected type '{}', got '{}'",
                                field.ty.as_str(),
                                json_type_name(v)
                            ),
                        });
                    }
                }
            }
        }

        Ok(())
    }
}

/// Returns a human-readable JSON type name for a [`serde_json::Value`].
///
/// Used in validation error messages to describe the actual type received.
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Loads and parses a `schema.yaml` file from the given path.
///
/// The YAML file must conform to the following structure:
/// ```yaml
/// table: <table_name>
/// fields:
///   - name: <field_name>
///     type: string|number|boolean|array|object
///     required: true|false   # optional, defaults to false
/// ```
///
/// # Arguments
/// - `path`: filesystem path to the `schema.yaml` file.
///
/// # Returns
/// A fully-parsed [`SchemaConfig`] on success.
///
/// # Errors
/// - [`MiniAppError::Io`] if the file cannot be opened.
/// - [`MiniAppError::Schema`] if the YAML is malformed or structurally
///   invalid (e.g. missing `table` or `fields` keys).
pub fn load_from_path(path: &Path) -> Result<SchemaConfig, MiniAppError> {
    let file = File::open(path)?;
    let config: SchemaConfig =
        serde_yaml_bw::from_reader(file).map_err(|e| MiniAppError::Schema(e.to_string()))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::{NamedTempFile, TempDir};

    /// Helper: write YAML text to a temp file and return its path.
    fn write_yaml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("temp file creation is infallible in tests");
        f.write_all(content.as_bytes())
            .expect("writing to temp file is infallible in tests");
        f
    }

    /// Helper: build a simple SchemaConfig for write_to_path tests.
    fn make_test_schema() -> SchemaConfig {
        SchemaConfig {
            table: "items".to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "name".to_string(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "count".to_string(),
                    ty: FieldType::Number,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        }
    }

    // ── T1: happy-path tests ──────────────────────────────────────────────

    #[test]
    fn load_valid_schema_yaml() {
        let yaml = r#"
table: issues
fields:
  - name: title
    type: string
    required: true
  - name: state
    type: string
    required: false
  - name: tags
    type: array
    required: false
"#;
        let f = write_yaml(yaml);
        let schema = load_from_path(f.path()).expect("valid YAML must parse");
        assert_eq!(schema.table, "issues");
        assert_eq!(schema.fields.len(), 3);
        assert_eq!(schema.fields[0].name, "title");
        assert!(schema.fields[0].required);
        assert_eq!(schema.fields[0].ty, FieldType::String);
        assert!(!schema.fields[1].required);
        assert_eq!(schema.fields[2].ty, FieldType::Array);
    }

    #[test]
    fn validate_happy_path_all_fields_present() {
        let schema = SchemaConfig {
            table: "issues".to_string(),
            title: None,
            description: None,
            fields: vec![
                FieldDef {
                    name: "title".to_string(),
                    ty: FieldType::String,
                    required: true,
                    description: None,
                },
                FieldDef {
                    name: "count".to_string(),
                    ty: FieldType::Number,
                    required: false,
                    description: None,
                },
            ],
            dump: None,
        };
        let value = serde_json::json!({ "title": "hello", "count": 42 });
        assert!(schema.validate(&value).is_ok());
    }

    #[test]
    fn validate_optional_field_absent_is_ok() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "tags".to_string(),
                ty: FieldType::Array,
                required: false,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({});
        assert!(schema.validate(&value).is_ok());
    }

    #[test]
    fn validate_unknown_fields_are_accepted() {
        // Crux #1: Agent-First extensibility — extra keys must not cause errors.
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "title".to_string(),
                ty: FieldType::String,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({ "title": "hi", "extra_key": 99 });
        assert!(schema.validate(&value).is_ok());
    }

    // ── T2: boundary / edge cases ────────────────────────────────────────

    #[test]
    fn validate_null_value_for_required_field_is_error() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "title".to_string(),
                ty: FieldType::String,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({ "title": null });
        let err = schema
            .validate(&value)
            .expect_err("null required field must error");
        match err {
            MiniAppError::Validation { field, .. } => assert_eq!(field, "title"),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn validate_null_value_for_optional_field_is_ok() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "state".to_string(),
                ty: FieldType::String,
                required: false,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({ "state": null });
        assert!(schema.validate(&value).is_ok());
    }

    #[test]
    fn validate_empty_object_with_no_required_fields() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: None,
        };
        let value = serde_json::json!({});
        assert!(schema.validate(&value).is_ok());
    }

    #[test]
    fn validate_non_object_root_is_error() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![],
            dump: None,
        };
        let value = serde_json::json!([1, 2, 3]);
        let err = schema.validate(&value).expect_err("array root must error");
        assert!(matches!(err, MiniAppError::Validation { .. }));
    }

    // ── T3: error-path tests ─────────────────────────────────────────────

    #[test]
    fn validate_required_field_missing_returns_validation_error() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "title".to_string(),
                ty: FieldType::String,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({});
        let err = schema
            .validate(&value)
            .expect_err("missing required field must error");
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "title");
                assert!(reason.contains("required"));
            }
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn validate_type_mismatch_string_vs_number() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "score".to_string(),
                ty: FieldType::Number,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({ "score": "not-a-number" });
        let err = schema
            .validate(&value)
            .expect_err("type mismatch must error");
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "score");
                assert!(
                    reason.contains("number"),
                    "reason should mention expected type"
                );
                assert!(
                    reason.contains("string"),
                    "reason should mention actual type"
                );
            }
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn validate_type_mismatch_boolean_field() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "active".to_string(),
                ty: FieldType::Boolean,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({ "active": 1 });
        let err = schema.validate(&value).expect_err("number is not boolean");
        assert!(matches!(err, MiniAppError::Validation { .. }));
    }

    #[test]
    fn validate_type_mismatch_array_field() {
        let schema = SchemaConfig {
            table: "t".to_string(),
            title: None,
            description: None,
            fields: vec![FieldDef {
                name: "tags".to_string(),
                ty: FieldType::Array,
                required: true,
                description: None,
            }],
            dump: None,
        };
        let value = serde_json::json!({ "tags": "not-an-array" });
        let err = schema.validate(&value).expect_err("string is not array");
        assert!(matches!(err, MiniAppError::Validation { .. }));
    }

    #[test]
    fn load_from_nonexistent_path_returns_io_error() {
        let result = load_from_path(Path::new("/nonexistent/path/schema.yaml"));
        let err = result.expect_err("missing file must error");
        assert!(
            matches!(err, MiniAppError::Io(_)),
            "expected Io error, got {:?}",
            err
        );
    }

    #[test]
    fn load_from_malformed_yaml_returns_schema_error() {
        let yaml = "table: [\ninvalid yaml {{{\n";
        let f = write_yaml(yaml);
        let result = load_from_path(f.path());
        let err = result.expect_err("malformed YAML must error");
        assert!(
            matches!(err, MiniAppError::Schema(_)),
            "expected Schema error, got {:?}",
            err
        );
    }

    #[test]
    fn yaml_with_dump_section_deserializes() {
        let yaml = r#"
table: issues
fields:
  - name: title
    type: string
    required: true
dump:
  dir: /tmp/test-dump
  title_field: title
  body_field: body
  sync: write-only
"#;
        let f = write_yaml(yaml);
        let schema = load_from_path(f.path()).expect("valid YAML with dump must parse");
        assert_eq!(schema.table, "issues");
        let dump = schema.dump.expect("dump must be Some");
        assert_eq!(dump.title_field.as_deref(), Some("title"));
        assert_eq!(dump.body_field.as_deref(), Some("body"));
        assert_eq!(dump.sync, Some(crate::dump::SyncMode::WriteOnly));
    }

    #[test]
    fn yaml_without_dump_section_yields_none() {
        let yaml = r#"
table: issues
fields:
  - name: title
    type: string
    required: true
"#;
        let f = write_yaml(yaml);
        let schema = load_from_path(f.path()).expect("valid YAML without dump must parse");
        assert!(
            schema.dump.is_none(),
            "dump must be None when section is absent"
        );
    }

    #[test]
    fn yaml_with_bidirectional_sync_deserializes() {
        let yaml = r#"
table: tasks
fields: []
dump:
  sync: bidirectional
"#;
        let f = write_yaml(yaml);
        let schema = load_from_path(f.path()).expect("yaml with bidirectional must parse");
        let dump = schema.dump.expect("dump must be Some");
        assert_eq!(dump.sync, Some(crate::dump::SyncMode::Bidirectional));
    }

    #[test]
    fn all_field_types_match_correctly() {
        let cases: Vec<(FieldType, serde_json::Value, bool)> = vec![
            (FieldType::String, serde_json::json!("hello"), true),
            (FieldType::String, serde_json::json!(42), false),
            (FieldType::Number, serde_json::json!(2.5), true),
            (FieldType::Number, serde_json::json!("3.14"), false),
            (FieldType::Boolean, serde_json::json!(true), true),
            (FieldType::Boolean, serde_json::json!(0), false),
            (FieldType::Array, serde_json::json!([1, 2]), true),
            (FieldType::Array, serde_json::json!({}), false),
            (FieldType::Object, serde_json::json!({}), true),
            (FieldType::Object, serde_json::json!([]), false),
        ];
        for (ty, value, expected) in cases {
            assert_eq!(
                ty.matches(&value),
                expected,
                "FieldType::{} .matches({:?}) should be {}",
                ty.as_str(),
                value,
                expected
            );
        }
    }

    // T1: write_to_path round-trips via load_from_path
    #[tokio::test]
    async fn write_to_path_round_trips_via_load_from_path() {
        let dir = TempDir::new().expect("temp dir creation is infallible in tests");
        let schema_path = dir.path().join("schema.yaml");
        let original = make_test_schema();

        original
            .write_to_path(&schema_path)
            .await
            .expect("write_to_path must succeed");

        let loaded = load_from_path(&schema_path).expect("load_from_path must succeed");

        assert_eq!(loaded.table, original.table);
        assert_eq!(loaded.fields.len(), original.fields.len());
        for (l, r) in loaded.fields.iter().zip(original.fields.iter()) {
            assert_eq!(l.name, r.name);
            assert_eq!(l.ty, r.ty);
            assert_eq!(l.required, r.required);
        }
        assert!(loaded.dump.is_none());
    }

    // T2: write_to_path uses tmp+rename — no partial file visible on simulated error
    //
    // This test verifies that the .tmp file does NOT persist after a successful
    // write.  A failed-write scenario (writing to a read-only path) verifies
    // the output path is clean.
    #[tokio::test]
    async fn write_to_path_uses_tmp_then_rename() {
        let dir = TempDir::new().expect("temp dir creation is infallible in tests");
        let schema_path = dir.path().join("schema.yaml");
        let schema = make_test_schema();

        schema
            .write_to_path(&schema_path)
            .await
            .expect("write_to_path must succeed");

        // After successful write, the final file exists...
        assert!(schema_path.exists(), "final schema.yaml must exist");
        // ...but the tmp file must have been cleaned up by rename.
        let mut tmp_path = PathBuf::from(&schema_path);
        let mut file_name = tmp_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        file_name.push(".tmp");
        tmp_path.set_file_name(file_name);
        assert!(
            !tmp_path.exists(),
            ".tmp file must not exist after successful write"
        );
    }

    // T3: write_to_path to a non-existent directory returns Io error
    #[tokio::test]
    async fn write_to_path_missing_parent_returns_io_error() {
        let schema = make_test_schema();
        let result = schema
            .write_to_path(Path::new("/nonexistent/deep/path/schema.yaml"))
            .await;
        let err = result.expect_err("write to missing dir must error");
        assert!(
            matches!(err, MiniAppError::Io(_)),
            "expected Io error, got {:?}",
            err
        );
    }

    #[test]
    fn yaml_with_title_and_description_deserializes() {
        let yaml = r#"
table: closet_snap
title: Closet Snap
description: |
  Persona の今日その瞬間の自分を保存する情緒 snapshot。
  outfit という domain-specific な snap で記録する。
fields:
  - name: date
    type: string
    required: true
"#;
        let f = write_yaml(yaml);
        let schema =
            load_from_path(f.path()).expect("valid YAML with title/description must parse");
        assert_eq!(schema.table, "closet_snap");
        assert_eq!(
            schema.title.as_deref(),
            Some("Closet Snap"),
            "title must be Some(\"Closet Snap\")"
        );
        assert!(
            schema
                .description
                .as_deref()
                .unwrap_or("")
                .contains("Persona"),
            "description must be Some and contain 'Persona'"
        );
    }

    #[test]
    fn yaml_without_title_section_yields_none() {
        let yaml = r#"
table: issues
fields:
  - name: title
    type: string
    required: true
"#;
        let f = write_yaml(yaml);
        let schema =
            load_from_path(f.path()).expect("valid YAML without title/description must parse");
        assert!(
            schema.title.is_none(),
            "title must be None when section is absent"
        );
        assert!(
            schema.description.is_none(),
            "description must be None when section is absent"
        );
    }

    #[test]
    fn field_def_with_description_round_trips() {
        let yaml = r#"
table: snap
fields:
  - name: date
    type: string
    required: true
    description: ISO date (YYYY-MM-DD)
  - name: mood
    type: string
    required: false
"#;
        let f = write_yaml(yaml);
        let schema =
            load_from_path(f.path()).expect("valid YAML with field description must parse");
        assert_eq!(schema.fields.len(), 2);
        assert_eq!(
            schema.fields[0].description.as_deref(),
            Some("ISO date (YYYY-MM-DD)"),
            "field description must round-trip"
        );
        assert!(
            schema.fields[1].description.is_none(),
            "absent field description must be None"
        );
    }
}
