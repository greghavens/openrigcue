extern crate lewton;

use lewton::header::read_header_ident;

fn header() -> lewton::header::IdentHeader {
    let mut packet = Vec::from(&b"\x01vorbis"[..]);
    packet.extend(0u32.to_le_bytes());
    packet.push(2);
    packet.extend(44_100u32.to_le_bytes());
    packet.extend([0; 12]);
    packet.extend([0xb8, 1]);
    read_header_ident(&packet).unwrap()
}

#[test]
fn supplied_window_validates_each_block_size() {
    let mut ident = header();
    assert!(!ident.set_window_slope(false, &[0.0; 127]));
    assert!(!ident.set_window_slope(true, &[0.0; 128]));
    assert!(ident.set_window_slope(false, &[0.0; 128]));
    assert!(ident.set_window_slope(true, &[0.0; 1024]));
}

#[test]
fn explicit_native_transform_accepts_each_legal_block() {
    let mut ident = header();
    ident.set_native_mdct(false);
    ident.set_native_mdct(true);
    // read_header_ident accepts both legal smallest transform sizes.
    let mut packet = Vec::from(&b"\x01vorbis"[..]);
    packet.extend(0u32.to_le_bytes());
    packet.push(1);
    packet.extend(44_100u32.to_le_bytes());
    packet.extend([0; 12]);
    packet.extend([0x76, 1]);
    let mut ident = read_header_ident(&packet).unwrap();
    ident.set_native_mdct(false);
    ident.set_native_mdct(true);
}

#[test]
fn default_windows_and_rejected_replacement_keep_upstream_output() {
    use lewton::inside_ogg::OggStreamReader;
    use lewton::samples::InterleavedSamples;
    use std::io::Cursor;

    let bytes = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/layout_51.ogg");
    let decode = |reject: bool, replace: bool| {
        let mut reader = OggStreamReader::new(Cursor::new(bytes)).unwrap();
        if reject {
            assert!(!reader.ident_hdr.set_window_slope(false, &[]));
            assert!(!reader.ident_hdr.set_window_slope(true, &[]));
        }
        if replace {
            for long in [false, true] {
                let exponent = if long { reader.ident_hdr.blocksize_1 } else { reader.ident_hdr.blocksize_0 };
                assert!(reader.ident_hdr.set_window_slope(long, &vec![0.0; 1 << (exponent - 1)]));
            }
        }
        let mut out = Vec::new();
        while let Some(packet) = reader.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
            out.extend(packet.samples);
        }
        out
    };
    let original = decode(false, false);
    let hash = original
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .fold(0xcbf29ce484222325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3));
    // Independently decoded with unmodified crates.io Lewton 0.10.2.
    assert_eq!((original.len(), hash), (53376, 0x446827644e7466e3));
    assert_eq!(original.len(), decode(true, false).len());
    assert!(original.iter().zip(decode(true, false)).all(|(a, b)| a.to_bits() == b.to_bits()));
    assert!(original.iter().zip(decode(false, true)).any(|(a, b)| a.to_bits() != b.to_bits()));
}

#[test]
fn supplied_twiddle_tables_validate_all_lengths_before_replacing() {
    let mut ident = header();
    for long in [false, true] {
        let size = 1usize << if long { ident.blocksize_1 } else { ident.blocksize_0 };
        let a = vec![0.; size / 2];
        let b = vec![0.; size / 2];
        let c = vec![0.; size / 4];
        assert!(!ident.set_mdct_twiddle_factors(long, &a[..a.len() - 1], &b, &c));
        assert!(!ident.set_mdct_twiddle_factors(long, &a, &b[..b.len() - 1], &c));
        assert!(!ident.set_mdct_twiddle_factors(long, &a, &b, &c[..c.len() - 1]));
        assert!(ident.set_mdct_twiddle_factors(long, &a, &b, &c));
    }
}

#[test]
fn rejected_twiddle_tables_do_not_partially_replace_the_defaults() {
    use lewton::inside_ogg::OggStreamReader;
    use lewton::samples::InterleavedSamples;
    use std::io::Cursor;

    let bytes = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/layout_51.ogg");
    let decode = |invalid: Option<(bool, usize)>| {
        let mut reader = OggStreamReader::new(Cursor::new(bytes)).unwrap();
        if let Some((long, index)) = invalid {
            let exponent = if long { reader.ident_hdr.blocksize_1 } else { reader.ident_hdr.blocksize_0 };
            let size = 1usize << exponent;
            let mut tables = [vec![0.0; size / 2], vec![0.0; size / 2], vec![0.0; size / 4]];
            tables[index].pop();
            assert!(!reader.ident_hdr.set_mdct_twiddle_factors(long, &tables[0], &tables[1], &tables[2]));
        }
        let mut output = Vec::new();
        while let Some(packet) = reader.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
            output.extend(packet.samples.into_iter().map(f32::to_bits));
        }
        output
    };
    let original = decode(None);
    for long in [false, true] {
        for index in 0..3 {
            assert_eq!(decode(Some((long, index))), original, "block {long}, table {index}");
        }
    }
}

#[test]
fn native_reader_preserves_finite_packed_zero_on_chained_headers() {
    use lewton::{inside_ogg::OggStreamReader, samples::InterleavedSamples};
    use std::io::Cursor;
    let first = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/zero_min_64.ogg");
    let second = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/zero_min_128.ogg");
    let mut bytes = first.to_vec();
    bytes.extend_from_slice(second);
    let mut original = OggStreamReader::new(Cursor::new(&bytes)).unwrap();
    let mut default_values = Vec::new();
    while let Some(packet) = original.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
        default_values.extend(packet.samples);
    }
    assert!(default_values.iter().all(|value| value.is_nan()));
    let mut native = OggStreamReader::new_native(Cursor::new(&bytes)).unwrap();
    let mut native_values = Vec::new();
    while let Some(packet) = native.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
        native_values.extend(packet.samples);
    }
    assert_eq!(native_values.len(), default_values.len());
    assert_eq!(native_values.len(), 192);
    assert!(native_values.iter().all(|value| value.is_finite()));
}
