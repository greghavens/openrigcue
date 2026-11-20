//! Package OpenRigCue game scripts, descriptors, and icon in GIANTS-compatible ZIPs.

use crate::{deflate7z, icon, script_fs19_fs22, LUA_FS25, MODDESC_FS19, MODDESC_FS22, MODDESC_FS25};

/// Farming Simulator edition a zip is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    Fs19,
    Fs22,
    Fs25,
}

/// Supported Farming Simulator editions.
pub const EDITIONS: [Edition; 3] = [Edition::Fs19, Edition::Fs22, Edition::Fs25];

/// One file inside the mod zip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Icon,
    ModDesc,
    Script,
}

impl Entry {
    pub fn file_name(self) -> &'static str {
        match self {
            Entry::Icon => "icon.png",
            Entry::ModDesc => "modDesc.xml",
            Entry::Script => "TelemetryMod.lua",
        }
    }
}

/// "Version needed to extract" of every entry (2.0: deflate).
pub const VERSION_NEEDED: u16 = 20;
/// Compression method of every entry (8 = deflate).
pub const METHOD_DEFLATE: u16 = 8;
/// General-purpose flag bit 3: sizes and CRC follow the data in a descriptor.
pub const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;

/// Container conventions shared by every entry of a zip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Container {
    /// "Version made by" of every central entry (low byte spec version,
    /// high byte host system).
    pub version_made_by: u16,
    /// General-purpose bit flags of every entry.
    pub flags: u16,
}

/// Windows-style container of the FS19/FS22 zips: spec 6.3, host 0 (FAT), no flags.
pub const CONTAINER_WINDOWS: Container = Container { version_made_by: 0x003f, flags: 0 };
/// Unix-style container of the FS25 zip: spec 2.0, host 3 (Unix), data descriptors.
pub const CONTAINER_UNIX: Container = Container { version_made_by: 0x0314, flags: FLAG_DATA_DESCRIPTOR };

/// Header fields of one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryMeta {
    pub entry: Entry,
    /// DOS time word (hours << 11 | minutes << 5 | seconds / 2).
    pub dos_time: u16,
    /// DOS date word ((year - 1980) << 9 | month << 5 | day).
    pub dos_date: u16,
    /// External file attributes of the central entry.
    pub external_attributes: u32,
    /// Extra field of the local header.
    pub local_extra: &'static [u8],
    /// Extra field of the central directory entry.
    pub central_extra: &'static [u8],
}

/// NTFS extra field (`0x000a`): reserved, tag 1, 24 bytes of FILETIME
/// mtime, atime, ctime (100 ns units since 1601-01-01 UTC).
pub const fn ntfs_extra(mtime: u64, atime: u64, ctime: u64) -> [u8; 36] {
    let mut out = [0u8; 36];
    let head: [u8; 12] = [0x0a, 0x00, 0x20, 0x00, 0, 0, 0, 0, 0x01, 0x00, 0x18, 0x00];
    let mut i = 0;
    while i < 12 {
        out[i] = head[i];
        i += 1;
    }
    let times = [mtime.to_le_bytes(), atime.to_le_bytes(), ctime.to_le_bytes()];
    let mut t = 0;
    while t < 3 {
        let mut j = 0;
        while j < 8 {
            out[12 + t * 8 + j] = times[t][j];
            j += 1;
        }
        t += 1;
    }
    out
}

/// Local extra field of the FS25 entries: `ux` (uid 0, gid 0) then `UT`
/// with flags 7 (mtime, atime, ctime as Unix epochs).
pub const fn unix_local_extra(mtime: u32, atime: u32, ctime: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    let head: [u8; 20] = [
        0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0, 0, 0, 0, 0x04, 0, 0, 0, 0, // ux
        0x55, 0x54, 0x0d, 0x00, 0x07, // UT, 13 bytes, flags 7
    ];
    let mut i = 0;
    while i < 20 {
        out[i] = head[i];
        i += 1;
    }
    let times = [mtime.to_le_bytes(), atime.to_le_bytes(), ctime.to_le_bytes()];
    let mut t = 0;
    while t < 3 {
        let mut j = 0;
        while j < 4 {
            out[20 + t * 4 + j] = times[t][j];
            j += 1;
        }
        t += 1;
    }
    out
}

/// Central extra field of the FS25 entries: `ux` (uid 0, gid 0) then `UT`
/// with flags 1 (mtime only).
pub const fn unix_central_extra(mtime: u32) -> [u8; 24] {
    let mut out = [0u8; 24];
    let head: [u8; 20] = [
        0x75, 0x78, 0x0b, 0x00, 0x01, 0x04, 0, 0, 0, 0, 0x04, 0, 0, 0, 0, // ux
        0x55, 0x54, 0x05, 0x00, 0x01, // UT, 5 bytes, flags 1
    ];
    let mut i = 0;
    while i < 20 {
        out[i] = head[i];
        i += 1;
    }
    let m = mtime.to_le_bytes();
    let mut j = 0;
    while j < 4 {
        out[20 + j] = m[j];
        j += 1;
    }
    out
}

// FS19 (Windows-style container): NTFS time stamps of the three entries.
const FS19_ICON_NTFS: [u8; 36] = ntfs_extra(0x01d4_4577_723c_cfa6, 0x01d7_e4fd_e125_6cd9, 0x01d7_e3c3_936b_f387);
const FS19_DESC_NTFS: [u8; 36] = ntfs_extra(0x01d7_e4fd_d9bd_a0f1, 0x01d7_e4fd_dc17_a933, 0x01d7_e3c3_936d_3fdb);
const FS19_LUA_NTFS: [u8; 36] = ntfs_extra(0x01d7_e4fc_cb38_a02d, 0x01d7_e4fc_e930_e951, 0x01d7_e4af_6806_91de);
// FS22 (Windows-style container): the script entry is identical to FS19's.
const FS22_ICON_NTFS: [u8; 36] = ntfs_extra(0x01d4_4577_723c_cfa6, 0x01d7_e4fc_e930_aea8, 0x01d7_e3c3_936b_f387);
const FS22_DESC_NTFS: [u8; 36] = ntfs_extra(0x01d7_e4fc_ce99_dc10, 0x01d7_e4fc_f365_b6f7, 0x01d7_e3c3_936d_3fdb);
// FS25 (Unix-style container): Unix time stamps of the three entries.
const FS25_LUA_LOCAL: [u8; 32] = unix_local_extra(0x6737_ce83, 0x6737_ceb3, 0x61a4_e66c);
const FS25_LUA_CENTRAL: [u8; 24] = unix_central_extra(0x6737_ce83);
const FS25_ICON_LOCAL: [u8; 32] = unix_local_extra(0x5b90_f16c, 0x6737_cfdf, 0x5b90_f16c);
const FS25_ICON_CENTRAL: [u8; 24] = unix_central_extra(0x5b90_f16c);
const FS25_DESC_LOCAL: [u8; 32] = unix_local_extra(0x6737_6cd0, 0x6737_ceb4, 0x6737_6cd0);
const FS25_DESC_CENTRAL: [u8; 24] = unix_central_extra(0x6737_6cd0);

/// DOS `archive` attribute.
const ATTR_ARCHIVE: u32 = 0x0000_0020;
/// External attributes of the FS19/FS22 icon and descriptor entries.
const ATTR_WINDOWS_ICON_DESC: u32 = 0x0008_0020;
/// External attributes of the FS25 entries: Unix mode `0100666` in the high word.
const ATTR_UNIX_RW: u32 = 0x81b6_0000;

impl Edition {
    /// Zip file name under which the mod is installed.
    pub fn zip_name(self) -> &'static str {
        match self {
            Edition::Fs19 => "FS19_TelemetryMod.zip",
            Edition::Fs22 => "FS22_TelemetryMod.zip",
            Edition::Fs25 => "FS25_TelemetryMod.zip",
        }
    }

    /// Folder under `Documents\My Games\` whose `mods\` subfolder receives the zip.
    pub fn my_games_dir(self) -> &'static str {
        match self {
            Edition::Fs19 => "FarmingSimulator2019",
            Edition::Fs22 => "FarmingSimulator2022",
            Edition::Fs25 => "FarmingSimulator2025",
        }
    }

    pub fn mod_desc(self) -> &'static [u8] {
        match self {
            Edition::Fs19 => MODDESC_FS19,
            Edition::Fs22 => MODDESC_FS22,
            Edition::Fs25 => MODDESC_FS25,
        }
    }

    pub fn script(self) -> Vec<u8> {
        match self {
            Edition::Fs19 | Edition::Fs22 => script_fs19_fs22(),
            Edition::Fs25 => LUA_FS25.to_vec(),
        }
    }

    /// Container conventions of this edition's zip.
    pub fn container(self) -> Container {
        match self {
            Edition::Fs19 | Edition::Fs22 => CONTAINER_WINDOWS,
            Edition::Fs25 => CONTAINER_UNIX,
        }
    }

    /// Entry order of this edition's zip with each entry's header fields.
    pub fn layout(self) -> [EntryMeta; 3] {
        match self {
            Edition::Fs19 => [
                EntryMeta {
                    entry: Entry::Icon,
                    dos_time: 0x0a96, // 2018-09-06 01:20:44
                    dos_date: 0x4d26,
                    external_attributes: ATTR_WINDOWS_ICON_DESC,
                    local_extra: &[],
                    central_extra: &FS19_ICON_NTFS,
                },
                EntryMeta {
                    entry: Entry::ModDesc,
                    dos_time: 0x4e09, // 2021-11-29 09:48:18
                    dos_date: 0x537d,
                    external_attributes: ATTR_WINDOWS_ICON_DESC,
                    local_extra: &[],
                    central_extra: &FS19_DESC_NTFS,
                },
                EntryMeta {
                    entry: Entry::Script,
                    dos_time: 0x4d16, // 2021-11-29 09:40:44
                    dos_date: 0x537d,
                    external_attributes: ATTR_ARCHIVE,
                    local_extra: &[],
                    central_extra: &FS19_LUA_NTFS,
                },
            ],
            Edition::Fs22 => [
                EntryMeta {
                    entry: Entry::Icon,
                    dos_time: 0x0a96, // 2018-09-06 01:20:44
                    dos_date: 0x4d26,
                    external_attributes: ATTR_WINDOWS_ICON_DESC,
                    local_extra: &[],
                    central_extra: &FS22_ICON_NTFS,
                },
                EntryMeta {
                    entry: Entry::ModDesc,
                    dos_time: 0x4d18, // 2021-11-29 09:40:48
                    dos_date: 0x537d,
                    external_attributes: ATTR_WINDOWS_ICON_DESC,
                    local_extra: &[],
                    central_extra: &FS22_DESC_NTFS,
                },
                EntryMeta {
                    entry: Entry::Script,
                    dos_time: 0x4d16, // 2021-11-29 09:40:44
                    dos_date: 0x537d,
                    external_attributes: ATTR_ARCHIVE,
                    local_extra: &[],
                    central_extra: &FS19_LUA_NTFS,
                },
            ],
            Edition::Fs25 => [
                EntryMeta {
                    entry: Entry::Script,
                    dos_time: 0x8d67, // 2024-11-15 17:43:14 (local time)
                    dos_date: 0x596f,
                    external_attributes: ATTR_UNIX_RW,
                    local_extra: &FS25_LUA_LOCAL,
                    central_extra: &FS25_LUA_CENTRAL,
                },
                EntryMeta {
                    entry: Entry::Icon,
                    dos_time: 0x2a96, // 2018-09-06 05:20:44
                    dos_date: 0x4d26,
                    external_attributes: ATTR_UNIX_RW,
                    local_extra: &FS25_ICON_LOCAL,
                    central_extra: &FS25_ICON_CENTRAL,
                },
                EntryMeta {
                    entry: Entry::ModDesc,
                    dos_time: 0x55cc, // 2024-11-15 10:46:24
                    dos_date: 0x596f,
                    external_attributes: ATTR_UNIX_RW,
                    local_extra: &FS25_DESC_LOCAL,
                    central_extra: &FS25_DESC_CENTRAL,
                },
            ],
        }
    }

    pub fn entry_bytes(self, entry: Entry) -> Vec<u8> {
        match entry {
            Entry::Icon => icon::icon_png(),
            Entry::ModDesc => self.mod_desc().to_vec(),
            Entry::Script => self.script(),
        }
    }
}

impl Edition {
    /// Deterministic compression of the authored script, descriptor, and icon.
    pub fn deflate(self, _entry: Entry, data: &[u8]) -> Vec<u8> {
        deflate7z::deflate(data)
    }
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The zip file bytes for `edition`.
pub fn build_zip(edition: Edition) -> Vec<u8> {
    let container = edition.container();
    let descriptor = container.flags & FLAG_DATA_DESCRIPTOR != 0;
    let mut out = Vec::with_capacity(32 * 1024);
    let mut central = Vec::with_capacity(512);
    let mut count: u16 = 0;
    for meta in edition.layout() {
        let name = meta.entry.file_name().as_bytes();
        let raw = edition.entry_bytes(meta.entry);
        let crc = crc32fast::hash(&raw);
        let compressed = edition.deflate(meta.entry, &raw);
        let (csize, usize_) = (compressed.len() as u32, raw.len() as u32);
        let local_offset = out.len() as u32;

        // local file header
        put32(&mut out, 0x0403_4b50);
        put16(&mut out, VERSION_NEEDED);
        put16(&mut out, container.flags);
        put16(&mut out, METHOD_DEFLATE);
        put16(&mut out, meta.dos_time);
        put16(&mut out, meta.dos_date);
        if descriptor {
            put32(&mut out, 0);
            put32(&mut out, 0);
            put32(&mut out, 0);
        } else {
            put32(&mut out, crc);
            put32(&mut out, csize);
            put32(&mut out, usize_);
        }
        put16(&mut out, name.len() as u16);
        put16(&mut out, meta.local_extra.len() as u16);
        out.extend_from_slice(name);
        out.extend_from_slice(meta.local_extra);
        out.extend_from_slice(&compressed);
        if descriptor {
            put32(&mut out, 0x0807_4b50);
            put32(&mut out, crc);
            put32(&mut out, csize);
            put32(&mut out, usize_);
        }

        // central directory entry
        put32(&mut central, 0x0201_4b50);
        put16(&mut central, container.version_made_by);
        put16(&mut central, VERSION_NEEDED);
        put16(&mut central, container.flags);
        put16(&mut central, METHOD_DEFLATE);
        put16(&mut central, meta.dos_time);
        put16(&mut central, meta.dos_date);
        put32(&mut central, crc);
        put32(&mut central, csize);
        put32(&mut central, usize_);
        put16(&mut central, name.len() as u16);
        put16(&mut central, meta.central_extra.len() as u16);
        put16(&mut central, 0); // comment length
        put16(&mut central, 0); // disk number start
        put16(&mut central, 0); // internal attributes
        put32(&mut central, meta.external_attributes);
        put32(&mut central, local_offset);
        central.extend_from_slice(name);
        central.extend_from_slice(meta.central_extra);
        count += 1;
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    // end of central directory record
    put32(&mut out, 0x0605_4b50);
    put16(&mut out, 0); // this disk
    put16(&mut out, 0); // disk with the central directory
    put16(&mut out, count);
    put16(&mut out, count);
    put32(&mut out, central.len() as u32);
    put32(&mut out, central_offset);
    put16(&mut out, 0); // comment length
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_field_builders_match_the_originals_bytes() {
        // pinned extra-field bytes
        assert_eq!(
            FS19_ICON_NTFS.to_vec(),
            hex("0a0020000000000001001800a6cf3c727745d401d96c25e1fde4d70187f36b93c3e3d701")
        );
        assert_eq!(FS25_LUA_LOCAL.to_vec(), hex("75780b00010400000000040000000055540d000783ce3767b3ce37676ce6a461"));
        assert_eq!(FS25_LUA_CENTRAL.to_vec(), hex("75780b000104000000000400000000555405000183ce3767"));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
