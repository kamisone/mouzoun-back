//! Injectable clock: `Utc::now()` in production; tests pin it to make time-dependent behaviour
//! (scheduled sections, scheduled publishing) deterministic.

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Default)]
pub struct Clock(Arc<RwLock<Option<DateTime<Utc>>>>);

impl Clock {
    pub fn now(&self) -> DateTime<Utc> {
        self.0
            .read()
            .ok()
            .and_then(|fixed| *fixed)
            .unwrap_or_else(Utc::now)
    }

    /// Pins the clock (`None` = back to real time).
    pub fn set(&self, at: Option<DateTime<Utc>>) {
        if let Ok(mut fixed) = self.0.write() {
            *fixed = at;
        }
    }
}
