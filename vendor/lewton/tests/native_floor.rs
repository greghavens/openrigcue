extern crate lewton;
extern crate ogg;
use lewton::header::{read_header_setup, read_header_setup_native};
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    pos: usize,
}
impl Bits {
    fn put(&mut self, x: u32, n: usize) {
        for i in 0..n {
            if self.pos / 8 == self.bytes.len() {
                self.bytes.push(0);
            }
            self.bytes[self.pos / 8] |= (((x >> i) & 1) as u8) << (self.pos % 8);
            self.pos += 1;
        }
    }
    fn octets(&mut self, x: &[u8]) {
        for b in x {
            self.put(u32::from(*b), 8);
        }
    }
}
fn floor_zero_setup() -> Vec<u8> {
    let mut w = Bits::default();
    w.octets(b"\x05vorbis");
    // One two-entry, one-dimensional codebook with a ±1 lookup.
    w.put(0, 8);
    w.put(0x564342, 24);
    w.put(1, 16);
    w.put(2, 24);
    w.put(0, 1);
    w.put(0, 1);
    w.put(0, 5);
    w.put(0, 5);
    w.put(2, 4);
    w.put(0xe0100000, 32);
    w.put(0x60300000, 32);
    w.put(0, 4);
    w.put(0, 1);
    w.put(0, 1);
    w.put(1, 1);
    // One time transform and a valid floor0 (order2, rate44100,
    // bark map64, amplitude6bits/60dB, one valid book).
    w.put(0, 6);
    w.put(0, 16);
    w.put(0, 6);
    w.put(0, 16);
    w.put(2, 8);
    w.put(44100, 16);
    w.put(64, 16);
    w.put(6, 6);
    w.put(60, 8);
    w.put(0, 4);
    w.put(0, 8);
    // One residue1, one uncoupled mapping and one short-block mode.
    w.put(0, 6);
    w.put(1, 16);
    w.put(0, 24);
    w.put(32, 24);
    w.put(0, 24);
    w.put(0, 6);
    w.put(0, 8);
    w.put(1, 3);
    w.put(0, 1);
    w.put(0, 8);
    w.put(0, 6);
    w.put(0, 16);
    w.put(0, 1);
    w.put(0, 1);
    w.put(0, 2);
    w.put(0, 8);
    w.put(0, 8);
    w.put(0, 8);
    w.put(0, 6);
    w.put(0, 1);
    w.put(0, 16);
    w.put(0, 16);
    w.put(0, 8);
    w.put(1, 1);
    w.bytes
}

fn floor_zero_ogg() -> Vec<u8> {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    use std::io::Cursor;
    let mut writer = PacketWriter::new(Cursor::new(Vec::new()));
    let mut ident = b"\x01vorbis".to_vec();
    ident.extend(0u32.to_le_bytes());
    ident.push(1);
    ident.extend(44100u32.to_le_bytes());
    ident.extend([0; 12]);
    ident.extend([0x66, 1]);
    let mut comment = b"\x03vorbis".to_vec();
    comment.extend(10u32.to_le_bytes());
    comment.extend(b"OpenRigCue");
    comment.extend(0u32.to_le_bytes());
    comment.push(1);
    for packet in [ident, comment, floor_zero_setup()] {
        writer.write_packet(packet.into_boxed_slice(), 1, PacketWriteEndInfo::EndPage, 0).unwrap();
    }
    writer.into_inner().into_inner()
}
#[test]
fn native_rejects_floor_zero_while_default_support_remains() {
    let setup = floor_zero_setup();
    assert!(read_header_setup(&setup, 1, (6, 6)).is_ok());
    assert!(read_header_setup_native(&setup, 1, (6, 6)).is_err());
}

#[test]
fn native_reader_rejects_floor_zero_at_initial_header() {
    use lewton::inside_ogg::OggStreamReader;
    use std::io::Cursor;
    let bytes = floor_zero_ogg();
    assert!(OggStreamReader::new(Cursor::new(&bytes)).is_ok());
    assert!(OggStreamReader::new_native(Cursor::new(&bytes)).is_err());
}
