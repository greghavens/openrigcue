//! The packet-counter liveness test of the Kunos listeners.

/// Unchanged packet ids after which the game counts as gone.
const UNCHANGED_LIMIT: u32 = 50;

/// The previous-packet-id and tolerance logic shared by the `ac` and `acevo` liveness tests.
#[derive(Debug, Default, Clone)]
pub(crate) struct PacketLiveness {
    previous: Option<i32>,
    unchanged: u32,
}

impl PacketLiveness {
    /// Records the current packet id and tells whether the game is alive: never on the first observation, always
    /// when the id changed, and while fewer than 50 consecutive observations repeated the previous id.
    pub(crate) fn observe(&mut self, packet_id: i32) -> bool {
        let alive = match self.previous {
            None => false,
            Some(previous) if previous != packet_id => {
                self.unchanged = 0;
                true
            }
            Some(_) => {
                self.unchanged += 1;
                self.unchanged < UNCHANGED_LIMIT
            }
        };
        self.previous = Some(packet_id);
        alive
    }

    /// Forgets the previous packet id (EVO after a failed refresh); the unchanged count is kept.
    pub(crate) fn forget_packet(&mut self) {
        self.previous = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance: 49 repeats stay alive, the 50th does not; a new id resets the count.
    #[test]
    fn tolerance_boundary() {
        let mut liveness = PacketLiveness::default();
        assert!(!liveness.observe(5));
        for _ in 0..49 {
            assert!(liveness.observe(5));
        }
        assert!(!liveness.observe(5));
        assert!(!liveness.observe(5));
        assert!(liveness.observe(6));
        assert!(liveness.observe(6));
    }

    /// EVO: after a reset the next observation is false again, and the tolerance
    /// count carries over.
    #[test]
    fn reset_keeps_tolerance() {
        let mut liveness = PacketLiveness::default();
        liveness.observe(1);
        for _ in 0..48 {
            liveness.observe(1);
        }
        liveness.forget_packet();
        assert!(!liveness.observe(1));
        assert!(liveness.observe(1)); // 49th unchanged observation in total
        assert!(!liveness.observe(1)); // 50th
    }
}
