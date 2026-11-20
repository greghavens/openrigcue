//! Local firmware selection and native bootloader transports. No automatic updates or arbitrary image guessing.
use openrigcue_devices::serial::{Link, OpenOptions, SerialLink};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub path: PathBuf,
    pub bytes: usize,
    pub address: u32,
    pub format: String,
}
#[derive(Clone, Debug)]
pub struct Payload {
    pub address: u32,
    pub bytes: Vec<u8>,
}
pub fn load(path: &Path, address: Option<u32>) -> Result<Payload, String> {
    use std::io::Read;
    let mut b = Vec::new();
    fs::File::open(path).and_then(|f| f.take(8 * 1024 * 1024 + 1).read_to_end(&mut b)).map_err(|e| e.to_string())?;
    if b.is_empty() || b.len() > 8 * 1024 * 1024 {
        return Err("Firmware image is empty or exceeds 8 MiB".into());
    }
    if path.extension().is_some_and(|s| s.eq_ignore_ascii_case("hex")) {
        decode_hex(std::str::from_utf8(&b).map_err(|_| "HEX file is not text")?)
    } else {
        Ok(Payload { address: address.ok_or("Select the documented load address for a binary image")?, bytes: b })
    }
}
pub fn decode_hex(text: &str) -> Result<Payload, String> {
    let mut bytes = std::collections::BTreeMap::new();
    let mut base = 0u32;
    let mut ended = false;
    for (line_number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if ended {
            return Err("Data follows HEX end-of-file".into());
        }
        let s = line.strip_prefix(':').ok_or("Invalid HEX record")?;
        if !s.is_ascii() {
            return Err("HEX record contains non-ASCII data".into());
        }
        if s.len() % 2 != 0 {
            return Err("Invalid HEX byte count".into());
        }
        let raw = (0..s.len())
            .step_by(2)
            .map(|n| u8::from_str_radix(&s[n..n + 2], 16).map_err(|_| "Invalid HEX digit"))
            .collect::<Result<Vec<_>, _>>()?;
        if raw.len() < 5 || raw.len() != raw[0] as usize + 5 || raw.iter().fold(0u8, |s, &b| s.wrapping_add(b)) != 0 {
            return Err(format!("Invalid HEX checksum/length at line {}", line_number + 1));
        }
        let offset = u16::from_be_bytes([raw[1], raw[2]]) as u32;
        let data = &raw[4..raw.len() - 1];
        match raw[3] {
            0 => {
                for (i, &b) in data.iter().enumerate() {
                    let at =
                        base.checked_add(offset).and_then(|a| a.checked_add(i as u32)).ok_or("HEX address overflow")?;
                    if let Some(old) = bytes.insert(at, b) {
                        if old != b {
                            return Err("Conflicting HEX records".into());
                        }
                    }
                }
            }
            1 if data.is_empty() => ended = true,
            2 if data.len() == 2 => base = u32::from(u16::from_be_bytes([data[0], data[1]])) << 4,
            4 if data.len() == 2 => base = u32::from(u16::from_be_bytes([data[0], data[1]])) << 16,
            3 | 5 if data.len() == 4 => {}
            _ => return Err("Unsupported HEX record".into()),
        }
    }
    if !ended || bytes.is_empty() {
        return Err("HEX image has no data or EOF".into());
    }
    let low = *bytes.keys().next().unwrap();
    let high = *bytes.keys().next_back().unwrap();
    if high - low >= 8 * 1024 * 1024 {
        return Err("HEX address span exceeds 8 MiB".into());
    }
    let mut out = vec![0xff; (high - low + 1) as usize];
    for (a, b) in bytes {
        out[(a - low) as usize] = b;
    }
    Ok(Payload { address: low, bytes: out })
}
pub fn inventory(root: &Path) -> Result<Vec<Image>, String> {
    fn walk(p: &Path, depth: usize, out: &mut Vec<Image>) {
        if depth == 0 {
            return;
        }
        for e in fs::read_dir(p).into_iter().flatten().flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&p, depth - 1, out);
            } else if p.extension().is_some_and(|s| s.eq_ignore_ascii_case("hex") || s.eq_ignore_ascii_case("bin")) {
                if let Ok(b) = fs::metadata(&p) {
                    out.push(Image {
                        format: p.extension().unwrap().to_string_lossy().into(),
                        path: p,
                        bytes: b.len() as usize,
                        address: 0,
                    });
                }
            }
        }
    }
    let mut out = Vec::new();
    if !root.is_dir() {
        return Err("Select the firmware directory supplied for your controller".into());
    }
    walk(root, 5, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Import only firmware data from a vendor ZIP. Never extract or execute packaged programs/scripts.
pub fn import_archive(bytes: Vec<u8>, destination: &Path) -> Result<Vec<Image>, String> {
    use std::io::{Read, Write};
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("Firmware archive exceeds 32 MiB".into());
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    let mut total = 0u64;
    for n in 0..archive.len() {
        let mut file = archive.by_index(n).map_err(|e| e.to_string())?;
        let path = file.enclosed_name().ok_or("Unsafe archive path")?;
        if file.is_dir()
            || !path.extension().is_some_and(|s| s.eq_ignore_ascii_case("bin") || s.eq_ignore_ascii_case("hex"))
        {
            continue;
        }
        if file.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err("Firmware archive contains a symbolic link".into());
        }
        total = total.checked_add(file.size()).ok_or("Archive size overflow")?;
        if file.size() > 8 * 1024 * 1024 || total > 32 * 1024 * 1024 {
            return Err("Firmware archive expands beyond its size limit".into());
        }
        let mut data = Vec::new();
        (&mut file).take(8 * 1024 * 1024 + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
        if data.is_empty() || data.len() > 8 * 1024 * 1024 {
            return Err("Invalid firmware member size".into());
        }
        files.push((path.to_path_buf(), data));
    }
    if files.is_empty() {
        return Err("This archive contains no BIN or HEX firmware images".into());
    }
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let root = fs::canonicalize(destination).map_err(|e| e.to_string())?;
    for (relative, data) in files {
        let mut parent = root.clone();
        let components: Vec<_> = relative.components().collect();
        for part in &components[..components.len() - 1] {
            parent.push(part.as_os_str());
            if fs::symlink_metadata(&parent).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err("Import destination contains a symbolic link".into());
            }
            fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        }
        let target = root.join(relative);
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Import target is a symbolic link".into());
        }
        if target.exists() {
            if fs::read(&target).map_err(|e| e.to_string())? == data {
                continue;
            }
            return Err(format!("Refusing to replace existing firmware {}", target.display()));
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .and_then(|mut f| f.write_all(&data))
            .map_err(|e| e.to_string())?;
    }
    inventory(&root)
}
pub fn download_vendor_archive(destination: &Path) -> Result<Vec<Image>, String> {
    use std::io::Read;
    let mut response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?
        .get("https://dofreality.com/firmwares/DOFLoad.zip")
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    (&mut response).take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    import_archive(bytes, destination)
}

pub trait Transport {
    fn exchange(&mut self, write: &[u8], read: usize) -> Result<Vec<u8>, String>;
}
pub struct SerialTransport {
    link: Box<dyn Link>,
    timeout: Duration,
}
impl SerialTransport {
    pub fn open(path: &str, baud: u32) -> Result<Self, String> {
        let link =
            SerialLink::open(path, &OpenOptions { baud_rate: baud, write_timeout: Some(Duration::from_secs(3)) })
                .map_err(|e| e.to_string())?;
        // AVR bootloaders listen briefly after DTR resets the controller. Waiting for the
        // normal SMC startup delay here would miss that window.
        std::thread::sleep(Duration::from_millis(80));
        Ok(Self { link: Box::new(link), timeout: Duration::from_millis(600) })
    }
}
impl Transport for SerialTransport {
    fn exchange(&mut self, write: &[u8], read: usize) -> Result<Vec<u8>, String> {
        if write == [0x30, 0x20] {
            self.link.clear_input().map_err(|e| e.to_string())?;
        }
        self.link.write_all(write).map_err(|e| e.to_string())?;
        let start = Instant::now();
        let mut bytes = Vec::new();
        while bytes.len() < read && start.elapsed() < self.timeout {
            bytes.extend(self.link.read_available().map_err(|e| e.to_string())?);
            if bytes.len() < read {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        if bytes.len() != read {
            return Err(format!("Bootloader expected {read} bytes, received {}", bytes.len()));
        }
        Ok(bytes)
    }
}
fn stk(io: &mut dyn Transport, command: &[u8], read: usize) -> Result<Vec<u8>, String> {
    let mut c = command.to_vec();
    c.push(0x20);
    let reply = io.exchange(&c, read + 2)?;
    if reply.first() != Some(&0x14) || reply.last() != Some(&0x10) {
        return Err(format!("AVR bootloader rejected command {:02x}", command[0]));
    }
    Ok(reply[1..reply.len() - 1].to_vec())
}
pub fn flash_avr(
    io: &mut dyn Transport,
    image: &Payload,
    mut progress: impl FnMut(usize, usize),
) -> Result<(), String> {
    if image.bytes.is_empty()
        || image.address >= 32256
        || image.bytes.len() > 32256 - image.address as usize
        || image.address & 127 != 0
    {
        return Err("Image must start on an ATmega328P flash page and fit below its bootloader".into());
    }
    let mut sync_error = None;
    for _ in 0..5 {
        match stk(io, &[0x30], 0) {
            Ok(_) => {
                sync_error = None;
                break;
            }
            Err(e) => sync_error = Some(e),
        }
    }
    if let Some(error) = sync_error {
        return Err(error);
    }
    let signature = stk(io, &[0x75], 3)?;
    if signature != [0x1e, 0x95, 0x0f] {
        return Err(format!("Expected ATmega328P, found {signature:02x?}"));
    }
    stk(io, &[0x50], 0)?;
    for (i, page) in image.bytes.chunks(128).enumerate() {
        let mut padded = page.to_vec();
        if padded.len() & 1 != 0 {
            padded.push(255);
        }
        let address = (image.address as usize + i * 128) / 2;
        stk(io, &[0x55, address as u8, (address >> 8) as u8], 0)?;
        let mut command = vec![0x64, 0, padded.len() as u8, b'F'];
        command.extend(&padded);
        stk(io, &command, 0)?;
        stk(io, &[0x55, address as u8, (address >> 8) as u8], 0)?;
        if stk(io, &[0x74, 0, padded.len() as u8, b'F'], padded.len())? != padded {
            return Err(format!("Flash verification failed at 0x{:x}", address * 2));
        }
        progress((i * 128 + page.len()).min(image.bytes.len()), image.bytes.len());
    }
    stk(io, &[0x51], 0)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn intel_hex_checksum_address_and_eof() {
        let p = decode_hex(":0400000001020304F2\n:00000001FF\n").unwrap();
        assert_eq!(p.bytes, [1, 2, 3, 4]);
        assert!(decode_hex(":0400000001020304F3\n:00000001FF").is_err());
        assert!(decode_hex(":00000001FF").is_err());
    }
    struct Fake {
        calls: Vec<Vec<u8>>,
        page: Vec<u8>,
    }
    impl Transport for Fake {
        fn exchange(&mut self, w: &[u8], _: usize) -> Result<Vec<u8>, String> {
            self.calls.push(w.into());
            let mut r = vec![0x14];
            match w[0] {
                0x75 => r.extend([0x1e, 0x95, 0x0f]),
                0x64 => self.page = w[4..w.len() - 1].into(),
                0x74 => r.extend(&self.page),
                _ => {}
            }
            r.push(0x10);
            Ok(r)
        }
    }
    #[test]
    fn avr_writes_and_reads_every_page_before_leaving() {
        let mut f = Fake { calls: vec![], page: vec![] };
        let p = Payload { address: 0, bytes: vec![42; 200] };
        flash_avr(&mut f, &p, |_, _| {}).unwrap();
        assert_eq!(f.calls.iter().filter(|w| w[0] == 0x64).count(), 2);
        assert_eq!(f.calls.iter().filter(|w| w[0] == 0x74).count(), 2);
        assert_eq!(f.calls.last().unwrap(), &[0x51, 0x20]);
    }
}
