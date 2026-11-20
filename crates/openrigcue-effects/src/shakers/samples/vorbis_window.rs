//! Ogg Vorbis overlap windows and MDCT twiddle tables, plus the inverse transform,
//! in float32 with a fixed operation order.
//! Other codec stages are separate from this arithmetic.

pub(super) fn configure(header: &mut lewton::header::IdentHeader) {
    for (long, exponent) in [(false, header.blocksize_0), (true, header.blocksize_1)] {
        let window = slope(1usize << (exponent - 1));
        assert!(header.set_window_slope(long, &window));
        let (a, b, c) = twiddle_factors(1usize << exponent);
        assert!(header.set_mdct_twiddle_factors(long, &a, &b, &c));
        header.set_native_mdct(long);
    }
}

// These tables use the same angles as Lewton's, but
// B is stored without Lewton's 1/2 normalization and C with a 1/2 factor.
fn twiddle_factors(block: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let pi = f32::from_bits(0x4049_0fdb);
    let step_a = pi / block as f32;
    let step_b = pi / (2 * block) as f32;
    let half_pi = f32::from_bits(0x3fc9_0fdb);
    let mut a = Vec::with_capacity(block / 2);
    let mut b = Vec::with_capacity(block / 2);
    let mut c = Vec::with_capacity(block / 4);
    for k in 0..block / 4 {
        let angle_a = (4 * k) as f32 * step_a;
        a.extend([sine(angle_a + half_pi), -sine(angle_a)]);
        let angle_b = (2 * k + 1) as f32 * step_b;
        b.extend([sine(angle_b + half_pi) * 0.5, sine(angle_b) * 0.5]);
    }
    for k in 0..block / 8 {
        let angle = (4 * k + 2) as f32 * step_a;
        c.extend([sine(angle + half_pi), -sine(angle)]);
    }
    (a, b, c)
}

fn slope(half_block: usize) -> Vec<f32> {
    let inverse = reciprocal(half_block as f32);
    let half_pi = f32::from_bits(0x3fc9_0fdb);
    (0..half_block)
        .map(|index| {
            let angle = ((index as f32 + 0.5) * inverse) * half_pi;
            let inner = sine(angle);
            sine((inner * inner) * half_pi)
        })
        .collect()
}

fn sine(angle: f32) -> f32 {
    // The bounded-input float32 sine polynomial. Window/twiddle angles stay below 5 radians.
    let magnitude = angle.abs();
    let rounded = magnitude * f32::from_bits(0x3ea2_f983) + f32::from_bits(0x4b40_0000);
    let multiple = rounded - f32::from_bits(0x4b40_0000);
    let mut reduced = magnitude;
    for bits in [0x4049_0000, 0x3a7d_a000, 0x3422_2000, 0x2cb4_611a] {
        reduced -= f32::from_bits(bits) * multiple;
    }
    let square = reduced * reduced;
    reduced = f32::from_bits(reduced.to_bits() ^ (rounded.to_bits() << 31));
    let mut polynomial = f32::from_bits(0x362e_def8) * square;
    for bits in [0xb94f_b7ff, 0x3c08_8766, 0xbe2a_aaa6] {
        polynomial = (polynomial + f32::from_bits(bits)) * square;
    }
    let value = reduced + polynomial * reduced;
    f32::from_bits(value.to_bits() ^ (angle.to_bits() & 0x8000_0000))
}

#[cfg(target_arch = "x86_64")]
fn reciprocal(value: f32) -> f32 {
    native_reciprocal::refined(value)
}

#[cfg(target_arch = "x86_64")]
mod native_reciprocal {
    // SSE is part of x86_64's baseline ISA. Keep the hardware-defined RCPPS
    // seed and the separate Newton operations inside this helper.
    #![allow(unsafe_code)]
    use std::arch::x86_64::{_mm_add_ps, _mm_cvtss_f32, _mm_mul_ps, _mm_rcp_ps, _mm_set1_ps, _mm_sub_ps};

    pub(super) fn refined(value: f32) -> f32 {
        unsafe {
            let value = _mm_set1_ps(value);
            let seed = _mm_rcp_ps(value);
            _mm_cvtss_f32(_mm_sub_ps(_mm_add_ps(seed, seed), _mm_mul_ps(_mm_mul_ps(seed, seed), value)))
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn reciprocal(value: f32) -> f32 {
    // Portable approximation only: the RCPPS seed is hardware-defined on x86_64,
    // so results on other ISAs can differ in the last bits.
    1.0 / value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn native_vorbis_window_matches_independent_sse_reference() {
        // Values independently evaluated with explicit
        // SSE intrinsics, including RCPPS and the sine polynomial.
        let expected = [
            (32, [0x3a78_0017, 0x3f3b_dbf7, 0x3f7f_fff8], 0x0f36_ed61_5136_78eb),
            (64, [0x3978_09a8, 0x3f38_797e, 0x3f7f_ffff], 0x9fcd_d864_a043_147a),
            (128, [0x3878_0c0d, 0x3f36_c16c, 0x3f80_0000], 0x5a2a_cac5_44b3_f8d1),
            (256, [0x3778_0ca5, 0x3f35_e3ba, 0x3f80_0000], 0xdb5e_8dd0_8c21_b249),
            (512, [0x3678_0ccd, 0x3f35_7478, 0x3f80_0000], 0x7fdd_06b0_23f5_adcd),
            (1024, [0x3578_0cd8, 0x3f35_3cbe, 0x3f80_0000], 0x9323_582c_f458_b94c),
            (2048, [0x3478_0cd9, 0x3f35_20d8, 0x3f80_0000], 0xb7bf_861f_1b74_ddf1),
            (4096, [0x3378_0cd9, 0x3f35_12e6, 0x3f80_0000], 0x3f4a_989f_7bd5_fd8e),
        ];
        for (size, bits, hash) in expected {
            let values = slope(size);
            assert_eq!(values.len(), size);
            assert_eq!([values[0].to_bits(), values[size / 2].to_bits(), values[size - 1].to_bits()], bits);
            let actual_hash = values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .fold(0xcbf2_9ce4_8422_2325_u64, |h, byte| (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3));
            assert_eq!(actual_hash, hash, "every window entry, half block {size}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn replacing_only_original_packet_windows_changes_the_imported_pcm() {
        use lewton::inside_ogg::OggStreamReader;
        use lewton::samples::InterleavedSamples;
        use std::io::Cursor;

        let bytes = include_bytes!("../testdata/layout_51.ogg");
        // Compare only the window stage; the twiddle tables are checked separately.
        let mut native_reader = OggStreamReader::new(Cursor::new(bytes)).unwrap();
        for (long, exponent) in
            [(false, native_reader.ident_hdr.blocksize_0), (true, native_reader.ident_hdr.blocksize_1)]
        {
            assert!(native_reader.ident_hdr.set_window_slope(long, &slope(1usize << (exponent - 1))));
        }
        let mut current_pcm = Vec::new();
        while let Some(packet) = native_reader.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
            for frame in packet.samples.as_chunks::<6>().0 {
                for channel in [0, 2, 1, 5, 3, 4] {
                    current_pcm.push((frame[channel] * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16);
                }
            }
        }
        current_pcm.truncate(8820 * 6);
        let mut reader = OggStreamReader::new(Cursor::new(bytes)).unwrap();
        let mut upstream_pcm = Vec::new();
        while let Some(packet) = reader.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
            for frame in packet.samples.as_chunks::<6>().0 {
                for channel in [0, 2, 1, 5, 3, 4] {
                    upstream_pcm.push((frame[channel] * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16);
                }
            }
        }
        upstream_pcm.truncate(8820 * 6);
        assert_eq!(upstream_pcm.len(), current_pcm.len());
        let changes = upstream_pcm.iter().zip(&current_pcm).filter(|(a, b)| a != b).count();
        assert!(changes > 0, "fixture must distinguish native from default windows");
        assert!(upstream_pcm.iter().zip(&current_pcm).all(|(&a, &b)| (i32::from(a) - i32::from(b)).abs() <= 1));
    }
}

#[cfg(test)]
mod twiddle_tests {
    use super::*;
    #[test]
    fn mapped_native_twiddles_match_independent_sse() {
        let expected = [
            (64, [0xbfeedd3412a1bc59, 0xf58805eafaf4ba0f, 0x3cb2405332850d2c]),
            (128, [0xb82852484c7dcea4, 0x37dd2fa65d49c934, 0xba0ba51377fd4102]),
            (256, [0x29db2f7b59414805, 0x71407e63ef6da920, 0x9530f637bdb7beea]),
            (512, [0x18024b8f2e2e9019, 0x5d72713d7c963e14, 0x388ba43ae4bc52e2]),
            (1024, [0x6fbca7d7d44b5d0e, 0x1a9fad9368c0757d, 0x9f430bd9545e6314]),
            (2048, [0xc4f78c560c19dcf0, 0x538c0685bf682623, 0x87cd2af4f31cdf50]),
            (4096, [0x93b1d2a5dcd4a3f5, 0x59ea5bfcdbbbdf60, 0x9c96d3778f3f2484]),
            (8192, [0x7f6dce091dad3a47, 0xe8e4aa2f3eb5ee87, 0x9137ba974acf186c]),
        ];
        for (n, hashes) in expected {
            let (a, b, c) = twiddle_factors(n);
            assert_eq!([a.len(), b.len(), c.len()], [n / 2, n / 2, n / 4]);
            for (values, hash) in [a, b, c].into_iter().zip(hashes) {
                let actual = values
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
                assert_eq!(actual, hash, "block {n}");
            }
        }
    }

    #[test]
    fn native_twiddles_change_imported_pcm_with_windows_held_fixed() {
        use crate::shakers::samples::decode;
        use lewton::inside_ogg::OggStreamReader;
        use lewton::samples::InterleavedSamples;
        use std::io::Cursor;

        let bytes = include_bytes!("../testdata/layout_51.ogg");
        let native = decode(bytes, "layout_51.ogg").unwrap();
        let mut baseline = OggStreamReader::new(Cursor::new(bytes)).unwrap();
        for (long, exponent) in [(false, baseline.ident_hdr.blocksize_0), (true, baseline.ident_hdr.blocksize_1)] {
            assert!(baseline.ident_hdr.set_window_slope(long, &slope(1usize << (exponent - 1))));
            baseline.ident_hdr.set_native_mdct(long);
        }
        let mut pcm = Vec::new();
        while let Some(packet) = baseline.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
            for frame in packet.samples.as_chunks::<6>().0 {
                for channel in [0, 2, 1, 5, 3, 4] {
                    pcm.push((frame[channel] * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16);
                }
            }
        }
        pcm.truncate(native.data.len());
        assert_eq!(pcm.len(), native.data.len());
        let differences: Vec<_> = pcm
            .iter()
            .zip(&native.data)
            .map(|(&a, &b)| (i32::from(a) - (b * 32768.0) as i32).abs())
            .filter(|&difference| difference != 0)
            .collect();
        assert!(!differences.is_empty(), "the imported fixture must distinguish the native twiddles");
        assert!(differences.iter().all(|&difference| difference <= 1));
    }

    #[test]
    fn original_mdct_order_changes_imported_pcm_with_tables_held_fixed() {
        use crate::shakers::samples::decode;
        use lewton::inside_ogg::OggStreamReader;
        use lewton::samples::InterleavedSamples;
        use std::io::Cursor;

        let bytes = include_bytes!("../testdata/layout_51.ogg");
        let native = decode(bytes, "layout_51.ogg").unwrap();
        let mut baseline = OggStreamReader::new(Cursor::new(bytes)).unwrap();
        for (long, exponent) in [(false, baseline.ident_hdr.blocksize_0), (true, baseline.ident_hdr.blocksize_1)] {
            assert!(baseline.ident_hdr.set_window_slope(long, &slope(1usize << (exponent - 1))));
            let (a, b, c) = twiddle_factors(1usize << exponent);
            assert!(baseline.ident_hdr.set_mdct_twiddle_factors(long, &a, &b, &c));
        }
        let mut pcm = Vec::new();
        while let Some(packet) = baseline.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
            for frame in packet.samples.as_chunks::<6>().0 {
                for channel in [0, 2, 1, 5, 3, 4] {
                    pcm.push((frame[channel] * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16);
                }
            }
        }
        pcm.truncate(native.data.len());
        assert_eq!(pcm.len(), native.data.len());
        let changes: Vec<_> = pcm
            .iter()
            .zip(&native.data)
            .map(|(&a, &b)| (i32::from(a) - (b * 32768.0) as i32).abs())
            .filter(|&difference| difference != 0)
            .collect();
        assert!(!changes.is_empty(), "fixture must distinguish original MDCT operation order");
        assert!(changes.iter().all(|&difference| difference <= 1));
    }

    #[test]
    fn legal_small_block_imports_match_independent_native_transform() {
        use crate::shakers::samples::decode;
        // The fixtures contain a one-bit, two-value residue book and a
        // constant unit floor, followed by three finite audio packets.
        // Expected PCM is independently generated by Xiph-style
        // MDCT stages with these tables and overlap arithmetic.
        let mut chained_bytes = Vec::new();
        let mut concatenated = Vec::new();
        for (bytes, frames, expected) in [
            (&include_bytes!("../testdata/small_64.ogg")[..], 64, 0xf8ac_68d6_1584_2c7du64),
            (&include_bytes!("../testdata/small_128.ogg")[..], 128, 0x0825_c0d0_a4f1_d86a),
        ] {
            let sample = decode(bytes, "small.ogg").unwrap();
            assert_eq!((sample.channels, sample.frames()), (1, frames));
            let actual = sample
                .data
                .iter()
                .flat_map(|value| ((value * 32768.0) as i16).to_le_bytes())
                .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3));
            assert_eq!(actual, expected, "block {frames}");
            chained_bytes.extend_from_slice(bytes);
            concatenated.extend_from_slice(&sample.data);
        }
        let chained = decode(&chained_bytes, "small-chain.ogg").unwrap();
        assert_eq!(chained.frames(), 192);
        assert_eq!(chained.data, concatenated);
    }

    #[test]
    fn native_packed_zero_minimum_keeps_normal_audio() {
        use crate::shakers::samples::decode;
        // Alternate legal packed +0: zero mantissa, highest exponent. The
        // double-precision scaling retains zero; float32 scaling
        // prematurely overflows and turns this codebook minimum into NaN.
        let mut bytes = Vec::new();
        let mut expected_chain = Vec::new();
        for (fixture, frames, expected) in [
            (&include_bytes!("../testdata/zero_min_64.ogg")[..], 64, 0xed11_badc_ae0f_7817u64),
            (&include_bytes!("../testdata/zero_min_128.ogg")[..], 128, 0x956d_cfb3_61bd_e2e2),
        ] {
            let sample = decode(fixture, "zero_min.ogg").unwrap();
            assert_eq!((sample.channels, sample.frames()), (1, frames));
            let hash = sample
                .data
                .iter()
                .flat_map(|value| ((value * 32768.0) as i16).to_le_bytes())
                .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3));
            assert_eq!(hash, expected);
            bytes.extend_from_slice(fixture);
            expected_chain.extend_from_slice(&sample.data);
        }
        let chain = decode(&bytes, "zero_min_chain.ogg").unwrap();
        assert_eq!(chain.data, expected_chain);
    }
}
