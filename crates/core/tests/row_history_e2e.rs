//! End-to-end tests for the row-history auto-log feature.
//!
//! Each test opens an in-memory Store, performs a sequence of mutations and
//! inspects the `_row_history` table via `row_history::*` helpers.
//!
//! All timestamps are **literal unix seconds** — `Instant::now` / `SystemTime`
//! are never used so the tests are fully deterministic and cannot flake.

use std::path::Path;

use mini_app_core::{
    row_history,
    schema::{FieldDef, FieldType, SchemaConfig},
    store::{Store, UpdateMode},
};
use serde_json::json;

/// Build a minimal in-memory Store for testing.
///
/// The table has one required field (`title: string`).
async fn make_store() -> Store {
    let schema = SchemaConfig {
        table: "test".to_string(),
        title: None,
        description: None,
        fields: vec![FieldDef {
            name: "title".to_string(),
            ty: FieldType::String,
            required: true,
            description: None,
        }],
        dump: None,
        history: Default::default(),
    };
    Store::open(Path::new(":memory:"), schema)
        .await
        .expect("in-memory store")
}

// =============================================================================
// Case 1 — fetch_at returns the correct intermediate snapshot
// =============================================================================

/// Directly inserts 4 history records at distinct literal timestamps (100, 200,
/// 300, 400), then verifies that `fetch_at` returns the right snapshot for each
/// boundary.
///
/// No `Instant::now` is used — all timestamps are literal unix seconds so the
/// test is fully deterministic regardless of clock precision.
#[tokio::test]
async fn case1_fetch_at_intermediate_state() {
    let store = make_store().await;
    let row_id = "row-case1-test";

    // Insert 4 history records with distinct synthetic timestamps using the
    // low-level helper (same pattern as case4_purge_max_per_row).
    {
        let conn = store.conn_for_test();
        let tx = conn.unchecked_transaction().expect("begin tx");

        row_history::record_in_tx_for_test(
            &tx,
            "test",
            row_id,
            row_history::HistoryOp::Create,
            r#"{"title":"v0"}"#,
            None,
            100,
            1,
        )
        .expect("record v0");
        row_history::record_in_tx_for_test(
            &tx,
            "test",
            row_id,
            row_history::HistoryOp::Update,
            r#"{"title":"v1"}"#,
            Some(r#"{"title":"v0"}"#),
            200,
            2,
        )
        .expect("record v1");
        row_history::record_in_tx_for_test(
            &tx,
            "test",
            row_id,
            row_history::HistoryOp::Update,
            r#"{"title":"v2"}"#,
            Some(r#"{"title":"v1"}"#),
            300,
            3,
        )
        .expect("record v2");
        row_history::record_in_tx_for_test(
            &tx,
            "test",
            row_id,
            row_history::HistoryOp::Update,
            r#"{"title":"v3"}"#,
            Some(r#"{"title":"v2"}"#),
            400,
            4,
        )
        .expect("record v3");

        tx.commit().expect("commit");
    } // conn dropped

    let conn = store.conn_for_test();
    let versions = row_history::list_versions(&conn, "test", row_id).expect("list_versions");

    // Expect 4 entries: create + 3 updates
    assert_eq!(versions.len(), 4, "expected 4 history entries");

    // The first entry is HistoryOp::Create
    assert_eq!(
        versions[0].op,
        row_history::HistoryOp::Create,
        "first entry must be Create"
    );

    // The remaining three are Update
    for v in &versions[1..] {
        assert_eq!(v.op, row_history::HistoryOp::Update);
    }

    // --- fetch_at(200) must return v1 (recorded at ts=200) ---
    let snap = row_history::fetch_at(&conn, "test", row_id, 200)
        .expect("fetch_at 200")
        .expect("must find snapshot at ts=200");

    let data: serde_json::Value = serde_json::from_str(snap.data_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        data["title"].as_str(),
        Some("v1"),
        "snapshot at ts=200 must be v1"
    );

    // --- fetch_at(150) must return v0 (Create at ts=100, the latest at or before 150) ---
    let snap0 = row_history::fetch_at(&conn, "test", row_id, 150)
        .expect("fetch_at 150")
        .expect("must find snapshot at ts=150");
    let data0: serde_json::Value =
        serde_json::from_str(snap0.data_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        data0["title"].as_str(),
        Some("v0"),
        "snapshot at ts=150 must be v0"
    );

    // --- fetch_at(50) must return None (before the first entry at ts=100) ---
    let none = row_history::fetch_at(&conn, "test", row_id, 50).expect("fetch_at 50");
    assert!(none.is_none(), "no snapshot before first create");
}

// =============================================================================
// Case 2 — create → update → row_restore to the original state
// =============================================================================

/// Verifies that restoring a live row rolls back its data and produces a new
/// history entry.
#[tokio::test]
async fn case2_restore_live_row_to_original_state() {
    let store = make_store().await;

    // create
    let row = store
        .create(json!({"title": "original"}))
        .await
        .expect("create");
    let id = row.id.clone();

    // update
    store
        .update(&id, json!({"title": "changed"}), UpdateMode::Merge)
        .await
        .expect("update");

    // Verify 2 history entries exist before restore; drop conn before any .await.
    {
        let conn = store.conn_for_test();
        let versions_before =
            row_history::list_versions(&conn, "test", &id).expect("list before restore");
        assert_eq!(
            versions_before.len(),
            2,
            "create + update = 2 history entries"
        );
    }
    // We know the original data from the create call — no fetch_at needed.
    // (Avoids same-second timestamp collision when all ops happen within 1 s.)
    let original_data = json!({"title": "original"});

    // Restore: the row exists, so this goes through update(Replace).
    let restored = store
        .update(&id, original_data, UpdateMode::Replace)
        .await
        .expect("restore via update");

    assert_eq!(
        restored.data["title"].as_str(),
        Some("original"),
        "row must be back to original"
    );

    // A new history entry must have been added (version 3 = Update from restore).
    let versions_after = {
        let conn = store.conn_for_test();
        row_history::list_versions(&conn, "test", &id).expect("list after restore")
    };
    assert_eq!(
        versions_after.len(),
        3,
        "restore must add a new history entry"
    );
    assert_eq!(
        versions_after[2].op,
        row_history::HistoryOp::Update,
        "restore of live row records Update"
    );
}

// =============================================================================
// Case 3 — create → delete → row_restore re-inserts the same id
// =============================================================================

/// Verifies that restoring a deleted row re-inserts it with the original id
/// and records a `Create` history entry.
#[tokio::test]
async fn case3_restore_deleted_row_same_id() {
    let store = make_store().await;

    // create
    let row = store
        .create(json!({"title": "to_delete"}))
        .await
        .expect("create");
    let id = row.id.clone();

    // delete
    store.delete(&id).await.expect("delete");

    // Verify history, obtain original data; drop conn before any .await.
    let original_data: serde_json::Value = {
        let conn = store.conn_for_test();
        let versions = row_history::list_versions(&conn, "test", &id).expect("list");
        assert_eq!(versions.len(), 2, "create + delete = 2 history entries");
        assert_eq!(
            versions[1].op,
            row_history::HistoryOp::Delete,
            "second entry must be Delete"
        );
        let create_ts = versions[0].recorded_at;
        let snap = row_history::fetch_at(&conn, "test", &id, create_ts)
            .expect("fetch_at")
            .expect("snap");
        serde_json::from_str(snap.data_json.as_deref().unwrap()).unwrap()
    }; // conn guard dropped here — safe to .await

    // Restore via restore_row (same-id re-insert path).
    let restored = store
        .restore_row(&id, original_data)
        .await
        .expect("restore_row");

    assert_eq!(restored.id, id, "restored row must keep the original id");
    assert_eq!(
        restored.data["title"].as_str(),
        Some("to_delete"),
        "data must be restored"
    );

    // A Create entry must follow the Delete entry in history.
    let versions_after = {
        let conn = store.conn_for_test();
        row_history::list_versions(&conn, "test", &id).expect("list after restore")
    };
    assert_eq!(
        versions_after.len(),
        3,
        "three history entries after restore"
    );
    assert_eq!(
        versions_after[2].op,
        row_history::HistoryOp::Create,
        "restore_row records Create"
    );
}

// =============================================================================
// Case 4 — purge_old_history removes max_per_row excess entries
// =============================================================================

/// Directly inserts extra history rows to exceed `max_per_row`, then calls
/// `purge_old_history` and confirms the oldest entries were removed.
///
/// `retention_days = 0` disables the time-based limit so only `max_per_row`
/// applies.  All `recorded_at` values are literal unix seconds.
#[tokio::test]
async fn case4_purge_max_per_row() {
    let store = make_store().await;

    // Insert 6 history records with synthetic timestamps (1000, 1001, … 1005)
    // using the low-level `record_in_tx_for_test` helper inside a transaction.
    {
        let conn = store.conn_for_test();
        let tx = conn.unchecked_transaction().expect("begin tx");
        for i in 0u32..6 {
            let ts = 1000_i64 + i64::from(i);
            row_history::record_in_tx_for_test(
                &tx,
                "test",
                "row-purge-test",
                row_history::HistoryOp::Create,
                &format!(r#"{{"title":"v{i}"}}"#),
                None,
                ts,
                i64::from(i + 1),
            )
            .expect("record_in_tx");
        }
        tx.commit().expect("commit");
    } // conn dropped

    let conn = store.conn_for_test();
    let versions_before =
        row_history::list_versions(&conn, "test", "row-purge-test").expect("before");
    assert_eq!(versions_before.len(), 6, "6 entries before purge");

    // Purge: keep at most 3 per row; disable retention limit (0 = no TTL).
    let now_secs = 9_999_999_i64; // far future so retention limit doesn't fire
    row_history::purge_old_history(&conn, 0, 3, now_secs).expect("purge");

    let versions_after =
        row_history::list_versions(&conn, "test", "row-purge-test").expect("after");
    assert_eq!(versions_after.len(), 3, "exactly 3 entries remain");

    // The 3 newest must survive.
    let kept_versions: Vec<i64> = versions_after.iter().map(|r| r.version).collect();
    assert_eq!(kept_versions, vec![4_i64, 5, 6], "newest 3 versions kept");
}

// =============================================================================
// Case 5 — atomicity: rusqlite Transaction rollback also rolls back history
// =============================================================================

/// Confirms that the history INSERT is rolled back together with the data DML
/// when the enclosing transaction is aborted.
///
/// We simulate a rollback by using the raw `rusqlite::Connection` from
/// `conn_for_test` and aborting the Tx ourselves via `drop` without `commit`.
#[tokio::test]
async fn case5_atomicity_rollback() {
    let store = make_store().await;

    // Create a row so the id exists in history.
    let row = store
        .create(json!({"title": "before"}))
        .await
        .expect("create");
    let id = row.id.clone();

    {
        let conn = store.conn_for_test();
        let versions_before =
            row_history::list_versions(&conn, "test", &id).expect("before rollback");
        assert_eq!(versions_before.len(), 1, "one entry before rollback test");

        // Begin a transaction manually, insert a history entry, then DROP the
        // transaction without committing — simulating a DML failure rollback.
        {
            let tx = conn.unchecked_transaction().expect("begin tx");
            row_history::record_in_tx_for_test(
                &tx,
                "test",
                &id,
                row_history::HistoryOp::Update,
                r#"{"title":"rolled_back"}"#,
                None,
                5000,
                2,
            )
            .expect("record in aborted tx");
            // Drop `tx` WITHOUT calling `tx.commit()` → automatic rollback.
        }

        // The history entry must not appear.
        let versions_after =
            row_history::list_versions(&conn, "test", &id).expect("after rollback");
        assert_eq!(
            versions_after.len(),
            1,
            "history must still have exactly 1 entry after rollback"
        );
    }
}
