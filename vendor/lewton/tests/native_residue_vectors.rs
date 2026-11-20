extern crate lewton;
use lewton::{inside_ogg::OggStreamReader, samples::InterleavedSamples};
use std::io::Cursor;
fn hash(bytes: &[u8], native: bool) -> u64 {
    let mut reader =
        if native { OggStreamReader::new_native(Cursor::new(bytes)) } else { OggStreamReader::new(Cursor::new(bytes)) }
            .unwrap();
    let mut result = 0xcbf29ce484222325u64;
    let mut samples = 0;
    while let Some(packet) = reader.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
        samples += packet.samples.len();
        for byte in packet.samples.iter().flat_map(|v| v.to_le_bytes()) {
            result = (result ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    assert_eq!(samples, 128);
    result
}
#[test]
fn native_residue_retains_final_vectors_within_the_spectral_extent() {
    let cases: [(&[u8], u64, u64); 3] = [
        (
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_vector_type0.ogg"),
            0xab5b808e11054b25,
            0xab5b808e11054b25,
        ),
        (
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_vector_type1.ogg"),
            0x2dbce66524d01268,
            0x777edc29fc23712e,
        ),
        (
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_vector_type2.ogg"),
            0xd0176b3f76707b06,
            0x869f4a365bf54ca0,
        ),
    ];
    for (bytes, upstream, native) in cases {
        assert_eq!(hash(bytes, false), upstream);
        assert_eq!(hash(bytes, true), native);
    }
}
