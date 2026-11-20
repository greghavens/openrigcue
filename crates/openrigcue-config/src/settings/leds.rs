//! `[LED]`: the RGB shift-light strip and its four segments.

use serde::{Deserialize, Serialize};

/// What a segment shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentKind {
    #[serde(rename = "rpm", alias = "R")]
    Rpm,
    #[serde(rename = "speed", alias = "S")]
    Speed,
    #[serde(rename = "flag", alias = "F")]
    Flag,
    #[serde(rename = "rpm_flag", alias = "RF")]
    RpmFlag,
    #[serde(rename = "speed_flag", alias = "SF")]
    SpeedFlag,
    #[serde(rename = "spotter_right", alias = "SR")]
    SpotterRight,
    #[serde(rename = "spotter_left", alias = "SL")]
    SpotterLeft,
    #[serde(rename = "spotter_right_flag", alias = "SRF")]
    SpotterRightFlag,
    #[serde(rename = "spotter_left_flag", alias = "SLF")]
    SpotterLeftFlag,
    #[serde(rename = "off", alias = "O")]
    Off,
}

/// One LED segment (`segmentN_type/start/end/direction/color`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedSegment {
    pub kind: SegmentKind,
    /// First LED (1-based).
    pub start: u32,
    /// Last LED.
    pub end: u32,
    /// Fill direction code (`F` in every default).
    pub direction: String,
    pub color: [u8; 3],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LedSettings {
    pub brightness: u32,
    pub nbr_segments: u32,
    pub nbr_leds: u32,
    /// v219 (`RGB`).
    pub model: String,
    pub segments: [LedSegment; 4],
}

impl Default for LedSettings {
    /// Template layout (`template.ini` `[LED]`): RPM on 1–50, speed on 51–100, flags on 101–125
    /// and 126–150 of 150 LEDs; colours from the template, rewritten by v85 with the same values.
    fn default() -> Self {
        let segment = |kind, start, end, color| LedSegment { kind, start, end, direction: "F".into(), color };
        LedSettings {
            brightness: 100,
            nbr_segments: 3,
            nbr_leds: 150,
            model: "RGB".into(),
            segments: [
                segment(SegmentKind::Rpm, 1, 50, [255, 0, 0]),
                segment(SegmentKind::Speed, 51, 100, [0, 128, 0]),
                segment(SegmentKind::Flag, 101, 125, [0, 0, 255]),
                segment(SegmentKind::Flag, 126, 150, [255, 165, 0]),
            ],
        }
    }
}

/// The flat key layout of the section in `config.ini`, used only to read it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct LegacyLed {
    brightness: u32,
    nbr_segments: u32,
    nbr_leds: u32,
    model: String,
    segment1_type: SegmentKind,
    segment1_start: u32,
    segment1_end: u32,
    segment1_direction: String,
    segment1_color: [u8; 3],
    segment2_type: SegmentKind,
    segment2_start: u32,
    segment2_end: u32,
    segment2_direction: String,
    segment2_color: [u8; 3],
    segment3_type: SegmentKind,
    segment3_start: u32,
    segment3_end: u32,
    segment3_direction: String,
    segment3_color: [u8; 3],
    segment4_type: SegmentKind,
    segment4_start: u32,
    segment4_end: u32,
    segment4_direction: String,
    segment4_color: [u8; 3],
}

impl Default for LegacyLed {
    fn default() -> Self {
        let d = LedSettings::default();
        let [s1, s2, s3, s4] = d.segments;
        LegacyLed {
            brightness: d.brightness,
            nbr_segments: d.nbr_segments,
            nbr_leds: d.nbr_leds,
            model: d.model,
            segment1_type: s1.kind,
            segment1_start: s1.start,
            segment1_end: s1.end,
            segment1_direction: s1.direction,
            segment1_color: s1.color,
            segment2_type: s2.kind,
            segment2_start: s2.start,
            segment2_end: s2.end,
            segment2_direction: s2.direction,
            segment2_color: s2.color,
            segment3_type: s3.kind,
            segment3_start: s3.start,
            segment3_end: s3.end,
            segment3_direction: s3.direction,
            segment3_color: s3.color,
            segment4_type: s4.kind,
            segment4_start: s4.start,
            segment4_end: s4.end,
            segment4_direction: s4.direction,
            segment4_color: s4.color,
        }
    }
}

impl From<LegacyLed> for LedSettings {
    fn from(l: LegacyLed) -> Self {
        LedSettings {
            brightness: l.brightness,
            nbr_segments: l.nbr_segments,
            nbr_leds: l.nbr_leds,
            model: l.model,
            segments: [
                LedSegment {
                    kind: l.segment1_type,
                    start: l.segment1_start,
                    end: l.segment1_end,
                    direction: l.segment1_direction,
                    color: l.segment1_color,
                },
                LedSegment {
                    kind: l.segment2_type,
                    start: l.segment2_start,
                    end: l.segment2_end,
                    direction: l.segment2_direction,
                    color: l.segment2_color,
                },
                LedSegment {
                    kind: l.segment3_type,
                    start: l.segment3_start,
                    end: l.segment3_end,
                    direction: l.segment3_direction,
                    color: l.segment3_color,
                },
                LedSegment {
                    kind: l.segment4_type,
                    start: l.segment4_start,
                    end: l.segment4_end,
                    direction: l.segment4_direction,
                    color: l.segment4_color,
                },
            ],
        }
    }
}
