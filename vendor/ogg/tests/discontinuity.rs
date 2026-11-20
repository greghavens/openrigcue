extern crate ogg;
use ogg::reading::{BasePacketReader, PageParser};

fn crc(bytes: &[u8], mut crc: u32) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x80000000 != 0 { 0x04c11db7 } else { 0 };
        }
    }
    crc
}
fn push(reader: &mut BasePacketReader, serial: u32, sequence: u32, flags: u8, segments: &[u8], body: &[u8]) {
    assert_eq!(segments.iter().map(|n| usize::from(*n)).sum::<usize>(), body.len());
    let mut h = [0u8; 27];
    h[..4].copy_from_slice(b"OggS");
    h[5] = flags;
    h[14..18].copy_from_slice(&serial.to_le_bytes());
    h[18..22].copy_from_slice(&sequence.to_le_bytes());
    h[26] = segments.len() as u8;
    let c = crc(body, crc(segments, crc(&h, 0)));
    h[22..26].copy_from_slice(&c.to_le_bytes());
    let (mut parser, _) = PageParser::new(h).unwrap();
    parser.parse_segments(segments.to_vec());
    reader.push_page(parser.parse_packet_data(body.to_vec()).unwrap()).unwrap();
}
#[test]
fn reports_a_gap_once_without_changing_packet_bytes() {
    let mut r = BasePacketReader::new();
    push(&mut r, 1, 0, 2, &[1], &[10]);
    let first = r.read_packet().unwrap();
    assert!(!first.discontinuity());
    assert_eq!(first.data, [10]);
    push(&mut r, 1, 2, 0, &[1, 1], &[11, 12]);
    let next = r.read_packet().unwrap();
    assert!(next.discontinuity());
    assert_eq!(next.data, [11]);
    assert!(!r.read_packet().unwrap().discontinuity());
    push(&mut r, 1, 3, 4, &[1], &[13]);
    assert!(!r.read_packet().unwrap().discontinuity());
}
#[test]
fn propagates_gap_through_partial_pages_and_keeps_original_recovery() {
    let mut r = BasePacketReader::new();
    push(&mut r, 1, 0, 2, &[1], &[10]);
    r.read_packet().unwrap();
    push(&mut r, 1, 2, 0, &[255], &[3; 255]);
    assert!(r.read_packet().is_none());
    push(&mut r, 1, 3, 1, &[1, 1], &[4, 5]);
    let next = r.read_packet().unwrap();
    assert!(next.discontinuity());
    assert_eq!(next.data.len(), 256);
    assert_eq!(next.data[255], 4);
    assert!(!r.read_packet().unwrap().discontinuity());
}
#[test]
fn treats_wrap_as_contiguous_and_tracks_streams_independently() {
    let mut r = BasePacketReader::new();
    push(&mut r, 1, u32::MAX, 2, &[1], &[10]);
    assert!(!r.read_packet().unwrap().discontinuity());
    push(&mut r, 2, 0, 2, &[1], &[20]);
    assert!(!r.read_packet().unwrap().discontinuity());
    push(&mut r, 1, 0, 0, &[1], &[11]);
    assert!(!r.read_packet().unwrap().discontinuity());
    push(&mut r, 2, 0, 0, &[1], &[21]);
    assert!(r.read_packet().unwrap().discontinuity());
    r.update_after_seek();
    push(&mut r, 1, 100, 0, &[1], &[12]);
    assert!(!r.read_packet().unwrap().discontinuity());
}

#[test]
fn native_recovery_discards_the_interrupted_packet_but_default_keeps_existing_bytes() {
    for native in [false, true] {
        let mut r = if native { BasePacketReader::new_native() } else { BasePacketReader::new() };
        push(&mut r, 1, 0, 2, &[1], &[1]);
        r.read_packet().unwrap();
        push(&mut r, 1, 1, 0, &[255], &[2; 255]);
        assert!(r.read_packet().is_none());
        push(&mut r, 1, 3, 1, &[1, 1], &[3, 4]);
        let packet = r.read_packet().unwrap();
        assert!(packet.discontinuity());
        if native {
            assert_eq!(packet.data, [4]);
            assert!(r.read_packet().is_none());
        } else {
            assert_eq!(packet.data.len(), 256);
            assert_eq!(r.read_packet().unwrap().data, [4]);
        }
    }
}

#[test]
fn native_recovery_ignores_remaining_continuation_until_a_fresh_packet() {
    let mut r = BasePacketReader::new_native();
    push(&mut r, 1, 0, 2, &[1], &[1]);
    r.read_packet().unwrap();
    push(&mut r, 1, 2, 1, &[255], &[2; 255]);
    assert!(r.read_packet().is_none());
    push(&mut r, 1, 3, 1, &[1, 1], &[3, 4]);
    let packet = r.read_packet().unwrap();
    assert!(packet.discontinuity());
    assert_eq!(packet.data, [4]);
}

#[test]
fn native_recovery_skips_checksum_failed_page_while_default_reports_it() {
    use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
    use std::io::Cursor;
    let mut writer = PacketWriter::new(Cursor::new(Vec::new()));
    for index in 0..3 {
        writer.write_packet(vec![index].into_boxed_slice(), 1, PacketWriteEndInfo::EndPage, u64::from(index)).unwrap();
    }
    let mut bytes = writer.into_inner().into_inner();
    bytes[29 + 22] ^= 1; // The second one-byte page has a bad checksum.
    let mut native = PacketReader::new_native(Cursor::new(bytes.clone()));
    assert_eq!(native.read_packet().unwrap().unwrap().data, [0]);
    let packet = native.read_packet().unwrap().unwrap();
    assert_eq!(packet.data, [2]);
    assert!(packet.discontinuity());
    let mut default = PacketReader::new(Cursor::new(bytes));
    default.read_packet().unwrap();
    assert!(default.read_packet().is_err());
}
