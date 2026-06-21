/// `OrderByItem` and `Direction` — server-side ORDER BY primitive for the
/// `list` tool.
///
/// Sort keys are validated against the table's `schema.yaml` before any SQL
/// is generated.  Unknown field names are rejected with
/// [`crate::error::MiniAppError::Validation`].  An empty `order_by` slice is
/// also rejected (callers should omit the argument entirely to use the default
/// `ORDER BY created_at DESC`).
///
/// # SQL injection safety
///
/// `build_order_by_sql` is **infallible** and emits only pre-validated field
/// names (schema-whitelist-checked by `validate_order_by`) combined with an
/// enum-to-literal direction (`"ASC"` / `"DESC"`).  No external string is
/// interpolated without prior validation.  Direction keywords are **not** bound
/// via `?` parameters because SQLite treats `ORDER BY` direction as a SQL
/// syntax keyword, not a parameterisable value.
///
/// # Crux constraints
/// - multi-key sort via `Vec<OrderByItem>` preserves caller-specified order.
/// - `Direction::Asc` → `"ASC"` / `Direction::Desc` → `"DESC"` literals only.
/// - No `HashMap` / `BTreeMap` representation (order guarantee lost).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::MiniAppError;
use crate::schema::SchemaConfig;

// ---------------------------------------------------------------------------
// Direction enum
// ---------------------------------------------------------------------------

/// Sort direction for a single [`OrderByItem`].
///
/// Serialised as a lowercase string (`"asc"` / `"desc"`) so the JSON wire
/// format matches the issue specification examples.
///
/// # Example JSON
/// ```json
/// {"field": "priority", "direction": "asc"}
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Sort ascending: smallest value first.
    Asc,
    /// Sort descending: largest value first.
    Desc,
}

impl Direction {
    /// Returns the SQL keyword literal for this direction.
    ///
    /// Used inside [`build_order_by_sql`] to construct the ORDER BY clause.
    /// Returns `"ASC"` or `"DESC"` (uppercase, as conventional in SQL).
    #[inline]
    pub fn as_sql_literal(self) -> &'static str {
        match self {
            Direction::Asc => "ASC",
            Direction::Desc => "DESC",
        }
    }
}

// ---------------------------------------------------------------------------
// OrderByItem struct
// ---------------------------------------------------------------------------

/// A single sort key for the `order_by` argument of [`crate::store::Store::list`].
///
/// `field` must be a name registered in `schema.yaml`; it is validated by
/// [`validate_order_by`] before any SQL is generated.  `direction` controls
/// whether the sort is ascending or descending.
///
/// # Example JSON
/// ```json
/// {"field": "priority", "direction": "asc"}
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct OrderByItem {
    /// Schema-registered field name to sort by.
    ///
    /// Must exist in `schema.yaml`; unknown names are rejected by
    /// [`validate_order_by`] with [`MiniAppError::Validation`].
    pub field: String,
    /// Sort direction: `"asc"` (ascending) or `"desc"` (descending).
    pub direction: Direction,
}

// ---------------------------------------------------------------------------
// validate_order_by
// ---------------------------------------------------------------------------

/// Validate a slice of [`OrderByItem`] values against a table schema.
///
/// Checks:
/// 1. The slice is non-empty (an empty `order_by` is ambiguous; callers should
///    omit the argument to use the default `ORDER BY created_at DESC`).
/// 2. Each `field` name exists in `schema.yaml`.
///
/// Type-checking is **not** performed: SQLite's `json_extract(data, '$.field')`
/// supports `ORDER BY` across all JSON types, so restricting by type would
/// produce spurious errors and add no safety value.
///
/// # Errors
///
/// Returns [`MiniAppError::Validation`] when:
/// - `items` is empty — `field` is `"order_by"` and `reason` contains
///   `"must not be empty"`.
/// - An item's `field` is not registered in the schema — `field` is the
///   unknown name and `reason` contains `"unknown field"`.
pub fn validate_order_by(items: &[OrderByItem], schema: &SchemaConfig) -> Result<(), MiniAppError> {
    if items.is_empty() {
        return Err(MiniAppError::Validation {
            field: "order_by".to_string(),
            reason: "order_by must not be empty when supplied \
                     (omit the argument to use the default created_at DESC)"
                .to_string(),
        });
    }
    for item in items {
        if !schema.fields.iter().any(|f| f.name == item.field) {
            return Err(MiniAppError::Validation {
                field: item.field.clone(),
                reason: format!(
                    "unknown field '{}' — only schema-registered fields are allowed in order_by",
                    item.field
                ),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// build_order_by_sql
// ---------------------------------------------------------------------------

/// Build a SQL ORDER BY clause body from a validated slice of [`OrderByItem`].
///
/// # Safety invariant
///
/// **Must only be called after [`validate_order_by`] returns `Ok`.**  Field
/// names are interpolated as SQL literals (inside `json_extract` paths) and
/// must have been verified against the schema whitelist first.  Direction
/// keywords are produced by [`Direction::as_sql_literal`] — no external string
/// is ever used for the direction.
///
/// # Returns
///
/// A `String` in the form:
/// ```text
/// json_extract(data, '$.field1') ASC, json_extract(data, '$.field2') DESC
/// ```
///
/// The string does **not** include the `ORDER BY` keyword itself; the caller
/// wraps it as needed (e.g. `format!(" ORDER BY {}", build_order_by_sql(…))`).
///
/// # Panics
///
/// Does not panic.  `items` must be non-empty (ensured by `validate_order_by`);
/// an empty slice produces an empty string.
pub fn build_order_by_sql(items: &[OrderByItem]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| {
            format!(
                "json_extract(data, '$.{}') {}",
                item.field,
                item.direction.as_sql_literal()
            )
        })
        .collect();
    parts.join(", ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldDef, FieldType, SchemaConfig};

    /// Build a minimal [`SchemaConfig`] with the given field names (all `string` type).
    fn make_schema(field_names: &[&str]) -> SchemaConfig {
        SchemaConfig {
            table: "test_table".to_string(),
            title: None,
            description: None,
            fields: field_names
                .iter()
                .map(|name| FieldDef {
                    name: name.to_string(),
                    ty: FieldType::String,
                    required: false,
                    description: None,
                })
                .collect(),
            dump: None,
        }
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip tests
    // -----------------------------------------------------------------------

    /// `Direction` serialises to `"asc"` / `"desc"` (lowercase) and
    /// deserialises back to the correct variant.
    #[test]
    fn direction_serde_roundtrip() {
        // Asc → "asc"
        let asc_json = serde_json::to_string(&Direction::Asc).unwrap();
        assert_eq!(asc_json, r#""asc""#);
        let asc_back: Direction = serde_json::from_str(&asc_json).unwrap();
        assert_eq!(asc_back, Direction::Asc);

        // Desc → "desc"
        let desc_json = serde_json::to_string(&Direction::Desc).unwrap();
        assert_eq!(desc_json, r#""desc""#);
        let desc_back: Direction = serde_json::from_str(&desc_json).unwrap();
        assert_eq!(desc_back, Direction::Desc);
    }

    /// `OrderByItem` roundtrips through JSON with the literal form used in the
    /// issue specification.
    #[test]
    fn order_by_item_serde_roundtrip() {
        let json = r#"{"field": "priority", "direction": "asc"}"#;
        let item: OrderByItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.field, "priority");
        assert_eq!(item.direction, Direction::Asc);

        let re_serialised = serde_json::to_value(&item).unwrap();
        assert_eq!(re_serialised["field"], "priority");
        assert_eq!(re_serialised["direction"], "asc");
    }

    // -----------------------------------------------------------------------
    // validate_order_by tests
    // -----------------------------------------------------------------------

    /// A valid single-field order_by passes validation.
    #[test]
    fn validate_order_by_ok() {
        let schema = make_schema(&["priority", "due", "status"]);
        let items = vec![OrderByItem {
            field: "priority".to_string(),
            direction: Direction::Asc,
        }];
        assert!(validate_order_by(&items, &schema).is_ok());
    }

    /// An unknown field name is rejected with a reason containing "unknown field".
    #[test]
    fn validate_order_by_unknown_field_reject() {
        let schema = make_schema(&["priority", "due"]);
        let items = vec![OrderByItem {
            field: "nonexistent".to_string(),
            direction: Direction::Asc,
        }];
        let err = validate_order_by(&items, &schema).unwrap_err();
        match &err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "nonexistent");
                assert!(
                    reason.contains("unknown field"),
                    "reason should contain 'unknown field', got: {reason}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    /// An empty slice is rejected with a reason containing "must not be empty".
    #[test]
    fn validate_order_by_empty_reject() {
        let schema = make_schema(&["priority"]);
        let err = validate_order_by(&[], &schema).unwrap_err();
        match &err {
            MiniAppError::Validation { field, reason } => {
                assert_eq!(field, "order_by");
                assert!(
                    reason.contains("must not be empty"),
                    "reason should contain 'must not be empty', got: {reason}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // build_order_by_sql tests
    // -----------------------------------------------------------------------

    /// Single-key ASC produces the expected literal.
    #[test]
    fn build_order_by_sql_single_asc() {
        let items = vec![OrderByItem {
            field: "priority".to_string(),
            direction: Direction::Asc,
        }];
        let sql = build_order_by_sql(&items);
        assert_eq!(sql, "json_extract(data, '$.priority') ASC");
    }

    /// Single-key DESC produces the expected literal.
    #[test]
    fn build_order_by_sql_single_desc() {
        let items = vec![OrderByItem {
            field: "priority".to_string(),
            direction: Direction::Desc,
        }];
        let sql = build_order_by_sql(&items);
        assert_eq!(sql, "json_extract(data, '$.priority') DESC");
    }

    /// Multi-key produces comma-separated entries preserving caller order.
    ///
    /// Crux: `priority ASC, due ASC` matches the issue specification example.
    #[test]
    fn build_order_by_sql_multi_key() {
        let items = vec![
            OrderByItem {
                field: "priority".to_string(),
                direction: Direction::Asc,
            },
            OrderByItem {
                field: "due".to_string(),
                direction: Direction::Asc,
            },
        ];
        let sql = build_order_by_sql(&items);
        assert_eq!(
            sql,
            "json_extract(data, '$.priority') ASC, json_extract(data, '$.due') ASC"
        );
    }

    /// Multi-key with mixed directions preserves order and direction literals.
    #[test]
    fn build_order_by_sql_multi_key_mixed_directions() {
        let items = vec![
            OrderByItem {
                field: "priority".to_string(),
                direction: Direction::Asc,
            },
            OrderByItem {
                field: "created_at".to_string(),
                direction: Direction::Desc,
            },
        ];
        let sql = build_order_by_sql(&items);
        assert_eq!(
            sql,
            "json_extract(data, '$.priority') ASC, json_extract(data, '$.created_at') DESC"
        );
    }

    // -----------------------------------------------------------------------
    // schemars JsonSchema derivation test
    // -----------------------------------------------------------------------

    /// Verify that `schemars::schema_for!(Vec<OrderByItem>)` does not panic.
    ///
    /// Confirms that the `JsonSchema` derive on `OrderByItem` and `Direction`
    /// produces valid schemas (no infinite recursion, no unsupported types).
    #[test]
    fn schema_for_order_by_succeeds() {
        // Must not panic.
        let _schema = schemars::schema_for!(Vec<OrderByItem>);
    }
}
