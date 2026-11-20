//! Placing the six speaker buses on an output device's channels, and choosing the device
//! format. The mix is 5.1; openrigcue opens the device with the channel count it is configured
//! for and places the buses in the backend's channel order.

use super::layout::Speaker;

/// The channel order an audio backend expects in interleaved frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOrder {
    /// ALSA's standard maps, also used by the PulseAudio and PipeWire ALSA plugins:
    /// FL FR RL RR FC LFE (5.1), plus SL SR (7.1).
    Alsa,
    /// WAVE/WASAPI order: FL FR FC LFE BL BR (5.1), plus SL SR (7.1).
    Wave,
}

/// For each device channel, the speaker bus it plays (`None` = silent).
pub type DeviceLayout = Vec<Option<Speaker>>;

/// The speaker buses in their own order, for a six-channel buffer in [`Speaker::ALL`] order.
pub const SPEAKER_ORDER: [Option<Speaker>; 6] = [
    Some(Speaker::FrontLeft),
    Some(Speaker::FrontRight),
    Some(Speaker::Center),
    Some(Speaker::Sub),
    Some(Speaker::RearLeft),
    Some(Speaker::RearRight),
];

/// Device channel counts openrigcue drives, best first: 5.1 (the mix's own layout), 7.1,
/// quad, stereo.
pub const PREFERRED_CHANNELS: [u16; 4] = [6, 8, 4, 2];

/// Where the speaker buses go on a device with `channels` channels in `order`.
///
/// - 6 channels: all six buses.
/// - 8 channels (7.1): the six buses, rear left/right on the back pair, side pair silent.
/// - 4 channels: FL FR RL RR; centre and sub are dropped.
/// - 2 channels: FL FR; the other buses are dropped. The surround check tells users
///   of a stereo device to pick a two-channel profile such as "2 Speakers - Front Left / Right".
///
/// `None` for any other count.
pub fn device_layout(order: ChannelOrder, channels: u16) -> Option<DeviceLayout> {
    use Speaker::*;
    let mut layout = match (channels, order) {
        (2, _) => vec![Some(FrontLeft), Some(FrontRight)],
        (4, _) => vec![Some(FrontLeft), Some(FrontRight), Some(RearLeft), Some(RearRight)],
        (6 | 8, ChannelOrder::Alsa) => {
            vec![Some(FrontLeft), Some(FrontRight), Some(RearLeft), Some(RearRight), Some(Center), Some(Sub)]
        }
        (6 | 8, ChannelOrder::Wave) => SPEAKER_ORDER.to_vec(),
        _ => return None,
    };
    layout.resize(usize::from(channels), None);
    Some(layout)
}

/// Conversion matrix from the completed 5.1 mix into the device format.
/// Rows follow the device's channel order; this is separate from the raw bus map.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputMatrix {
    rows: Vec<[f32; 6]>,
    direct: bool,
    simd: bool,
}

impl OutputMatrix {
    pub fn new(order: ChannelOrder, channels: u16) -> Option<Self> {
        let indices: &[usize] = match (channels, order) {
            (2, _) => &[0, 1],
            (4, _) => &[0, 1, 2, 3],
            (6, ChannelOrder::Alsa) => &[0, 1, 4, 5, 2, 3],
            (8, ChannelOrder::Alsa) => &[0, 1, 6, 7, 2, 3, 4, 5],
            (6, ChannelOrder::Wave) => &[0, 1, 2, 3, 4, 5],
            // The internal 7.1 order places surrounds before backs;
            // the WAVE device order swaps these two pairs.
            (8, ChannelOrder::Wave) => &[0, 1, 2, 3, 6, 7, 4, 5],
            _ => return None,
        };
        Self::with_indices(channels, indices.iter().copied().map(Some))
    }

    /// PulseAudio supplies explicit speaker names, which may be permuted or unknown.
    /// A 5.1 side pair is the surround pair; in 7.1 it is distinct from the back pair.
    pub fn from_positions(positions: &[String]) -> Option<Self> {
        let channels = u16::try_from(positions.len()).ok()?;
        let rear_pair = positions.iter().any(|name| matches!(name.as_str(), "rear-left" | "rear-right"));
        let indices = positions.iter().map(|name| match name.as_str() {
            "front-left" => Some(0),
            "front-right" => Some(1),
            "front-center" if channels >= 6 => Some(2),
            "lfe" if channels >= 6 => Some(3),
            "rear-left" => match channels {
                4 => Some(2),
                6 => Some(4),
                8 => Some(6),
                _ => None,
            },
            "rear-right" => match channels {
                4 => Some(3),
                6 => Some(5),
                8 => Some(7),
                _ => None,
            },
            "side-left" if channels == 8 => Some(4),
            "side-right" if channels == 8 => Some(5),
            "side-left" if !rear_pair && channels == 6 => Some(4),
            "side-right" if !rear_pair && channels == 6 => Some(5),
            "side-left" if !rear_pair && channels == 4 => Some(2),
            "side-right" if !rear_pair && channels == 4 => Some(3),
            _ => None,
        });
        Self::with_indices(channels, indices)
    }

    fn with_indices(channels: u16, indices: impl Iterator<Item = Option<usize>>) -> Option<Self> {
        let center = f32::from_bits(0x3f35_0481); // 0.7071
        let side = f32::from_bits(0x3f61_ff2e); // 0.8828
        let back = f32::from_bits(0x3ef0_7c85); // 0.4697
        let mut rows = vec![[0.0; 6]; usize::from(channels)];
        match channels {
            2 => {
                rows[0] = [1.0, 0.0, center, 0.0, center, 0.0];
                rows[1] = [0.0, 1.0, center, 0.0, 0.0, center];
            }
            4 => {
                rows[0] = [1.0, 0.0, center, 0.0, 0.0, 0.0];
                rows[1] = [0.0, 1.0, center, 0.0, 0.0, 0.0];
                rows[2][4] = 1.0;
                rows[3][5] = 1.0;
            }
            6 => {
                for (index, row) in rows.iter_mut().enumerate() {
                    row[index] = 1.0;
                }
            }
            8 => {
                for (index, row) in rows.iter_mut().take(4).enumerate() {
                    row[index] = 1.0;
                }
                rows[4][4] = side;
                rows[5][5] = side;
                rows[6][4] = back;
                rows[7][5] = back;
            }
            _ => return None,
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        let simd = std::is_x86_feature_detected!("sse3");
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        let simd = false;
        Some(Self {
            rows: indices.map(|index| index.and_then(|index| rows.get(index)).copied().unwrap_or([0.0; 6])).collect(),
            direct: channels == 6,
            simd,
        })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn write(&self, buses: &[f64; 6], output: &mut [f32]) {
        for (sample, row) in output.iter_mut().zip(&self.rows) {
            if self.direct {
                // Equal layouts: copy the diagonal channels.
                *sample = row.iter().position(|&gain| gain == 1.0).map_or(0.0, |index| buses[index] as f32);
            } else {
                let p: [f32; 6] = std::array::from_fn(|index| buses[index] as f32 * row[index]);
                *sample = if self.simd {
                    // Summation order of the six inputs: the high and low four-lane
                    // vectors, then horizontal pairs, then the cleared output.
                    ((p[0] + p[4]) + (p[1] + p[5])) + (p[2] + p[3]) + 0.0
                } else {
                    p.into_iter().rev().reduce(|sum, value| sum + value).unwrap()
                };
            }
        }
    }
}

/// One output format a device offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOption {
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    /// Preference of the sample format (lower is better); `None` when openrigcue cannot write it.
    pub format_rank: Option<u8>,
}

/// The chosen format: index into the offered options, channel count and sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatChoice {
    pub index: usize,
    pub channels: u16,
    pub rate: u32,
}

/// Pick the device format: `configured` (the channel count the device is configured for, when
/// known) if the device offers it in a usable sample format, else the first channel count of
/// [`PREFERRED_CHANNELS`] it offers; among the options of that count, one that runs at
/// `wanted_rate` if any (best sample format first), else the best sample format at its rate
/// nearest to `wanted_rate`.
pub fn choose_format(options: &[FormatOption], wanted_rate: u32, configured: Option<u16>) -> Option<FormatChoice> {
    let counts = configured.into_iter().filter(|c| device_layout(ChannelOrder::Wave, *c).is_some());
    counts.chain(PREFERRED_CHANNELS).find_map(|channels| {
        let usable = || {
            options
                .iter()
                .enumerate()
                .filter(move |(_, o)| o.channels == channels)
                .filter_map(|(i, o)| o.format_rank.map(|rank| (i, o, rank)))
        };
        let at_rate = usable()
            .filter(|(_, o, _)| (o.min_rate..=o.max_rate).contains(&wanted_rate))
            .min_by_key(|(_, _, rank)| *rank);
        let (index, option, _) = at_rate.or_else(|| usable().min_by_key(|(_, _, rank)| *rank))?;
        Some(FormatChoice { index, channels, rate: wanted_rate.clamp(option.min_rate, option.max_rate) })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Speaker::*;

    #[test]
    fn alsa_51_puts_center_and_sub_last() {
        assert_eq!(
            device_layout(ChannelOrder::Alsa, 6).unwrap(),
            [Some(FrontLeft), Some(FrontRight), Some(RearLeft), Some(RearRight), Some(Center), Some(Sub)]
        );
        assert_eq!(device_layout(ChannelOrder::Wave, 6).unwrap(), SPEAKER_ORDER.to_vec());
    }

    #[test]
    fn seven_one_quad_stereo() {
        let alsa = device_layout(ChannelOrder::Alsa, 8).unwrap();
        assert_eq!(alsa.len(), 8);
        assert_eq!(&alsa[6..], [None, None]);
        assert_eq!(alsa[4], Some(Center));
        let wave = device_layout(ChannelOrder::Wave, 8).unwrap();
        assert_eq!(&wave[..6], SPEAKER_ORDER);
        assert_eq!(&wave[6..], [None, None]);
        assert_eq!(
            device_layout(ChannelOrder::Alsa, 4).unwrap(),
            [Some(FrontLeft), Some(FrontRight), Some(RearLeft), Some(RearRight)]
        );
        assert_eq!(device_layout(ChannelOrder::Wave, 2).unwrap(), [Some(FrontLeft), Some(FrontRight)]);
        assert_eq!(device_layout(ChannelOrder::Alsa, 1), None);
        assert_eq!(device_layout(ChannelOrder::Alsa, 3), None);
    }

    fn option(channels: u16, min_rate: u32, max_rate: u32, rank: Option<u8>) -> FormatOption {
        FormatOption { channels, min_rate, max_rate, format_rank: rank }
    }

    #[test]
    fn format_prefers_51_then_rate_then_sample_format() {
        let options = [
            option(2, 8_000, 192_000, Some(0)),
            option(6, 48_000, 48_000, Some(0)),
            option(6, 44_100, 44_100, Some(1)),
            option(8, 44_100, 44_100, Some(0)),
        ];
        assert_eq!(choose_format(&options, 44_100, None), Some(FormatChoice { index: 2, channels: 6, rate: 44_100 }));
        // No six-channel format at the wanted rate: best sample format at its nearest rate.
        assert_eq!(choose_format(&options, 96_000, None), Some(FormatChoice { index: 1, channels: 6, rate: 48_000 }));
    }

    /// The device's configured channel count comes first; a count the device does not offer, or one no layout exists
    /// for, falls back to the preference order.
    #[test]
    fn format_takes_the_configured_channel_count() {
        let options = [option(2, 44_100, 48_000, Some(0)), option(6, 44_100, 48_000, Some(0))];
        assert_eq!(
            choose_format(&options, 48_000, Some(2)),
            Some(FormatChoice { index: 0, channels: 2, rate: 48_000 })
        );
        assert_eq!(
            choose_format(&options, 48_000, Some(8)),
            Some(FormatChoice { index: 1, channels: 6, rate: 48_000 })
        );
        assert_eq!(
            choose_format(&options, 48_000, Some(3)),
            Some(FormatChoice { index: 1, channels: 6, rate: 48_000 })
        );
    }

    #[test]
    fn format_falls_back_to_fewer_channels() {
        let options =
            [option(6, 44_100, 48_000, None), option(1, 44_100, 48_000, Some(0)), option(2, 44_100, 48_000, Some(0))];
        assert_eq!(choose_format(&options, 48_000, None), Some(FormatChoice { index: 2, channels: 2, rate: 48_000 }));
        assert_eq!(choose_format(&options[..2], 48_000, None), None);
    }

    #[test]
    fn native_output_connections_match_independent_matrix_reference() {
        // An independent C implementation with the same matrices, SSE
        // lane sums and scalar reverse-channel accumulation over 4096 frames.
        for (channels, expected) in [
            (2, [0xef2d_20ae_baa9_aac2, 0xa1f2_a902_729b_cccf]),
            (4, [0x678f_93e3_77ec_3184; 2]),
            (6, [0x5169_a79b_1674_598d; 2]),
            (8, [0xbcd7_a944_e7fa_801f; 2]),
        ] {
            for (mode, expected) in expected.into_iter().enumerate() {
                let mut matrix = OutputMatrix::new(ChannelOrder::Wave, channels).unwrap();
                matrix.simd = mode == 1;
                let mut hash = 0xcbf2_9ce4_8422_2325_u64;
                for frame in 0..4096 {
                    let source = std::array::from_fn(|channel| {
                        f64::from(((frame * 503 + channel as i32 * 2099) % 48001 - 24000) as f32 / 32768.0)
                    });
                    let mut output = vec![0.0; usize::from(channels)];
                    matrix.write(&source, &mut output);
                    for byte in output.into_iter().flat_map(f32::to_le_bytes) {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                    }
                }
                assert_eq!(hash, expected, "device channels{channels}, SIMD{mode}");
            }
        }
    }

    #[test]
    fn output_matrix_respects_device_order_and_unknown_positions() {
        let source = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let wave = OutputMatrix::new(ChannelOrder::Wave, 8).unwrap();
        let alsa = OutputMatrix::new(ChannelOrder::Alsa, 8).unwrap();
        let mut expected = [0.0; 8];
        let mut actual = [0.0; 8];
        wave.write(&source, &mut expected);
        alsa.write(&source, &mut actual);
        assert_eq!(
            actual,
            [expected[0], expected[1], expected[4], expected[5], expected[2], expected[3], expected[6], expected[7]]
        );
        let names =
            ["side-right", "lfe", "rear-left", "front-center", "front-right", "side-left", "front-left", "unknown"]
                .map(str::to_owned);
        let named = OutputMatrix::from_positions(&names).unwrap();
        named.write(&source, &mut actual);
        assert_eq!(
            actual,
            [expected[7], expected[3], expected[4], expected[2], expected[1], expected[6], expected[0], 0.0]
        );
        assert!(OutputMatrix::from_positions(&[]).is_none());
        assert!(OutputMatrix::new(ChannelOrder::Wave, 3).is_none());
        assert!(OutputMatrix::from_positions(&["front-left".into()]).is_none());
        let unknown = OutputMatrix::from_positions(&["unknown".into(), "unknown".into()]).unwrap();
        let mut silence = [1.0; 2];
        unknown.write(&source, &mut silence);
        assert_eq!(silence, [0.0; 2]);
    }

    #[test]
    fn output_matrix_keeps_six_channel_identity_and_side_only_surround() {
        let names = ["front-left", "front-right", "front-center", "lfe", "side-left", "side-right"].map(str::to_owned);
        let matrix = OutputMatrix::from_positions(&names).unwrap();
        let source = [-0.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut output = [0.0; 6];
        matrix.write(&source, &mut output);
        assert_eq!(output.map(f32::to_bits), source.map(|value| (value as f32).to_bits()));
        let names = ["side-right", "front-right", "front-left", "side-left"].map(str::to_owned);
        let matrix = OutputMatrix::from_positions(&names).unwrap();
        let mut output = [0.0; 4];
        matrix.write(&source, &mut output);
        let center = 3.0 * f32::from_bits(0x3f35_0481);
        assert_eq!(output, [6.0, 2.0 + center, center, 5.0]);
    }
}
