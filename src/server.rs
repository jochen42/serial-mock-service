// Server-wide state: the live set of `PortState`s indexed by name.
//
// The port set is mutable across reloads, but the *handles* held by
// the HTTP routes never move — they're behind an `Arc`. Reload swaps
// the inner map under a write lock.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::port::PortState;

pub struct Server {
    pub ports: PortMap,
}

/// Wrapper that lets HTTP handlers use `server.ports.get(name)` /
/// `.values()` without thinking about the RwLock.
pub struct PortMap {
    inner: RwLock<HashMap<String, Arc<PortState>>>,
}

impl PortMap {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<PortState>> {
        self.inner.read().unwrap().get(name).cloned()
    }

    pub fn values(&self) -> Vec<Arc<PortState>> {
        self.inner.read().unwrap().values().cloned().collect()
    }

    pub fn insert(&self, state: Arc<PortState>) {
        self.inner
            .write()
            .unwrap()
            .insert(state.name.clone(), state);
    }

    pub fn remove(&self, name: &str) -> Option<Arc<PortState>> {
        self.inner.write().unwrap().remove(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.inner.read().unwrap().keys().cloned().collect()
    }
}
