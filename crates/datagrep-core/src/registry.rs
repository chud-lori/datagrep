use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

use datagrep_api::Driver;

use crate::{read, write};

type DriverCtor = Box<dyn Fn() -> Arc<dyn Driver> + Send + Sync>;

struct Entry {
    ctor: DriverCtor,
    cell: OnceLock<Arc<dyn Driver>>,
}

pub struct DriverRegistry {
    entries: RwLock<HashMap<Arc<str>, Arc<Entry>>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

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

    pub fn get(&self, id: &str) -> Option<Arc<dyn Driver>> {
        let entry = read(&self.entries).get(id).cloned()?;
        Some(entry.cell.get_or_init(|| (entry.ctor)()).clone())
    }

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
            "register must build nothing"
        );

        let d1 = reg.get("mock").expect("registered");
        let d2 = reg.get("mock").expect("registered");
        assert_eq!(built.load(Ordering::SeqCst), 1, "constructed exactly once");
        assert!(Arc::ptr_eq(&d1, &d2), "same cached instance");
        assert!(reg.get("nope").is_none());
    }
}
