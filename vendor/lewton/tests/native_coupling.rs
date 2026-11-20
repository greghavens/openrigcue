extern crate lewton;
use lewton::{inside_ogg::OggStreamReader, samples::InterleavedSamples};
use std::io::Cursor;
fn decode(bytes: &[u8], native: bool) -> Vec<f32> {
    let mut r =
        if native { OggStreamReader::new_native(Cursor::new(bytes)) } else { OggStreamReader::new(Cursor::new(bytes)) }
            .unwrap();
    let mut out = Vec::new();
    while let Some(p) = r.read_dec_packet_generic::<InterleavedSamples<f32>>().unwrap() {
        out.extend(p.samples);
    }
    out
}
fn hash(v: &[f32]) -> u64 {
    v.iter()
        .flat_map(|x| x.to_le_bytes())
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}
#[test]
fn native_coupling_preserves_declared_channel_order_and_default_behavior() {
    let f = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/coupling_forward.ogg");
    let r = include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/coupling_reverse.ogg");
    assert_eq!(hash(&decode(f, false)), 0x81c67e90bc785f10);
    assert_eq!(hash(&decode(r, false)), 0x81c67e90bc785f10);
    assert_eq!(hash(&decode(f, true)), 0x81c67e90bc785f10);
    assert_eq!(hash(&decode(r, true)), 0x77bcc16b591f4845);
}
#[test]
fn native_coupling_order_propagates_through_chained_setup() {
    let mut bytes =
        include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/coupling_forward.ogg").to_vec();
    bytes.extend(include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/coupling_reverse.ogg"));
    let out = decode(&bytes, true);
    assert_eq!(out.len(), 256);
    assert_eq!(hash(&out[..128]), 0x81c67e90bc785f10);
    assert_eq!(hash(&out[128..]), 0x77bcc16b591f4845);
}
