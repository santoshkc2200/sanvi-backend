use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use sanvi_kernel::UserId;

use crate::domain::ports::{Decision, DecisionCache};

struct Entry {
    decision: Decision,
    inserted: Instant,
    ttl_secs: u64,
}

impl Entry {
    fn expired(&self) -> bool {
        self.inserted.elapsed().as_secs() >= self.ttl_secs
    }
}

pub struct InMemoryDecisionCache {
    entries: Mutex<HashMap<String, Entry>>,
    max_entries: usize,
}

impl InMemoryDecisionCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_entries,
        }
    }
}

#[async_trait]
impl DecisionCache for InMemoryDecisionCache {
    async fn get(&self, key: &str) -> Option<Decision> {
        let mut entries = self.entries.lock().expect("cache mutex poisoned");
        let expired = entries.get(key).is_some_and(Entry::expired);
        if expired {
            entries.remove(key);
            return None;
        }
        entries.get(key).map(|entry| entry.decision)
    }

    async fn put(&self, key: &str, decision: Decision, ttl_secs: u64) {
        let mut entries = self.entries.lock().expect("cache mutex poisoned");
        if entries.len() >= self.max_entries {
            entries.retain(|_, entry| !entry.expired());
            if entries.len() >= self.max_entries {
                // Drop an arbitrary entry to stay bounded (cache is a pure
                // optimisation; dropping is always correct).
                let key_to_drop = entries.keys().next().cloned();
                if let Some(key_to_drop) = key_to_drop {
                    entries.remove(&key_to_drop);
                }
            }
        }
        entries.insert(
            key.to_string(),
            Entry {
                decision,
                inserted: Instant::now(),
                ttl_secs,
            },
        );
    }

    async fn invalidate_user(&self, user_id: UserId) {
        let prefix = format!("{user_id}:");
        self.entries
            .lock()
            .expect("cache mutex poisoned")
            .retain(|key, _| !key.starts_with(&prefix));
    }

    async fn invalidate_all(&self) {
        self.entries.lock().expect("cache mutex poisoned").clear();
    }
}

/// A cache that never stores (used when the identity feature flag is off or
/// in tests that assert on uncached behaviour).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopDecisionCache;

#[async_trait]
impl DecisionCache for NoopDecisionCache {
    async fn get(&self, _key: &str) -> Option<Decision> {
        None
    }

    async fn put(&self, _key: &str, _decision: Decision, _ttl_secs: u64) {}

    async fn invalidate_user(&self, _user_id: UserId) {}

    async fn invalidate_all(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn entries_expire_after_ttl() {
        let cache = InMemoryDecisionCache::new(10);
        cache.put("k", Decision::Allowed, 60).await;
        assert_eq!(cache.get("k").await, Some(Decision::Allowed));
        // TTL 0 expires immediately (the acceptance window never holds
        // a decision longer than its TTL).
        cache.put("k", Decision::Allowed, 0).await;
        assert_eq!(cache.get("k").await, None);
    }

    #[tokio::test]
    async fn invalidation_drops_all_entries_for_a_user() {
        let cache = InMemoryDecisionCache::new(10);
        let user = UserId::try_from(101_i64).unwrap();
        cache
            .put(&format!("{user}:a:b"), Decision::Allowed, 60)
            .await;
        cache
            .put(&format!("{user}:c:d"), Decision::Denied, 60)
            .await;
        cache.put("other:user:key", Decision::Allowed, 60).await;
        cache.invalidate_user(user).await;
        assert_eq!(cache.get(&format!("{user}:a:b")).await, None);
        assert_eq!(cache.get(&format!("{user}:c:d")).await, None);
        assert_eq!(cache.get("other:user:key").await, Some(Decision::Allowed));
    }

    #[tokio::test]
    async fn capacity_is_bounded() {
        let cache = InMemoryDecisionCache::new(2);
        cache.put("a", Decision::Allowed, 60).await;
        cache.put("b", Decision::Allowed, 60).await;
        cache.put("c", Decision::Allowed, 60).await;
        assert!(cache.entries.lock().unwrap().len() <= 2);
    }
}
