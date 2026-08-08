//! Driver registry (design §2.8 / §3): registration costs a hashmap insert
//! and constructs nothing; the driver is built lazily on first `get` and
//! cached. `datagrep-core` never names a concrete driver — this registry is the
//! only coupling.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

use datagrep_api::Driver;

use crate::{read, write};

type DriverCtor = Box<dyn Fn() -> Arc<dyn Driver> + Send + Sync>;

/// One registered driver: the lazy constructor plus the once-built instance.
struct Entry {
    ctor: DriverCtor,
    cell: OnceLock<Arc<dyn Driver>>,
}

/// Thread-safe id → driver map with lazy, at-most-once construction per entry.
pub struct DriverRegistry {
    entries: RwLock<HashMap<Arc<str>, Arc<Entry>>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Register a driver id with a lazy constructor. §2.8: this is a hashmap
    /// insert — nothing is constructed until the first [`DriverRegistry::get`].
    /// Re-registering an id replaces the entry (and forgets any built instance).
    pub fn register(
        &self,
        id: impl Into<Arc<str>>,
        ctor: impl Fn() -> Arc<dyn Driver> + Send + Sync + 'static,
    ) {
        let entry = Arc::new(Entry {
            ctor: Box::new(ctor),
            cell: OnceLock::new(),
        });
        write(&self.entries).insert(id.into(), entry);
    }

    /// Fetch a driver, constructing it on first use. Construction runs outside
    /// the map lock, so a slow constructor never blocks other lookups.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Driver>> {
        let entry = read(&self.entries).get(id).cloned()?;
        Some(entry.cell.get_or_init(|| (entry.ctor)()).clone())
    }

    /// Registered ids, unordered.
    pub fn ids(&self) -> Vec<Arc<str>> {
        read(&self.entries).keys().cloned().collect()
    }
}

impl Default for DriverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DriverRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockDriver;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn registration_constructs_nothing_and_get_constructs_once() {
        let built = Arc::new(AtomicUsize::new(0));
        let reg = DriverRegistry::new();
        let b = built.clone();
        reg.register("mock", move || {
            b.fetch_add(1, Ordering::SeqCst);
            Arc::new(MockDriver::new())
        });
        assert_eq!(
            built.load(Ordering::SeqCst),
            0,
            "§2.8: register builds nothing"
        );

        let d1 = reg.get("mock").expect("registered");
        let d2 = reg.get("mock").expect("registered");
        assert_eq!(built.load(Ordering::SeqCst), 1, "constructed exactly once");
        assert!(Arc::ptr_eq(&d1, &d2), "same cached instance");
        assert!(reg.get("nope").is_none());
    }
}
