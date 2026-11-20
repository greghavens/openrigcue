//! `TelemetryMod:createPipe(dt)`: when to (re)open the pipe.
//!
//! `Context.updateCount` starts at 0.  On each update: if it is 0, the
//! current handle (if any) is flushed and closed and `io.open(pipeName, "w")`
//! is called (its result, possibly nil, becomes the handle); then the counter
//! is incremented and wrapped from 256 to 0.  So the open happens on updates
//! 1, 257, 513, ... (period [`REOPEN_PERIOD`]).  A frame is written in the
//! same update whenever the handle is non-nil.

/// Updates between two `io.open` calls.
pub const REOPEN_PERIOD: u32 = 256;

/// What `createPipe` does in one update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeAction {
    /// Nothing; keep the current handle.
    Keep,
    /// Call `io.open`; `close_previous` = flush + close the old handle first.
    Open { close_previous: bool },
}

/// The counter and handle-presence state of `createPipe`.
#[derive(Clone, Debug, Default)]
pub struct PipeSchedule {
    update_count: u32,
    has_handle: bool,
}

impl PipeSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance one update. `open_succeeds` tells whether `io.open` would
    /// return a handle if it is called this update. Returns the action and
    /// whether a frame is written this update.
    pub fn update(&mut self, open_succeeds: bool) -> (PipeAction, bool) {
        let action = if self.update_count == 0 {
            let close_previous = self.has_handle;
            self.has_handle = open_succeeds;
            PipeAction::Open { close_previous }
        } else {
            PipeAction::Keep
        };
        self.update_count += 1;
        if self.update_count == REOPEN_PERIOD {
            self.update_count = 0;
        }
        (action, self.has_handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_on_1_257_513() {
        let mut s = PipeSchedule::new();
        let mut opens = vec![];
        for n in 1..=600u32 {
            if let (PipeAction::Open { close_previous }, _) = s.update(true) {
                opens.push((n, close_previous));
            }
        }
        assert_eq!(opens, vec![(1, false), (257, true), (513, true)]);
    }

    #[test]
    fn failed_open_suppresses_frames_until_next_slot() {
        let mut s = PipeSchedule::new();
        assert_eq!(s.update(false), (PipeAction::Open { close_previous: false }, false));
        for _ in 2..=256 {
            assert_eq!(s.update(true), (PipeAction::Keep, false));
        }
        assert_eq!(s.update(true), (PipeAction::Open { close_previous: false }, true));
    }
}
