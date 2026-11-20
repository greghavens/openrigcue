extern crate lewton;
use lewton::{inside_ogg::OggStreamReader, samples::InterleavedSamples};
use std::io::Cursor;
fn decode(bytes: &[u8], native: bool) -> Result<Vec<f32>, lewton::VorbisError> {
    let mut reader = if native {
        OggStreamReader::new_native(Cursor::new(bytes))
    } else {
        OggStreamReader::new(Cursor::new(bytes))
    }?;
    let mut output = Vec::new();
    while let Some(packet) = reader.read_dec_packet_generic::<InterleavedSamples<f32>>()? {
        output.extend(packet.samples);
    }
    Ok(output)
}
fn fixtures() -> [(&'static [u8], &'static [u8]); 3] {
    [
        (
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_pass0_type0.ogg"),
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_pass7_type0.ogg"),
        ),
        (
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_pass0_type1.ogg"),
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_pass7_type1.ogg"),
        ),
        (
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_pass0_type2.ogg"),
            include_bytes!("../../../crates/openrigcue-effects/src/shakers/testdata/residue_pass7_type2.ogg"),
        ),
    ]
}
#[test]
fn native_setup_accepts_eighth_residue_pass_for_all_residue_types() {
    for (lower, highest) in fixtures() {
        let expected = decode(lower, false).unwrap();
        assert_eq!(expected.len(), 128);
        assert_eq!(decode(lower, true).unwrap(), expected);
        assert_eq!(decode(highest, true).unwrap(), expected);
        assert!(decode(highest, false).is_err());
    }
}
#[test]
fn eighth_residue_pass_is_enabled_after_a_chained_header() {
    for (lower, highest) in fixtures() {
        let mut bytes = lower.to_vec();
        // Give the second logical stream a distinct serial and update its CRC through the packet writer.
        let mut packets = ogg::PacketReader::new(Cursor::new(highest));
        let mut writer = ogg::writing::PacketWriter::new(Cursor::new(Vec::new()));
        while let Some(packet) = packets.read_packet().unwrap() {
            let end = if packet.last_in_stream() {
                ogg::writing::PacketWriteEndInfo::EndStream
            } else if packet.last_in_page() {
                ogg::writing::PacketWriteEndInfo::EndPage
            } else {
                ogg::writing::PacketWriteEndInfo::NormalPacket
            };
            let granule = packet.absgp_page();
            writer.write_packet(packet.data.into_boxed_slice(), 9913, end, granule).unwrap();
        }
        bytes.extend(writer.into_inner().into_inner());
        let output = decode(&bytes, true).unwrap();
        assert_eq!(output.len(), 256);
        assert_eq!(&output[..128], &output[128..]);
    }
}
