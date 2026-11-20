//! One-shot in-prefix desktop automation protocol. No telemetry workers are started.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selector {
    Name { value: String, depth: u32 },
    AutomationId { value: String, depth: u32 },
    Child { index: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Element {
    pub window: String,
    pub path: Vec<Selector>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Request {
    Titles,
    Launch { command: String },
    Exists { element: Element },
    Name { element: Element },
    Children { element: Element },
    Activate { element: Element },
    Focus { element: Element },
    Topmost { element: Element },
    Click { element: Element },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<u64>,
}

/// A routine-local control graph. IDs are allocated by the helper and own actual
/// native elements; independently constructed selectors have independent caches.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum SessionRequest {
    Root { window: String },
    Child { parent: u64, selector: Selector },
    Exists { control: u64, timeout_ms: u64, interval_ms: u64 },
    Execute { control: u64, action: ControlAction },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Name,
    Children,
    Activate,
    Focus,
    Topmost,
    Click,
}

/// Existence check: search first, then wait at most the remaining search time.
/// Callers supply the clock and sleep so the same policy covers real and fake desktops.
pub fn wait_for<T, E>(
    mut search: impl FnMut() -> Result<Option<T>, E>,
    timeout: std::time::Duration,
    interval: std::time::Duration,
    mut elapsed: impl FnMut() -> std::time::Duration,
    mut sleep: impl FnMut(std::time::Duration),
) -> Result<Option<T>, E> {
    loop {
        if let Some(value) = search()? {
            return Ok(Some(value));
        }
        let remaining = timeout.saturating_sub(elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        sleep(interval.min(remaining));
    }
}

#[cfg(test)]
mod search_tests {
    use super::wait_for;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    #[test]
    fn control_search_waits_for_a_tab_control_that_appears_later() {
        let elapsed = Cell::new(Duration::ZERO);
        let attempts = Cell::new(0);
        let result = wait_for(
            || {
                attempts.set(attempts.get() + 1);
                Ok::<_, &str>((elapsed.get() >= Duration::from_millis(500)).then_some("Tuning Menu"))
            },
            Duration::from_secs(1),
            Duration::from_millis(500),
            || elapsed.get(),
            |delay| elapsed.set(elapsed.get() + delay),
        );
        assert_eq!(result, Ok(Some("Tuning Menu")));
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn control_search_caps_the_last_sleep_and_checks_at_the_deadline() {
        let elapsed = Cell::new(Duration::ZERO);
        let waits = RefCell::new(Vec::new());
        let attempts = Cell::new(0);
        let result = wait_for(
            || {
                attempts.set(attempts.get() + 1);
                elapsed.set(elapsed.get() + Duration::from_millis(700));
                Ok::<Option<()>, &str>(None)
            },
            Duration::from_secs(3),
            Duration::from_secs(1),
            || elapsed.get(),
            |delay| {
                waits.borrow_mut().push(delay);
                elapsed.set(elapsed.get() + delay);
            },
        );
        assert_eq!(result, Ok(None));
        assert_eq!(*waits.borrow(), [Duration::from_secs(1), Duration::from_millis(600)]);
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn control_search_propagates_provider_errors_without_retrying() {
        let result = wait_for::<(), _>(
            || Err("COM failure"),
            Duration::from_secs(1),
            Duration::from_millis(500),
            || panic!("failed search must not read the clock"),
            |_| panic!("failed search must not sleep"),
        );
        assert_eq!(result, Err("COM failure"));
    }
}
