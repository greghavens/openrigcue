extern crate lewton;
use lewton::{inside_ogg::OggStreamReader, samples::InterleavedSamples};
use std::io::Cursor;
fn decode(bytes: &[u8], native: bool) -> Vec<f32> {
    let mut reader =
        if native { OggStreamReader::new_native(Cursor::new(bytes)) } else { OggStreamReader::new(Cursor::new(bytes)) }
            .unwrap();
    let mut output = Vec::new();
    while let Some(packet) = reader.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
        output.extend(packet.samples);
    }
    output
}
fn hash(values: &[f32]) -> u64 {
    values
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}
#[test]
fn native_residue_width_handles_maximum_blocks_without_u16_wrap() {
    let bytes = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_8ch_8192.ogg");
    let output = decode(bytes, true);
    assert_eq!(output.len(), 65536);
    assert_eq!(hash(&output), 0x24d3f26fbbb0b3ee);
    // Upstream's default mode retains its prior panic, rather than silently changing API behavior.
    assert!(std::panic::catch_unwind(|| decode(bytes, false)).is_err());
}
#[test]
fn native_residue_width_preserves_nonoverflowing_default_and_native_output() {
    let bytes = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_8ch_4096.ogg");
    let output = decode(bytes, true);
    assert_eq!(output.len(), 32768);
    assert_eq!(hash(&output), 0xe9b552d2d13624dc);
    assert_eq!(decode(bytes, false), output);
}
#[test]
fn native_residue_width_applies_to_chained_headers() {
    let first = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/coupling_forward.ogg");
    let second = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_8ch_8192.ogg");
    let bytes = [first.as_slice(), second.as_slice()].concat();
    let output = decode(&bytes, true);
    assert_eq!(output.len(), 128 + 65536);
    assert_eq!(hash(&output[128..]), 0x24d3f26fbbb0b3ee);
}
