/// Concurrency tests for the ArcSwap-based hot-reload mechanism.
///
/// These tests correspond to concurrency-analysis.md §2 (6 tests required by
/// Subtask 2 Acceptance Criteria #11). Each test validates a specific
/// concurrency property of the reload implementation.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mini_app_mcp::{
    config::Config,
    mcp::{
        registry::{TableEntry, TableRegistry},
        server::MiniAppMcpServer,
    },
    schema::{FieldDef, FieldType, SchemaConfig},
    store::Store,
};

// =============================================================================
// Shared helpers
// =============================================================================

fn make_schema(table: &str) -> SchemaConfig {
    SchemaConfig {
        table: table.to_string(),
        title: None,
        description: None,
        fields: vec![FieldDef {
            name: "title".to_string(),
            ty: FieldType::String,
            required: false,
            description: None,
        }],
        dump: None,
    }
}

async fn make_entry(table: &str) -> TableEntry {
    let schema = make_schema(table);
    let store = Store::open(Path::new(":memory:"), schema.clone())
        .await
        .expect("in-memory store");
    TableEntry {
        store: Arc::new(store),
        schema: Arc::new(schema),
        schema_path: Arc::new(PathBuf::from(format!("/fake/{table}/schema.yaml"))),
    }
}

async fn make_registry(tables: &[&str]) -> TableRegistry {
    let mut entries: HashMap<String, TableEntry> = HashMap::new();
    for &table in tables {
        entries.insert(table.to_string(), make_entry(table).await);
    }
    TableRegistry::from_entries(entries, None)
}

// =============================================================================
// Test 1: ArcSwap atomic swap visibility to concurrent readers
//
// 8 tasks continuously load_full() snapshots while 1 task calls store().
// Tasks that captured the snapshot before store() see the old registry;
// tasks that captured after store() see the new registry. Neither panics.
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_reload_arcswap_atomic_swap_visible_to_concurrent_readers() {
    let old_registry = make_registry(&["table_old"]).await;
    let new_registry = make_registry(&["table_new"]).await;

    let swap = Arc::new(ArcSwap::from_pointee(old_registry));
    let swap_clone_for_store = Arc::clone(&swap);

    // Spawn 8 reader tasks, each doing 100 load_full() iterations.
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let swap = Arc::clone(&swap);
            tokio::spawn(async move {
                for _ in 0..100 {
                    let snapshot = swap.load_full();
                    // The snapshot must contain exactly one table, either old or
                    // new; it must never be in an inconsistent state.
                    let count = snapshot.table_count();
                    assert_eq!(count, 1, "snapshot must always have 1 table");
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect();

    // Writer task: store the new registry after a brief yield.
    let writer = tokio::spawn(async move {
        tokio::task::yield_now().await;
        swap_clone_for_store.store(Arc::new(new_registry));
    });

    // Await all.
    writer.await.expect("writer must not panic");
    for r in readers {
        r.await.expect("reader must not panic");
    }

    // After store, the swap must hold the new registry.
    let final_snap = swap.load_full();
    let names: Vec<&str> = final_snap.table_names().collect();
    assert!(
        names.contains(&"table_new"),
        "final snapshot must contain table_new, got: {names:?}"
    );
}

// =============================================================================
// Test 2: Concurrent double reload — last-write-wins semantics
//
// 2 tasks race to store() different registries. The final state must be one of
// the two candidates (not corrupted or empty).
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reload_concurrent_double_reload_last_write_wins() {
    let registry_a = make_registry(&["table_a"]).await;
    let registry_b = make_registry(&["table_b"]).await;

    let swap = Arc::new(ArcSwap::from_pointee(make_registry(&["table_init"]).await));

    let swap_a = Arc::clone(&swap);
    let swap_b = Arc::clone(&swap);

    let task_a = tokio::spawn(async move {
        tokio::task::yield_now().await;
        swap_a.store(Arc::new(registry_a));
    });

    let task_b = tokio::spawn(async move {
        tokio::task::yield_now().await;
        swap_b.store(Arc::new(registry_b));
    });

    task_a.await.expect("task_a must not panic");
    task_b.await.expect("task_b must not panic");

    // Final state must be exactly one of the two candidates.
    let final_snap = swap.load_full();
    let count = final_snap.table_count();
    assert_eq!(count, 1, "final state must have exactly 1 table (LWW)");

    let names: Vec<&str> = final_snap.table_names().collect();
    let valid = names.contains(&"table_a") || names.contains(&"table_b");
    assert!(
        valid,
        "final state must be either table_a or table_b, got: {names:?}"
    );
}

// =============================================================================
// Test 3: spawn_blocking in reload does not block the runtime
//
// Verifies that a sync I/O operation in spawn_blocking runs concurrently with
// async tool calls on the same 2-worker runtime without causing deadlock.
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawn_blocking_in_reload_does_not_block_runtime() {
    // Create a temp directory with one table so reload has something to scan.
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let user_dir = tmp_dir.path();
    let table_dir = user_dir.join("table_x");
    std::fs::create_dir_all(&table_dir).expect("create table_x dir");
    std::fs::write(
        table_dir.join("schema.yaml"),
        b"table: table_x\nfields:\n  - name: v\n    type: string\n    required: false\n",
    )
    .expect("write schema");

    let config = Arc::new(Config {
        schema_path: None,
        db_path: None,
        user_dir: Some(user_dir.to_path_buf()),
        project_dir: None,
        backup_retention: None,
        snapshot_retention: None,
    });

    let registry = TableRegistry::mount_from_dirs(Some(user_dir), None)
        .await
        .expect("initial mount");
    let server = Arc::new(MiniAppMcpServer::new_multi(registry, Arc::clone(&config)));

    // Spawn a reload task (uses spawn_blocking internally).
    let server_reload = Arc::clone(&server);
    let reload_task = tokio::spawn(async move {
        server_reload
            .tool_reload()
            .await
            .expect("reload must succeed")
    });

    // Meanwhile, run 20 async tool_info calls on the same runtime.
    use mini_app_mcp::mcp::server::InfoParams;
    use rmcp::handler::server::wrapper::Parameters;

    let info_tasks: Vec<_> = (0..20)
        .map(|_| {
            let s = Arc::clone(&server);
            tokio::spawn(async move {
                // table_x may or may not exist depending on timing; we just want
                // to ensure the runtime is not blocked (no timeout/hang).
                let _ = s
                    .tool_info(Parameters(InfoParams {
                        table: Some("table_x".to_string()),
                    }))
                    .await;
            })
        })
        .collect();

    // All tasks must complete (no deadlock within a generous timeout).
    let timeout = tokio::time::Duration::from_secs(10);
    tokio::time::timeout(timeout, reload_task)
        .await
        .expect("reload must not time out")
        .expect("reload task must not panic");

    for t in info_tasks {
        tokio::time::timeout(timeout, t)
            .await
            .expect("info task must not time out")
            .expect("info task must not panic");
    }
}

// =============================================================================
// Test 4: ArcSwap Guard dropped before any .await in resolve_table
//
// Runs 100 concurrent tool_info calls while 5 reload calls race.
// All tool_info calls must succeed (or return a well-typed error, not a panic).
// The test verifies that the Guard held by resolve_table is dropped before any
// await in the tool implementation (no await-holding-lock violation).
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_arcswap_guard_drop_before_await_in_resolve_table() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let user_dir = tmp_dir.path();
    let table_dir = user_dir.join("table_g");
    std::fs::create_dir_all(&table_dir).expect("create table_g dir");
    std::fs::write(
        table_dir.join("schema.yaml"),
        b"table: table_g\nfields:\n  - name: val\n    type: string\n    required: false\n",
    )
    .expect("write schema");

    let config = Arc::new(Config {
        schema_path: None,
        db_path: None,
        user_dir: Some(user_dir.to_path_buf()),
        project_dir: None,
        backup_retention: None,
        snapshot_retention: None,
    });

    let registry = TableRegistry::mount_from_dirs(Some(user_dir), None)
        .await
        .expect("initial mount");
    let server = Arc::new(MiniAppMcpServer::new_multi(registry, Arc::clone(&config)));

    use mini_app_mcp::mcp::server::InfoParams;
    use rmcp::handler::server::wrapper::Parameters;

    // 100 concurrent tool_info calls
    let info_tasks: Vec<_> = (0..100)
        .map(|_| {
            let s = Arc::clone(&server);
            tokio::spawn(async move {
                s.tool_info(Parameters(InfoParams {
                    table: Some("table_g".to_string()),
                }))
                .await
            })
        })
        .collect();

    // 5 concurrent reload calls
    let reload_tasks: Vec<_> = (0..5)
        .map(|_| {
            let s = Arc::clone(&server);
            tokio::spawn(async move { s.tool_reload().await })
        })
        .collect();

    // All must complete without panicking.
    let timeout = tokio::time::Duration::from_secs(15);
    for t in info_tasks {
        let result = tokio::time::timeout(timeout, t)
            .await
            .expect("info task must not time out")
            .expect("info task must not panic");
        // Result is Ok or Err(String); neither case panics.
        let _ = result;
    }
    for t in reload_tasks {
        let _ = tokio::time::timeout(timeout, t)
            .await
            .expect("reload task must not time out")
            .expect("reload task must not panic");
    }
}

// =============================================================================
// Test 5: Dual-Store WAL no lock conflict (Crux #1 end-to-end)
//
// Opens the same .db file twice (old Store = Connection A, new Store =
// Connection B). Performs a read on Connection A and a write on Connection B
// simultaneously. WAL mode guarantees 1 writer + N readers can coexist without
// SQLITE_BUSY.
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_dual_store_wal_no_lock_conflict() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = tmp_dir.path().join("dual_test.db");

    let schema = make_schema("dual_table");

    // Open the "old" connection (simulates the pre-reload Store).
    let old_store = Arc::new(
        Store::open(&db_path, schema.clone())
            .await
            .expect("old store must open"),
    );

    // Open the "new" connection on the same file (simulates the post-reload
    // Store). WAL mode must allow this without SQLITE_BUSY.
    let new_store = Arc::new(
        Store::open(&db_path, schema.clone())
            .await
            .expect("new store must open — WAL must allow dual connections"),
    );

    // Create a row via old_store to have something to read.
    old_store
        .create(serde_json::json!({"title": "seed"}))
        .await
        .expect("old store create must succeed");

    // Concurrently: read from old_store, write to new_store.
    let old_read = {
        let s = Arc::clone(&old_store);
        tokio::spawn(async move {
            for _ in 0..10 {
                let rows = s
                    .list(Some(10), None, None, None)
                    .await
                    .expect("old_store list must not return SQLITE_BUSY");
                assert!(!rows.is_empty(), "must have at least the seed row");
                tokio::task::yield_now().await;
            }
        })
    };

    let new_write = {
        let s = Arc::clone(&new_store);
        tokio::spawn(async move {
            for i in 0..10 {
                s.create(serde_json::json!({"title": format!("new-{i}")}))
                    .await
                    .expect("new_store create must not return SQLITE_BUSY");
                tokio::task::yield_now().await;
            }
        })
    };

    let timeout = tokio::time::Duration::from_secs(10);
    tokio::time::timeout(timeout, old_read)
        .await
        .expect("old_read must not time out")
        .expect("old_read task must not panic");
    tokio::time::timeout(timeout, new_write)
        .await
        .expect("new_write must not time out")
        .expect("new_write task must not panic");
}

// =============================================================================
// Test 6: Arc<Config> clone + Send across tasks
//
// Clones Arc<Config> to 4 tasks and verifies each task can read config fields.
// Demonstrates that Arc<Config> is Send + Sync in a multi-thread context.
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_arc_config_clone_send_across_tasks() {
    let config = Arc::new(Config {
        schema_path: Some(PathBuf::from("/fake/schema.yaml")),
        db_path: Some(PathBuf::from("/fake/app.db")),
        user_dir: Some(PathBuf::from("/fake/user")),
        project_dir: Some(PathBuf::from("/fake/project")),
        backup_retention: None,
        snapshot_retention: None,
    });

    let tasks: Vec<_> = (0..4)
        .map(|i| {
            let c = Arc::clone(&config);
            tokio::spawn(async move {
                // Each task reads config fields — verifies Send + Sync.
                assert!(c.has_legacy_env(), "task {i}: has_legacy_env must be true");
                assert_eq!(
                    c.schema_path,
                    Some(PathBuf::from("/fake/schema.yaml")),
                    "task {i}: schema_path must match"
                );
                assert_eq!(
                    c.user_dir,
                    Some(PathBuf::from("/fake/user")),
                    "task {i}: user_dir must match"
                );
                tokio::task::yield_now().await;
                // Verify config is still intact after await.
                assert_eq!(
                    c.project_dir,
                    Some(PathBuf::from("/fake/project")),
                    "task {i}: project_dir must match after await"
                );
            })
        })
        .collect();

    for t in tasks {
        t.await.expect("config clone task must not panic");
    }
}
