// This file is used by trybuild to verify that `Arc<rusqlite::Connection>`
// cannot be sent to a tokio::spawn closure because `Connection` is `!Sync`.
//
// Expected: compile error — `Arc<Connection>` is `!Send` because
// `Connection: !Sync`, and `tokio::spawn` requires a `Send` future.
fn main() {
    let conn = std::sync::Arc::new(rusqlite::Connection::open_in_memory().unwrap());
    // Arc<T>: Send requires T: Send + Sync.
    // rusqlite::Connection: Send + !Sync, therefore Arc<Connection>: !Send.
    // The async block below captures `conn`, making the future !Send,
    // which violates tokio::spawn's bound.
    let _handle = tokio::spawn(async move {
        let _c = conn;
    });
}
