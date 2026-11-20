//! Final sample conversion after the device speaker matrix.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pcm16Rounding {
    Truncate,
    NearestEven,
}
impl Pcm16Rounding {
    pub(super) fn native() -> Self {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            Self::for_cpu(std::is_x86_feature_detected!("sse3"), std::is_x86_feature_detected!("avx"))
        }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        {
            Self::Truncate
        }
    }

    fn for_cpu(sse3: bool, avx: bool) -> Self {
        if sse3 || avx {
            Self::NearestEven
        } else {
            Self::Truncate
        }
    }
}

/// Conversion of the final device frame, after the float32 speaker matrix.
pub(super) trait OutputSample {
    fn from_mix(value: f32, rounding: Pcm16Rounding) -> Self;
}
impl OutputSample for f32 {
    fn from_mix(value: f32, _: Pcm16Rounding) -> Self {
        value
    }
}
impl OutputSample for i16 {
    fn from_mix(value: f32, rounding: Pcm16Rounding) -> Self {
        pcm16(value, rounding)
    }
}
impl OutputSample for u16 {
    fn from_mix(value: f32, rounding: Pcm16Rounding) -> Self {
        // Linux unsigned16 transport uses the same signed quantization plus bias.
        (i32::from(pcm16(value, rounding)) + 32768) as u16
    }
}
impl OutputSample for i32 {
    fn from_mix(value: f32, _: Pcm16Rounding) -> Self {
        let scaled = value * 2147483648.0;
        // The negative endpoint is clamped one
        // above i32::MIN; unordered input takes that same lower branch.
        if scaled <= -2147483648.0 || scaled.is_nan() {
            -2147483647
        } else if scaled >= 2147483648.0 {
            2147483647
        } else {
            scaled as i32
        }
    }
}
fn pcm16(value: f32, rounding: Pcm16Rounding) -> i16 {
    let scaled = value * 32768.0;
    // An out-of-range float converts to the indefinite i32 (as CVTPS2DQ does) before the 16-bit saturation.
    let integer = if !(-2147483648.0..2147483648.0).contains(&scaled) {
        i32::MIN
    } else {
        match rounding {
            Pcm16Rounding::Truncate => scaled as i32,
            Pcm16Rounding::NearestEven => scaled.round_ties_even() as i32,
        }
    };
    integer.clamp(-32768, 32767) as i16
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_pcm16_matches_independent_sse_reference() {
        for (mode, expected) in
            [(Pcm16Rounding::NearestEven, 0x3aa3_d9d4_ba32_7dc5), (Pcm16Rounding::Truncate, 0x60e2_5a33_9ba9_d84b)]
        {
            let mut seed = 0x12345678u32;
            let mut h = 0xcbf29ce484222325u64;
            for _ in 0..131072 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let f = seed as i32 as f32 / 2147483648.0;
                for b in i16::from_mix(f, mode).to_le_bytes() {
                    h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
                }
            }
            assert_eq!(h, expected);
        }
    }
    #[test]
    fn native_pcm32_matches_independent_sse_reference() {
        let mut seed = 0x12345678u32;
        let mut h = 0xcbf29ce484222325u64;
        for _ in 0..131072 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let f = seed as i32 as f32 / 1073741824.0;
            for b in i32::from_mix(f, Pcm16Rounding::NearestEven).to_le_bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            }
        }
        assert_eq!(h, 0x3f0c615bb8aaf19e);
    }
    #[test]
    fn final_quantization_and_unsigned_adapter() {
        assert_eq!(Pcm16Rounding::for_cpu(false, false), Pcm16Rounding::Truncate);
        assert_eq!(Pcm16Rounding::for_cpu(true, false), Pcm16Rounding::NearestEven);
        assert_eq!(Pcm16Rounding::for_cpu(false, true), Pcm16Rounding::NearestEven);
        assert_eq!(Pcm16Rounding::for_cpu(true, true), Pcm16Rounding::NearestEven);
        for (input, rounded, truncated) in
            [(0.5, 0, 0), (1.5, 2, 1), (2.5, 2, 2), (-0.5, 0, 0), (-1.5, -2, -1), (-2.5, -2, -2)]
        {
            let v = input / 32768.0;
            assert_eq!(i16::from_mix(v, Pcm16Rounding::NearestEven), rounded);
            assert_eq!(i16::from_mix(v, Pcm16Rounding::Truncate), truncated);
            assert_eq!(u16::from_mix(v, Pcm16Rounding::NearestEven), (i32::from(rounded) + 32768) as u16);
        }
        for (input, expected) in [(-2., -32768), (-1., -32768), (1., 32767), (2., 32767)] {
            assert_eq!(i16::from_mix(input, Pcm16Rounding::native()), expected);
        }
        assert_eq!(i32::from_mix(-1.0, Pcm16Rounding::native()), -2147483647);
        assert_eq!(i32::from_mix(1.0, Pcm16Rounding::native()), 2147483647);
        for v in [-2.0, -0.0, 0.1, 2.0] {
            assert_eq!(f32::from_mix(v, Pcm16Rounding::native()).to_bits(), v.to_bits());
        }
    }

    #[test]
    fn final_device_conversion_uses_rendered_audio() {
        use crate::shakers::channel_map::{ChannelOrder, OutputMatrix};
        use crate::shakers::layout::Speaker;
        use crate::shakers::levels::GroupLevels;
        use crate::shakers::mixer::Mixer;
        use crate::shakers::rig::Voice;
        use crate::shakers::samples::{SampleBank, SampleFolder, SampleKey};
        use crate::shakers::{ShakerTuning, SpeakerProfile};

        let profile = SpeakerProfile::empty();
        let levels = GroupLevels::compute(&profile, &ShakerTuning::default(), None);
        let bank = SampleBank::synthesized();
        let key = SampleKey::new(SampleFolder::Transmission, 1);
        let mut mixer = Mixer::new(44_100, bank, levels);
        let mut gains = [0.0; 6];
        gains[Speaker::FrontLeft.index()] = 1.0;
        mixer.play_direct(Voice::SpeakerTest, key, gains);
        let matrix = OutputMatrix::new(ChannelOrder::Wave, 2).unwrap();
        let mut mixed = [0.0; 512];
        mixer.render_output(&mut mixed, &matrix);
        let pcm: Vec<_> = mixed.iter().map(|&value| i16::from_mix(value, Pcm16Rounding::NearestEven)).collect();
        assert!(pcm.iter().any(|&value| value != 0));
        for (&source, &actual) in mixed.iter().zip(&pcm) {
            let expected = (source * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16;
            assert_eq!(actual, expected);
        }
        assert!(pcm.as_chunks::<2>().0.iter().all(|frame| frame[1] == 0));
    }
}
