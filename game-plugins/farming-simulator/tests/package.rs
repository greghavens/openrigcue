//! Authored scripts, descriptors and icon in compatible mod containers.
//! Independent readers validate all payloads, checksums and container fields.

use std::io::{Cursor, Read};

use openrigcue_fs_mods::package::{build_zip, Edition, EDITIONS, FLAG_DATA_DESCRIPTOR, METHOD_DEFLATE, VERSION_NEEDED};
use openrigcue_fs_mods::{icon, script_fs19_fs22, FS19_FS22_SCRIPT_CRC32, FS19_FS22_SCRIPT_LEN, LUA_FS25};

/// One entry of a parsed zip: central directory fields, local header fields,
/// the optional data descriptor and the inflated data.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    name: String,
    version_made_by: u16,
    version_needed: u16,
    flags: u16,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc: u32,
    csize: u32,
    usize_: u32,
    internal_attributes: u16,
    external_attributes: u32,
    comment_len: u16,
    disk: u16,
    central_extra: Vec<u8>,
    local_version_needed: u16,
    local_flags: u16,
    local_method: u16,
    local_dos_time: u16,
    local_dos_date: u16,
    local_crc: u32,
    local_csize: u32,
    local_usize: u32,
    local_extra: Vec<u8>,
    descriptor: Option<(u32, u32, u32)>,
    data: Vec<u8>,
}

struct Archive {
    entries: Vec<Parsed>,
    /// The local headers and data are laid out back to back from offset 0,
    /// then the central directory, then the end record with nothing after it.
    contiguous: bool,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn parse(b: &[u8]) -> Archive {
    let eocd = b.len() - 22;
    assert_eq!(u32_at(b, eocd), 0x0605_4b50, "end record is the last 22 bytes");
    assert_eq!(u16_at(b, eocd + 4), 0);
    assert_eq!(u16_at(b, eocd + 6), 0);
    let count = u16_at(b, eocd + 8);
    assert_eq!(u16_at(b, eocd + 10), count);
    let central_size = u32_at(b, eocd + 12) as usize;
    let central_offset = u32_at(b, eocd + 16) as usize;
    assert_eq!(u16_at(b, eocd + 20), 0, "no archive comment");
    assert_eq!(central_offset + central_size, eocd, "central directory ends at the end record");
    let mut entries = Vec::new();
    let mut p = central_offset;
    let mut next_local = 0usize;
    let mut contiguous = true;
    for _ in 0..count {
        assert_eq!(u32_at(b, p), 0x0201_4b50);
        let name_len = u16_at(b, p + 28) as usize;
        let extra_len = u16_at(b, p + 30) as usize;
        let comment_len = u16_at(b, p + 32);
        let local_offset = u32_at(b, p + 42) as usize;
        let name = String::from_utf8(b[p + 46..p + 46 + name_len].to_vec()).unwrap();
        let central_extra = b[p + 46 + name_len..p + 46 + name_len + extra_len].to_vec();
        let q = local_offset;
        assert_eq!(u32_at(b, q), 0x0403_4b50);
        let lname_len = u16_at(b, q + 26) as usize;
        let lextra_len = u16_at(b, q + 28) as usize;
        assert_eq!(&b[q + 30..q + 30 + lname_len], name.as_bytes());
        let local_extra = b[q + 30 + lname_len..q + 30 + lname_len + lextra_len].to_vec();
        let data_start = q + 30 + lname_len + lextra_len;
        let csize = u32_at(b, p + 20) as usize;
        let compressed = &b[data_start..data_start + csize];
        let mut data = Vec::new();
        flate2::read::DeflateDecoder::new(compressed).read_to_end(&mut data).unwrap();
        let mut end = data_start + csize;
        let flags = u16_at(b, p + 8);
        let descriptor = if flags & FLAG_DATA_DESCRIPTOR != 0 {
            assert_eq!(u32_at(b, end), 0x0807_4b50, "data descriptor with signature");
            let d = (u32_at(b, end + 4), u32_at(b, end + 8), u32_at(b, end + 12));
            end += 16;
            Some(d)
        } else {
            None
        };
        if q != next_local {
            contiguous = false;
        }
        next_local = end;
        entries.push(Parsed {
            name,
            version_made_by: u16_at(b, p + 4),
            version_needed: u16_at(b, p + 6),
            flags,
            method: u16_at(b, p + 10),
            dos_time: u16_at(b, p + 12),
            dos_date: u16_at(b, p + 14),
            crc: u32_at(b, p + 16),
            csize: csize as u32,
            usize_: u32_at(b, p + 24),
            internal_attributes: u16_at(b, p + 36),
            external_attributes: u32_at(b, p + 38),
            comment_len,
            disk: u16_at(b, p + 34),
            central_extra,
            local_version_needed: u16_at(b, q + 4),
            local_flags: u16_at(b, q + 6),
            local_method: u16_at(b, q + 8),
            local_dos_time: u16_at(b, q + 10),
            local_dos_date: u16_at(b, q + 12),
            local_crc: u32_at(b, q + 14),
            local_csize: u32_at(b, q + 18),
            local_usize: u32_at(b, q + 22),
            local_extra,
            descriptor,
            data,
        });
        p += 46 + name_len + extra_len + comment_len as usize;
    }
    if next_local != central_offset {
        contiguous = false;
    }
    Archive { entries, contiguous }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// Pinned header fields of one zip entry: name, compressed size, DOS time,
/// DOS date, external attributes, central extra field, local extra field.
struct Expect {
    name: &'static str,
    /// Compressed size of the entry's deflate stream.
    csize: u32,
    dos_time: u16,
    dos_date: u16,
    external_attributes: u32,
    central_extra: &'static str,
    local_extra: &'static str,
}

const FS19_EXPECT: [Expect; 3] = [
    Expect {
        name: "icon.png",
        csize: 2907,
        dos_time: 0x0a96,
        dos_date: 0x4d26,
        external_attributes: 0x0008_0020,
        central_extra: "0a0020000000000001001800a6cf3c727745d401d96c25e1fde4d70187f36b93c3e3d701",
        local_extra: "",
    },
    Expect {
        name: "modDesc.xml",
        csize: 274,
        dos_time: 0x4e09,
        dos_date: 0x537d,
        external_attributes: 0x0008_0020,
        central_extra: "0a0020000000000001001800f1a0bdd9fde4d70133a917dcfde4d701db3f6d93c3e3d701",
        local_extra: "",
    },
    Expect {
        name: "TelemetryMod.lua",
        csize: 1184,
        dos_time: 0x4d16,
        dos_date: 0x537d,
        external_attributes: 0x0000_0020,
        central_extra: "0a00200000000000010018002da038cbfce4d70151e930e9fce4d701de910668afe4d701",
        local_extra: "",
    },
];

const FS22_EXPECT: [Expect; 3] = [
    Expect {
        name: "icon.png",
        csize: 2907,
        dos_time: 0x0a96,
        dos_date: 0x4d26,
        external_attributes: 0x0008_0020,
        central_extra: "0a0020000000000001001800a6cf3c727745d401a8ae30e9fce4d70187f36b93c3e3d701",
        local_extra: "",
    },
    Expect {
        name: "modDesc.xml",
        csize: 274,
        dos_time: 0x4d18,
        dos_date: 0x537d,
        external_attributes: 0x0008_0020,
        central_extra: "0a002000000000000100180010dc99cefce4d701f7b665f3fce4d701db3f6d93c3e3d701",
        local_extra: "",
    },
    Expect {
        name: "TelemetryMod.lua",
        csize: 1184,
        dos_time: 0x4d16,
        dos_date: 0x537d,
        external_attributes: 0x0000_0020,
        central_extra: "0a00200000000000010018002da038cbfce4d70151e930e9fce4d701de910668afe4d701",
        local_extra: "",
    },
];

const FS25_EXPECT: [Expect; 3] = [
    Expect {
        name: "TelemetryMod.lua",
        csize: 1270,
        dos_time: 0x8d67,
        dos_date: 0x596f,
        external_attributes: 0x81b6_0000,
        central_extra: "75780b000104000000000400000000555405000183ce3767",
        local_extra: "75780b00010400000000040000000055540d000783ce3767b3ce37676ce6a461",
    },
    Expect {
        name: "icon.png",
        csize: 2907,
        dos_time: 0x2a96,
        dos_date: 0x4d26,
        external_attributes: 0x81b6_0000,
        central_extra: "75780b00010400000000040000000055540500016cf1905b",
        local_extra: "75780b00010400000000040000000055540d00076cf1905bdfcf37676cf1905b",
    },
    Expect {
        name: "modDesc.xml",
        csize: 273,
        dos_time: 0x55cc,
        dos_date: 0x596f,
        external_attributes: 0x81b6_0000,
        central_extra: "75780b0001040000000004000000005554050001d06c3767",
        local_extra: "75780b00010400000000040000000055540d0007d06c3767b4ce3767d06c3767",
    },
];

/// (version made by, flags, entries, modDesc.xml crc32) pinned for each edition's zip.
fn expected(e: Edition) -> (u16, u16, &'static [Expect; 3], u32) {
    match e {
        Edition::Fs19 => (0x003f, 0, &FS19_EXPECT, 0x2752_3c96),
        Edition::Fs22 => (0x003f, 0, &FS22_EXPECT, 0x5b12_c8e8),
        Edition::Fs25 => (0x0314, FLAG_DATA_DESCRIPTOR, &FS25_EXPECT, 0x0d20_eb2f),
    }
}

#[test]
fn authored_mod_payloads_match_their_pinned_layouts() {
    for edition in EDITIONS {
        let (made_by, flags, entries, desc_crc) = expected(edition);
        let archive = parse(&build_zip(edition));
        assert!(archive.contiguous, "{edition:?}: headers, data and central directory back to back");
        assert_eq!(archive.entries.len(), 3);
        for (got, want) in archive.entries.iter().zip(entries) {
            let ctx = format!("{edition:?} {}", want.name);
            assert_eq!(got.name, want.name, "{ctx}");
            assert_eq!(got.version_made_by, made_by, "{ctx}");
            assert_eq!(got.version_needed, VERSION_NEEDED, "{ctx}");
            assert_eq!(got.local_version_needed, VERSION_NEEDED, "{ctx}");
            assert_eq!((got.flags, got.local_flags), (flags, flags), "{ctx}");
            assert_eq!((got.method, got.local_method), (METHOD_DEFLATE, METHOD_DEFLATE), "{ctx}");
            assert_eq!((got.dos_time, got.dos_date), (want.dos_time, want.dos_date), "{ctx}");
            assert_eq!((got.local_dos_time, got.local_dos_date), (want.dos_time, want.dos_date), "{ctx}");
            assert_eq!(got.external_attributes, want.external_attributes, "{ctx}");
            assert_eq!(got.internal_attributes, 0, "{ctx}");
            assert_eq!((got.comment_len, got.disk), (0, 0), "{ctx}");
            assert_eq!(got.central_extra, hex(want.central_extra), "{ctx} central extra");
            assert_eq!(got.local_extra, hex(want.local_extra), "{ctx} local extra");
            assert_eq!(got.crc, crc32fast::hash(&got.data), "{ctx}");
            assert_eq!(got.csize, want.csize, "{ctx}: compressed size");
            assert_eq!(got.usize_ as usize, got.data.len(), "{ctx}");
            if flags & FLAG_DATA_DESCRIPTOR != 0 {
                assert_eq!((got.local_crc, got.local_csize, got.local_usize), (0, 0, 0), "{ctx}");
                assert_eq!(got.descriptor, Some((got.crc, got.csize, got.usize_)), "{ctx}");
            } else {
                assert_eq!(
                    (got.local_crc, got.local_csize, got.local_usize),
                    (got.crc, got.csize, got.usize_),
                    "{ctx}"
                );
                assert_eq!(got.descriptor, None, "{ctx}");
            }
            match want.name {
                "modDesc.xml" => {
                    assert_eq!(got.data, edition.mod_desc());
                    assert_eq!((got.usize_, got.crc), (516, desc_crc), "{ctx}");
                }
                "TelemetryMod.lua" => {
                    if edition == Edition::Fs25 {
                        assert_eq!(got.data, LUA_FS25, "{ctx}");
                        assert_eq!((got.usize_, got.crc), (7048, 0xf3f3_b9a6), "{ctx}");
                    } else {
                        assert_eq!(got.data, script_fs19_fs22(), "{ctx}");
                        assert_eq!(
                            (got.usize_ as usize, got.crc),
                            (FS19_FS22_SCRIPT_LEN, FS19_FS22_SCRIPT_CRC32),
                            "{ctx}"
                        );
                    }
                }
                "icon.png" => {
                    assert_eq!(got.data, icon::ICON_PNG, "{ctx}");
                    assert_eq!((got.usize_ as usize, got.crc), (icon::ICON_LEN, icon::ICON_CRC32), "{ctx}");
                    let dec = png::Decoder::new(Cursor::new(&got.data));
                    let mut r = dec.read_info().unwrap();
                    let info = r.info();
                    assert_eq!((info.width, info.height), (128, 128));
                    assert_eq!(info.color_type, png::ColorType::Rgba);
                    assert_eq!(info.bit_depth, png::BitDepth::Eight);
                    let mut pixels = vec![0; r.output_buffer_size()];
                    let frame = r.next_frame(&mut pixels).unwrap();
                    assert_eq!(frame.buffer_size(), 128 * 128 * 4);
                    assert_eq!(pixels[3], 0, "rounded tile retains transparent corners");
                }
                _ => unreachable!(),
            }
        }
    }
}

/// Size and CRC32 of the built archives with the approved OpenRigCue mark.
fn branded_zip(e: Edition) -> (usize, u32) {
    match e {
        Edition::Fs19 => (4793, 0x9188_0198),
        Edition::Fs22 => (4793, 0xf43c_ea81),
        Edition::Fs25 => (4986, 0x0da2_bb41),
    }
}

#[test]
fn zips_match_the_openrigcue_image_checksums() {
    for edition in EDITIONS {
        let z = build_zip(edition);
        assert_eq!((z.len(), crc32fast::hash(&z)), branded_zip(edition), "{edition:?}");
    }
}

#[test]
fn an_independent_reader_accepts_the_zips() {
    for edition in EDITIONS {
        let bytes = build_zip(edition);
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(z.len(), 3);
        let (_, _, entries, _) = expected(edition);
        for (i, want) in entries.iter().enumerate() {
            let mut f = z.by_index(i).unwrap();
            assert_eq!(f.name(), want.name);
            assert_eq!(f.compression(), zip::CompressionMethod::Deflated);
            let mut data = Vec::new();
            f.read_to_end(&mut data).unwrap();
            assert!(!data.is_empty());
        }
    }
}

#[test]
fn zip_names_and_targets() {
    let names: Vec<_> = EDITIONS.iter().map(|e| (e.zip_name(), e.my_games_dir())).collect();
    assert_eq!(
        names,
        [
            ("FS19_TelemetryMod.zip", "FarmingSimulator2019"),
            ("FS22_TelemetryMod.zip", "FarmingSimulator2022"),
            ("FS25_TelemetryMod.zip", "FarmingSimulator2025"),
        ]
    );
}

#[test]
fn builds_are_deterministic() {
    for e in EDITIONS {
        assert_eq!(build_zip(e), build_zip(e));
    }
}

#[test]
fn descriptors_declare_the_script_and_icon() {
    for e in EDITIONS {
        let d = std::str::from_utf8(e.mod_desc()).unwrap();
        assert!(d.contains("<sourceFile filename=\"TelemetryMod.lua\"/>"));
        assert!(d.contains("<iconFilename>icon.png</iconFilename>"));
        assert!(d.replace(" />", "/>").contains("<multiplayer supported=\"true\"/>"));
        let v = match e {
            Edition::Fs19 => "51",
            Edition::Fs22 => "61",
            Edition::Fs25 => "91",
        };
        assert!(d.contains(&format!("<modDesc descVersion=\"{v}\">")));
    }
}
