//! The "test effect" and "test speaker" buttons.

use std::time::Duration;

use super::rig::{ShakeInput, WheelValues};

/// Gap between two test frames (1/15 s).
pub const TEST_FRAME_GAP: Duration = Duration::from_nanos(66_666_667);

/// One canned frame of the effect test.
///
/// The effect test stores the test listener (the running one, `'test'` when none runs) as the shared
/// running-listener value right before each of the first 12 frames and stores the real
/// listener back right after the 12th (before the last pause), so the 13th frame sees the
/// listener the test started with. Every other reader of that value (the shaker loop's
/// telemetry frames, the shift beep, the motion and accessory loops) sees the test listener until
/// the game-listener thread writes its own value over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TestFrame {
    pub input: ShakeInput,
    /// The test listener is stored right before this frame.
    pub forced_active: bool,
    /// After this frame the test listener is left stored (frames 1-11); `false` after the
    /// 12th, when the real listener is stored back, and after the 13th.
    pub forced_after: bool,
}

/// `(rpm, gear, travel, wheel slip, lateral G, vertical G, longitudinal G, lateral velocity, canopy)`
/// of the 13 test frames; speed 100, max rpm 8700, shock velocity 0.101 and
/// rumble 0 are common to all.
type Row = (f64, f64, f64, [f64; 4], f64, f64, f64, f64, f64);

const ROWS: [Row; 13] = [
    (7378.0, 1.0, 0.09942309558391571, [1.0, 2.0, 3.0, 4.0], -12.681578397750854, 66.0, 4.0, -2.0, 1.0),
    (8033.0, 1.0, 0.07946551591157913, [2.0, 3.0, 4.0, 5.0], -11.554134845733643, 4.0, 4.0, -2.0, 1.0),
    (7706.0, 1.0, 0.08793578296899796, [3.0, 4.0, 5.0, 6.0], -12.571727216243744, -4.0, 99.0, -4.0, 1.0),
    (6732.0, 1.0, 0.09973720461130142, [4.0, 5.0, 6.0, 7.0], 11.853484153747559, 4.0, 5.0, -4.0, 1.0),
    (6494.0, 1.0, 0.0784224420785904, [5.0, 6.0, 7.0, 8.0], 12.777189254760742, -4.0, 5.0, -5.0, 1.0),
    (5532.0, 1.0, 0.08866000920534134, [6.0, 7.0, 8.0, 9.0], -11.04188847541809, 4.0, 8.0, -5.0, 1.0),
    (5319.0, 2.0, 0.09828391671180725, [7.0, 8.0, 9.0, 10.0], -12.529855132102966, 66.0, 99.0, 2.0, 1.0),
    (5301.0, 2.0, 0.08686944097280502, [8.0, 9.0, 10.0, 11.0], -11.71826559305191, 4.0, -9.0, 2.0, 1.0),
    (5804.0, 2.0, 0.09319207817316055, [9.0, 10.0, 11.0, 2.0], -13.624362230300903, -4.0, -10.0, 4.0, 1.0),
    (6477.0, 2.0, 0.08387245982885361, [10.0, 11.0, 2.0, 3.0], 11.4542977809906, 4.0, -12.0, 4.0, 1.0),
    (7675.0, 2.0, 0.08795157074928284, [11.0, 2.0, 3.0, 4.0], 12.508720219135284, -4.0, -7.0, 5.0, 1.0),
    (8386.0, 2.0, 0.08294370770454407, [2.0, 3.0, 4.0, 5.0], 12.933151602745056, 4.0, -6.0, 5.0, 9.0),
    (8386.0, 2.0, 0.08294370770454407, [3.0, 4.0, 5.0, 6.0], 12.933151602745056, -4.0, -9.0, 5.0, 0.0),
];

/// The frames the effect test feeds to the rig, in order, [`TEST_FRAME_GAP`] apart (12 pauses:
/// none after the 13th frame). The caller computes group levels focused on the tested effect
/// first and restores them afterwards.
pub fn effect_test_frames() -> Vec<TestFrame> {
    ROWS.iter()
        .enumerate()
        .map(|(n, &(rpm, gear, travel, slip, lat, vert, long, lat_vel, canopy))| TestFrame {
            input: ShakeInput {
                speed_kmh: 100.0,
                rpm,
                max_rpm: 8700.0,
                gear,
                suspension_travel: WheelValues { lf: travel, lr: travel, rf: travel, rr: travel },
                suspension_velocity: WheelValues { lf: 0.101, lr: 0.101, rf: 0.101, rr: 0.101 },
                wheel_slip: WheelValues { lf: slip[0], lr: slip[1], rf: slip[2], rr: slip[3] },
                rumble_pitch: WheelValues::default(),
                lateral_acceleration: lat,
                vertical_acceleration: vert,
                longitudinal_acceleration: long,
                lateral_velocity: lat_vel,
                canopy,
                ..ShakeInput::default()
            },
            forced_active: n + 1 < ROWS.len(),
            forced_after: n + 2 < ROWS.len(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sfx_sequence_shape() {
        let f = effect_test_frames();
        assert_eq!(f.len(), 13);
        assert!(f[..12].iter().all(|t| t.forced_active));
        assert!(!f[12].forced_active);
        // Order: the 12th frame, the real listener stored back, then the pause.
        assert!(f[..11].iter().all(|t| t.forced_after));
        assert!(!f[11].forced_after && !f[12].forced_after);
        assert_eq!(f[0].input.wheel_slip.rr, 4.0);
        assert_eq!(f[6].input.gear, 2.0);
        assert_eq!(f[12].input.canopy, 0.0);
        assert_eq!(f[3].input.suspension_velocity.lf, 0.101);
        assert!((TEST_FRAME_GAP.as_secs_f64() - 1.0 / 15.0).abs() < 1e-9);
    }
}
