/// Verifies that `Arc<rusqlite::Connection>` cannot be sent into a
/// `tokio::spawn` closure because [`rusqlite::Connection`] is `!Sync`.
///
/// This test documents the Crux-required design constraint: `Arc<Mutex<Connection>>`
/// is the only legal way to share a `Connection` across async tasks.
#[test]
fn connection_not_sync() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/arc_connection_no_sync.rs");
}
