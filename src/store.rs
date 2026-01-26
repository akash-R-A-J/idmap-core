use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type ShareStore<T> =
    Arc<RwLock<HashMap<(u64, String), T>>>;

pub async fn put<T>(
    store: &ShareStore<T>,
    id: u64,
    session: &str,
    value: T,
) {
    let mut s = store.write().await;
    s.insert((id, session.to_string()), value);
}

pub async fn get<T: Clone>(
    store: &ShareStore<T>,
    id: u64,
    session: &str,
) -> Option<T> {
    let s = store.read().await;
    s.get(&(id, session.to_string())).cloned()
}
