/// `ListFilter` enum for server-side row filtering in the `list` tool.
///
/// Filters are validated against the table's `schema.yaml` before any SQL is
/// generated.  Unknown field names and type-mismatched scalar values are
/// rejected with [`crate::error::MiniAppError::Validation`].
///
/// # Crux constraints
/// - **Unknown field reject**: every `field` reference is checked against
///   `schema.yaml` before reaching the query builder.
/// - **Typed scalar mismatch reject**: `Eq`/`In` values are type-checked with
///   [`crate::schema::FieldType::matches`]; `Null`, `Object`, and `Array`
///   values are always rejected.
/// - **Recursive Or/And**: `Or` and `And` accept `Vec<ListFilter>` that may
///   itself contain `Or`/`And` to arbitrary depth.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::MiniAppError;
use crate::schema::SchemaConfig;

// ---------------------------------------------------------------------------
// FilterParam — rusqlite bind value for a filter placeholder
// ---------------------------------------------------------------------------

/// A single bound parameter produced by [`ListFilter::build_sql`].
///
/// Covers the scalar types that can appear in `Eq` / `In` filter values:
/// strings, numbers, and booleans.  `Null` and compound types (`Object`,
/// `Array`) are rejected during [`ListFilter::validate`] and never appear
/// here.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterParam {
    /// A UTF-8 string value.
    Text(String),
    /// A numeric value (stored as `f64`; SQLite treats both `INTEGER` and
    /// `REAL` comparisons uniformly via `json_extract`).
    Number(f64),
    /// A boolean value.
    Bool(bool),
}

impl rusqlite::ToSql for FilterParam {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            FilterParam::Text(s) => s.to_sql(),
            FilterParam::Number(n) => n.to_sql(),
            FilterParam::Bool(b) => b.to_sql(),
        }
    }
}

// ---------------------------------------------------------------------------
// ListFilter enum
// ---------------------------------------------------------------------------

/// Server-side row filter for the `list` tool.
///
/// Serialised as a serde tagged enum (`"type"` discriminant, snake_case).
///
/// # Variants
///
/// - `Eq` — exact equality: `json_extract(data, '$.field') = ?`
/// - `In` — membership: `json_extract(data, '$.field') IN (?, …)`
/// - `Or` — logical OR of a non-empty list of sub-filters (parenthesised)
/// - `And` — logical AND of a non-empty list of sub-filters (parenthesised)
///
/// # Example JSON
/// ```json
/// {"type": "or", "filters": [
///   {"type": "eq", "field": "mailbox", "value": "inbox"},
///   {"type": "eq", "field": "mailbox", "value": "sent"}
/// ]}
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ListFilter {
    /// Equality filter: `json_extract(data, '$.field') = value`.
    Eq {
        /// Schema field name to compare against.
        field: String,
        /// Scalar value to compare.  Must match the schema-declared type for
        /// `field`.  `null`, objects, and arrays are rejected.
        value: serde_json::Value,
    },
    /// Membership filter: `json_extract(data, '$.field') IN (v1, v2, …)`.
    In {
        /// Schema field name to compare against.
        field: String,
        /// Non-empty list of scalar values.  Each element must match the
        /// schema-declared type for `field`.
        values: Vec<serde_json::Value>,
    },
    /// Logical OR of a non-empty list of sub-filters.
    Or {
        /// Non-empty list of sub-filters combined with `OR`.
        filters: Vec<ListFilter>,
    },
    /// Logical AND of a non-empty list of sub-filters.
    And {
        /// Non-empty list of sub-filters combined with `AND`.
        filters: Vec<ListFilter>,
    },
}

impl ListFilter {
    /// Validate the filter tree against `schema`.
    ///
    /// Checks recursively:
    /// 1. Every `field` name exists in `schema.yaml` (unknown field → reject).
    /// 2. Every scalar value matches the schema-declared type for its field
    ///    (`Null`, `Object`, `Array` are always rejected; `Number`/`Boolean`
    ///    only accepted if the field type matches).
    /// 3. `In.values` is non-empty.
    /// 4. `Or.filters` / `And.filters` are non-empty.
    ///
    /// # Errors
    /// Returns [`MiniAppError::Validation`] on any violation.
    pub fn validate(&self, schema: &SchemaConfig) -> Result<(), MiniAppError> {
        match self {
            ListFilter::Eq { field, value } => {
                let field_def = schema.fields.iter().find(|f| &f.name == field);
                let fd = field_def.ok_or_else(|| MiniAppError::Validation {
                    field: field.clone(),
                    reason: format!(
                        "unknown field '{field}' — only schema-registered fields are allowed in filter"
                    ),
                })?;
                validate_scalar_value(field, value, &fd.ty)?;
            }
            ListFilter::In { field, values } => {
                let field_def = schema.fields.iter().find(|f| &f.name == field);
                let fd = field_def.ok_or_else(|| MiniAppError::Validation {
                    field: field.clone(),
                    reason: format!(
                        "unknown field '{field}' — only schema-registered fields are allowed in filter"
                    ),
                })?;
                if values.is_empty() {
                    return Err(MiniAppError::Validation {
                        field: field.clone(),
                        reason: "In.values must not be empty".to_string(),
                    });
                }
                for v in values {
                    validate_scalar_value(field, v, &fd.ty)?;
                }
            }
            ListFilter::Or { filters } => {
                if filters.is_empty() {
                    return Err(MiniAppError::Validation {
                        field: "filters".to_string(),
                        reason: "Or.filters must not be empty".to_string(),
                    });
                }
                for f in filters {
                    f.validate(schema)?;
                }
            }
            ListFilter::And { filters } => {
                if filters.is_empty() {
                    return Err(MiniAppError::Validation {
                        field: "filters".to_string(),
                        reason: "And.filters must not be empty".to_string(),
                    });
                }
                for f in filters {
                    f.validate(schema)?;
                }
            }
        }
        Ok(())
    }

    /// Build a parameterised SQL WHERE fragment and its bound parameters.
    ///
    /// Must be called **after** [`validate`](Self::validate) returns `Ok`.
    /// Field names are guaranteed safe (schema-whitelist-checked); values are
    /// bound via positional `?` placeholders — no string interpolation of
    /// values occurs.
    ///
    /// Returns `(sql_fragment, params)` where `sql_fragment` is the WHERE
    /// predicate (without the `WHERE` keyword) and `params` is the ordered
    /// list of bound values.
    ///
    /// # Errors
    /// Returns [`MiniAppError::Validation`] if a value cannot be converted to
    /// a [`FilterParam`] (e.g. an `Array`/`Object`/`Null` that slipped
    /// through — treated defensively).
    pub fn build_sql(&self) -> Result<(String, Vec<FilterParam>), MiniAppError> {
        match self {
            ListFilter::Eq { field, value } => {
                let param = json_value_to_param(field, value)?;
                let sql = format!("json_extract(data, '$.{field}') = ?");
                Ok((sql, vec![param]))
            }
            ListFilter::In { field, values } => {
                let mut params = Vec::with_capacity(values.len());
                for v in values {
                    params.push(json_value_to_param(field, v)?);
                }
                let placeholders = vec!["?"; values.len()].join(", ");
                let sql = format!("json_extract(data, '$.{field}') IN ({placeholders})");
                Ok((sql, params))
            }
            ListFilter::Or { filters } => build_composite_sql(filters, "OR"),
            ListFilter::And { filters } => build_composite_sql(filters, "AND"),
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Validate that `value` is a scalar (non-null, non-object, non-array) and
/// that its type matches `field_ty`.
fn validate_scalar_value(
    field: &str,
    value: &serde_json::Value,
    field_ty: &crate::schema::FieldType,
) -> Result<(), MiniAppError> {
    // Null is always rejected for filter values.
    if value.is_null() {
        return Err(MiniAppError::Validation {
            field: field.to_string(),
            reason: format!("filter value for field '{field}' must not be null"),
        });
    }
    // Object and Array are not scalar filter values.
    if value.is_object() || value.is_array() {
        return Err(MiniAppError::Validation {
            field: field.to_string(),
            reason: format!(
                "filter value for field '{field}' must be a scalar (string, number, or boolean), \
                 got an object or array"
            ),
        });
    }
    // Check the value against the schema-declared field type.
    if !field_ty.matches(value) {
        return Err(MiniAppError::Validation {
            field: field.to_string(),
            reason: format!(
                "filter value for field '{field}' has type mismatch: expected {}, \
                 got {}",
                field_ty.as_str(),
                json_type_name(value),
            ),
        });
    }
    Ok(())
}

/// Convert a validated scalar [`serde_json::Value`] to a [`FilterParam`].
///
/// Panics are structurally impossible here because `validate` is called first,
/// but we return an error defensively for robustness.
fn json_value_to_param(
    field: &str,
    value: &serde_json::Value,
) -> Result<FilterParam, MiniAppError> {
    match value {
        serde_json::Value::String(s) => Ok(FilterParam::Text(s.clone())),
        serde_json::Value::Number(n) => {
            let f = n.as_f64().ok_or_else(|| MiniAppError::Validation {
                field: field.to_string(),
                reason: format!("filter number value for field '{field}' is out of f64 range"),
            })?;
            Ok(FilterParam::Number(f))
        }
        serde_json::Value::Bool(b) => Ok(FilterParam::Bool(*b)),
        _ => Err(MiniAppError::Validation {
            field: field.to_string(),
            reason: format!("filter value for field '{field}' is not a bindable scalar"),
        }),
    }
}

/// Build a composite SQL fragment for `Or` / `And` by joining sub-fragments
/// with `operator` ("OR" or "AND") inside parentheses.
fn build_composite_sql(
    filters: &[ListFilter],
    operator: &str,
) -> Result<(String, Vec<FilterParam>), MiniAppError> {
    let mut parts: Vec<String> = Vec::with_capacity(filters.len());
    let mut all_params: Vec<FilterParam> = Vec::new();
    for f in filters {
        let (fragment, params) = f.build_sql()?;
        parts.push(fragment);
        all_params.extend(params);
    }
    let sql = format!("({})", parts.join(&format!(" {operator} ")));
    Ok((sql, all_params))
}

/// Return a human-readable JSON type name for error messages.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldDef, FieldType, SchemaConfig};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_schema(fields: Vec<(&str, FieldType)>) -> SchemaConfig {
        SchemaConfig {
            table: "test".to_string(),
            title: None,
            description: None,
            fields: fields
                .into_iter()
                .map(|(name, ty)| FieldDef {
                    name: name.to_string(),
                    ty,
                    required: false,
                    description: None,
                })
                .collect(),
            dump: None,
        }
    }

    // -----------------------------------------------------------------------
    // Validate tests
    // -----------------------------------------------------------------------

    /// Eq filter against a known string field validates successfully.
    #[test]
    fn eq_validate_ok() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::Eq {
            field: "mailbox".to_string(),
            value: serde_json::Value::String("inbox".to_string()),
        };
        assert!(f.validate(&schema).is_ok());
    }

    /// In filter with multiple values validates successfully.
    #[test]
    fn in_validate_ok() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::In {
            field: "mailbox".to_string(),
            values: vec![
                serde_json::Value::String("inbox".to_string()),
                serde_json::Value::String("sent".to_string()),
            ],
        };
        assert!(f.validate(&schema).is_ok());
    }

    /// Or filter with nested sub-filters validates successfully (2-level nest).
    #[test]
    fn or_composition_validate_ok() {
        let schema = make_schema(vec![
            ("mailbox", FieldType::String),
            ("status", FieldType::String),
        ]);
        let f = ListFilter::Or {
            filters: vec![
                ListFilter::Eq {
                    field: "mailbox".to_string(),
                    value: serde_json::Value::String("inbox".to_string()),
                },
                ListFilter::And {
                    filters: vec![
                        ListFilter::Eq {
                            field: "mailbox".to_string(),
                            value: serde_json::Value::String("sent".to_string()),
                        },
                        ListFilter::Eq {
                            field: "status".to_string(),
                            value: serde_json::Value::String("read".to_string()),
                        },
                    ],
                },
            ],
        };
        assert!(f.validate(&schema).is_ok());
    }

    /// And filter with Eq and In sub-filters validates successfully.
    #[test]
    fn and_composition_validate_ok() {
        let schema = make_schema(vec![
            ("mailbox", FieldType::String),
            ("tag", FieldType::String),
        ]);
        let f = ListFilter::And {
            filters: vec![
                ListFilter::Eq {
                    field: "mailbox".to_string(),
                    value: serde_json::Value::String("inbox".to_string()),
                },
                ListFilter::In {
                    field: "tag".to_string(),
                    values: vec![
                        serde_json::Value::String("urgent".to_string()),
                        serde_json::Value::String("flagged".to_string()),
                    ],
                },
            ],
        };
        assert!(f.validate(&schema).is_ok());
    }

    /// Eq filter with a field not in schema is rejected with Validation error.
    #[test]
    fn unknown_field_reject() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::Eq {
            field: "nonexistent_field".to_string(),
            value: serde_json::Value::String("x".to_string()),
        };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "nonexistent_field");
                assert!(reason.contains("unknown field"));
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    /// Eq filter with a Number value against a String-typed field is rejected.
    #[test]
    fn typed_scalar_mismatch_reject() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::Eq {
            field: "mailbox".to_string(),
            value: serde_json::json!(42),
        };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "mailbox");
                assert!(reason.contains("type mismatch"), "got: {reason}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    /// Null value is rejected for Eq filter.
    #[test]
    fn null_value_reject() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::Eq {
            field: "mailbox".to_string(),
            value: serde_json::Value::Null,
        };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { field, .. } => assert_eq!(field, "mailbox"),
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    /// Bool value is rejected against a String-typed schema field.
    #[test]
    fn bool_against_string_field_reject() {
        let schema = make_schema(vec![("active", FieldType::String)]);
        let f = ListFilter::Eq {
            field: "active".to_string(),
            value: serde_json::Value::Bool(true),
        };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "active");
                assert!(reason.contains("type mismatch"), "got: {reason}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    /// Number field accepts a number value.
    #[test]
    fn number_field_accepts_number_value() {
        let schema = make_schema(vec![("count", FieldType::Number)]);
        let f = ListFilter::Eq {
            field: "count".to_string(),
            value: serde_json::json!(42),
        };
        assert!(f.validate(&schema).is_ok());
    }

    /// Boolean field accepts a boolean value.
    #[test]
    fn boolean_field_accepts_bool_value() {
        let schema = make_schema(vec![("active", FieldType::Boolean)]);
        let f = ListFilter::Eq {
            field: "active".to_string(),
            value: serde_json::Value::Bool(true),
        };
        assert!(f.validate(&schema).is_ok());
    }

    /// Empty In.values is rejected.
    #[test]
    fn in_empty_values_reject() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::In {
            field: "mailbox".to_string(),
            values: vec![],
        };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { reason, .. } => {
                assert!(reason.contains("must not be empty"), "got: {reason}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    /// Empty Or.filters is rejected.
    #[test]
    fn or_empty_filters_reject() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::Or { filters: vec![] };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { reason, .. } => {
                assert!(reason.contains("must not be empty"), "got: {reason}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    /// Empty And.filters is rejected.
    #[test]
    fn and_empty_filters_reject() {
        let schema = make_schema(vec![("mailbox", FieldType::String)]);
        let f = ListFilter::And { filters: vec![] };
        let err = f.validate(&schema).unwrap_err();
        match err {
            MiniAppError::Validation { reason, .. } => {
                assert!(reason.contains("must not be empty"), "got: {reason}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // build_sql tests
    // -----------------------------------------------------------------------

    /// Eq filter builds correct SQL fragment and params.
    #[test]
    fn eq_build_sql() {
        let f = ListFilter::Eq {
            field: "mailbox".to_string(),
            value: serde_json::Value::String("inbox".to_string()),
        };
        let (sql, params) = f.build_sql().unwrap();
        assert_eq!(sql, "json_extract(data, '$.mailbox') = ?");
        assert_eq!(params, vec![FilterParam::Text("inbox".to_string())]);
    }

    /// In filter builds correct SQL fragment with multiple placeholders.
    #[test]
    fn in_build_sql() {
        let f = ListFilter::In {
            field: "mailbox".to_string(),
            values: vec![
                serde_json::Value::String("inbox".to_string()),
                serde_json::Value::String("sent".to_string()),
            ],
        };
        let (sql, params) = f.build_sql().unwrap();
        assert_eq!(sql, "json_extract(data, '$.mailbox') IN (?, ?)");
        assert_eq!(
            params,
            vec![
                FilterParam::Text("inbox".to_string()),
                FilterParam::Text("sent".to_string()),
            ]
        );
    }

    /// Or filter builds parenthesised OR expression.
    #[test]
    fn or_build_sql() {
        let f = ListFilter::Or {
            filters: vec![
                ListFilter::Eq {
                    field: "mailbox".to_string(),
                    value: serde_json::Value::String("inbox".to_string()),
                },
                ListFilter::Eq {
                    field: "mailbox".to_string(),
                    value: serde_json::Value::String("sent".to_string()),
                },
            ],
        };
        let (sql, params) = f.build_sql().unwrap();
        assert_eq!(
            sql,
            "(json_extract(data, '$.mailbox') = ? OR json_extract(data, '$.mailbox') = ?)"
        );
        assert_eq!(
            params,
            vec![
                FilterParam::Text("inbox".to_string()),
                FilterParam::Text("sent".to_string()),
            ]
        );
    }

    /// And filter builds parenthesised AND expression.
    #[test]
    fn and_build_sql() {
        let f = ListFilter::And {
            filters: vec![
                ListFilter::Eq {
                    field: "mailbox".to_string(),
                    value: serde_json::Value::String("inbox".to_string()),
                },
                ListFilter::Eq {
                    field: "status".to_string(),
                    value: serde_json::Value::String("unread".to_string()),
                },
            ],
        };
        let (sql, params) = f.build_sql().unwrap();
        assert_eq!(
            sql,
            "(json_extract(data, '$.mailbox') = ? AND json_extract(data, '$.status') = ?)"
        );
        assert_eq!(
            params,
            vec![
                FilterParam::Text("inbox".to_string()),
                FilterParam::Text("unread".to_string()),
            ]
        );
    }

    /// Nested Or-of-And builds structurally correct SQL (mailbox use case).
    #[test]
    fn nested_or_of_and_build_sql() {
        // OR( AND(mailbox=inbox, status=unread), AND(mailbox=sent, status=read) )
        let f = ListFilter::Or {
            filters: vec![
                ListFilter::And {
                    filters: vec![
                        ListFilter::Eq {
                            field: "mailbox".to_string(),
                            value: serde_json::json!("inbox"),
                        },
                        ListFilter::Eq {
                            field: "status".to_string(),
                            value: serde_json::json!("unread"),
                        },
                    ],
                },
                ListFilter::And {
                    filters: vec![
                        ListFilter::Eq {
                            field: "mailbox".to_string(),
                            value: serde_json::json!("sent"),
                        },
                        ListFilter::Eq {
                            field: "status".to_string(),
                            value: serde_json::json!("read"),
                        },
                    ],
                },
            ],
        };
        let (sql, params) = f.build_sql().unwrap();
        assert_eq!(
            sql,
            "((json_extract(data, '$.mailbox') = ? AND json_extract(data, '$.status') = ?) \
             OR (json_extract(data, '$.mailbox') = ? AND json_extract(data, '$.status') = ?))"
        );
        assert_eq!(
            params,
            vec![
                FilterParam::Text("inbox".to_string()),
                FilterParam::Text("unread".to_string()),
                FilterParam::Text("sent".to_string()),
                FilterParam::Text("read".to_string()),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // R1: schemars JsonSchema derivation test
    // -----------------------------------------------------------------------

    /// Verify that `schemars::schema_for!(ListFilter)` does not panic.
    ///
    /// Confirms that the recursive `Vec<ListFilter>` type is handled correctly
    /// by schemars (generates `$ref` / `$defs` without infinite recursion).
    #[test]
    fn schema_for_list_filter_succeeds() {
        // This must not panic.
        let _schema = schemars::schema_for!(ListFilter);
    }
}
