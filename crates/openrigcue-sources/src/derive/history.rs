//! The chart's shared typed sample list, independent of listener and dispatcher waits.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use openrigcue_model::native_value::LegacyValue;

#[derive(Debug, Clone, Default)]
pub struct TelemetryHistory(Arc<Mutex<VecDeque<LegacyValue>>>);

impl TelemetryHistory {
    pub fn snapshot(&self) -> VecDeque<LegacyValue> {
        self.lock().clone()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }

    pub(super) fn pop_front(&self) -> Option<LegacyValue> {
        self.lock().pop_front()
    }

    pub(super) fn push_back(&self, value: LegacyValue) {
        self.lock().push_back(value);
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<LegacyValue>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
