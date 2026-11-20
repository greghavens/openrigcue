//! STM32 DfuSe over Linux usbfs, following ST AN3156. Only addressed flash sectors are erased.
use crate::{firmware::Payload, usb};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

pub trait Control {
    fn transfer(&mut self, input: bool, request: u8, value: u16, bytes: &mut [u8]) -> Result<usize, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sector {
    pub address: u32,
    pub length: usize,
}

pub fn memory_map(text: &str) -> Result<Vec<Sector>, String> {
    let mut out = Vec::new();
    let mut sections = text.split('/').skip(1);
    while let Some(base) = sections.next() {
        let mut address =
            u32::from_str_radix(base.trim().trim_start_matches("0x"), 16).map_err(|_| "Invalid DFU memory address")?;
        let groups = sections.next().ok_or("Missing DFU memory layout")?;
        for group in groups.split(',') {
            let (count, size) = group.trim().split_once('*').ok_or("Invalid DFU sector group")?;
            let count = count.trim().parse::<usize>().map_err(|_| "Invalid DFU sector count")?;
            let digits = size.trim().bytes().take_while(u8::is_ascii_digit).count();
            let size = size.trim();
            let amount = size[..digits].parse::<usize>().map_err(|_| "Invalid DFU sector size")?;
            let unit = size.as_bytes().get(digits).copied().ok_or("Missing DFU size unit")?;
            let multiplier = match unit {
                b'B' => 1,
                b'K' => 1024,
                b'M' => 1024 * 1024,
                _ => return Err("Unknown DFU size unit".into()),
            };
            let length = amount
                .checked_mul(multiplier)
                .filter(|&n| n > 0 && n <= 1024 * 1024)
                .ok_or("Invalid DFU sector size")?;
            if count == 0 || count > 4096 || out.len() + count > 4096 {
                return Err("Invalid DFU sector count".into());
            }
            for _ in 0..count {
                out.push(Sector { address, length });
                address = address.checked_add(length as u32).ok_or("DFU map overflow")?;
            }
        }
    }
    if out.is_empty() {
        return Err("Controller did not advertise its flash layout".into());
    }
    Ok(out)
}

pub struct Dfu<T> {
    pub io: T,
    pub transfer_size: usize,
    pub sectors: Vec<Sector>,
}
impl<T: Control> Dfu<T> {
    fn send(&mut self, request: u8, value: u16, bytes: &[u8]) -> Result<(), String> {
        let mut bytes = bytes.to_vec();
        if self.io.transfer(false, request, value, &mut bytes)? != bytes.len() {
            return Err("Short DFU write".into());
        }
        Ok(())
    }
    fn status(&mut self) -> Result<(u8, Duration), String> {
        let mut bytes = [0; 6];
        if self.io.transfer(true, 3, 0, &mut bytes)? != 6 {
            return Err("Truncated DFU status".into());
        }
        if bytes[0] != 0 {
            return Err(format!("Controller rejected DFU operation (status {}, state {})", bytes[0], bytes[4]));
        }
        let millis = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], 0]);
        Ok((bytes[4], Duration::from_millis(u64::from(millis))))
    }
    fn idle(&mut self) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let (state, delay) = self.status()?;
            if matches!(state, 2 | 5) {
                return Ok(());
            }
            if !matches!(state, 3 | 4) || start.elapsed() > Duration::from_secs(30) {
                return Err(format!("Unexpected DFU state {state}"));
            }
            std::thread::sleep(delay.max(Duration::from_millis(1)).min(Duration::from_secs(1)));
        }
    }
    fn address(&mut self, address: u32) -> Result<(), String> {
        self.send(6, 0, &[])?;
        let mut command = vec![0x21];
        command.extend(address.to_le_bytes());
        self.send(1, 0, &command)?;
        self.idle()
    }
    pub fn read(&mut self, address: u32, length: usize) -> Result<Vec<u8>, String> {
        if self.transfer_size == 0 || self.transfer_size > 4096 || length > 2 * 1024 * 1024 {
            return Err("Invalid DFU read size".into());
        }
        self.address(address)?;
        self.send(6, 0, &[])?;
        let mut result = Vec::with_capacity(length);
        for (i, n) in (0..length).step_by(self.transfer_size).enumerate() {
            let mut bytes = vec![0; self.transfer_size.min(length - n)];
            let block = u16::try_from(i + 2).map_err(|_| "DFU upload is too large")?;
            let read = self.io.transfer(true, 2, block, &mut bytes)?;
            if read != bytes.len() {
                return Err("Truncated DFU memory read".into());
            }
            result.extend(bytes);
        }
        self.send(6, 0, &[])?;
        Ok(result)
    }
    pub fn flash(&mut self, image: &Payload, mut progress: impl FnMut(usize, usize)) -> Result<(), String> {
        if self.transfer_size == 0 || self.transfer_size > 4096 {
            return Err("Invalid DFU transfer size".into());
        }
        let end = image
            .address
            .checked_add(image.bytes.len().try_into().map_err(|_| "Image too large")?)
            .ok_or("Image address overflow")?;
        if image.bytes.is_empty() || !(0x08000000..0x08200000).contains(&image.address) || end > 0x08200000 {
            return Err("Select an STM32 internal-flash image".into());
        }
        let sectors: Vec<_> = self
            .sectors
            .iter()
            .filter(|s| s.address < end && s.address.saturating_add(s.length as u32) > image.address)
            .cloned()
            .collect();
        let mut next = image.address;
        for s in &sectors {
            if s.address > next {
                return Err("Image crosses an unavailable flash region".into());
            }
            next = s.address.checked_add(s.length as u32).ok_or("Sector address overflow")?;
        }
        if next < end {
            return Err("Image exceeds this controller's advertised flash".into());
        }
        // Read before erasing so bytes outside the supplied image survive shared-sector updates.
        let mut prepared = Vec::new();
        for sector in sectors {
            let mut bytes = self.read(sector.address, sector.length)?;
            let lo = sector.address.max(image.address);
            let hi = (sector.address + sector.length as u32).min(end);
            bytes[(lo - sector.address) as usize..(hi - sector.address) as usize]
                .copy_from_slice(&image.bytes[(lo - image.address) as usize..(hi - image.address) as usize]);
            prepared.push((sector, bytes));
        }
        let total = prepared.iter().map(|(_, b)| b.len()).sum();
        let mut done = 0;
        for (sector, bytes) in prepared {
            self.send(6, 0, &[])?;
            let mut erase = vec![0x41];
            erase.extend(sector.address.to_le_bytes());
            self.send(1, 0, &erase)?;
            self.idle()?;
            self.address(sector.address)?;
            for (i, chunk) in bytes.chunks(self.transfer_size).enumerate() {
                self.send(1, u16::try_from(i + 2).map_err(|_| "DFU image too large")?, chunk)?;
                self.idle()?;
            }
            if self.read(sector.address, bytes.len())? != bytes {
                return Err(format!("Flash verification failed at 0x{:08x}", sector.address));
            }
            done += bytes.len();
            progress(done, total);
        }
        Ok(())
    }
    pub fn reboot(&mut self, address: u32) -> Result<(), String> {
        self.address(address)?;
        self.send(1, 2, &[])?;
        // Manifestation starts only after GETSTATUS. A reset can disconnect before its reply.
        let mut bytes = [0; 6];
        match self.io.transfer(true, 3, 0, &mut bytes) {
            Ok(6) if bytes[0] == 0 => Ok(()),
            Err(e) if e.contains("No such device") => Ok(()),
            Ok(_) => Err("DFU reboot returned an invalid status".into()),
            Err(e) => Err(e),
        }
    }
}

pub struct LinuxControl {
    file: fs::File,
    interface: u16,
}
#[allow(unsafe_code)]
impl Control for LinuxControl {
    fn transfer(&mut self, input: bool, request: u8, value: u16, bytes: &mut [u8]) -> Result<usize, String> {
        use std::os::fd::AsRawFd;
        #[repr(C)]
        struct Request {
            kind: u8,
            request: u8,
            value: u16,
            index: u16,
            length: u16,
            timeout: u32,
            data: *mut u8,
        }
        let mut c = Request {
            kind: if input { 0xa1 } else { 0x21 },
            request,
            value,
            index: self.interface,
            length: bytes.len().try_into().map_err(|_| "USB transfer too large")?,
            timeout: 5000,
            data: bytes.as_mut_ptr(),
        };
        // SAFETY: Request matches usbdevfs_ctrltransfer; its data buffer and fd outlive ioctl.
        let result = unsafe { libc::ioctl(self.file.as_raw_fd(), 0xc0185500u64 as libc::c_ulong, &mut c) };
        if result < 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(result as usize)
        }
    }
}
#[allow(unsafe_code)]
impl Drop for LinuxControl {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let n = u32::from(self.interface);
        // SAFETY: RELEASEINTERFACE reads one interface number; fd remains open.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), 0x80045510u64 as libc::c_ulong, &n);
        }
    }
}
#[allow(unsafe_code)]
pub fn open(id: &str) -> Result<Dfu<LinuxControl>, String> {
    use std::os::fd::AsRawFd;
    let devices = usb::enumerate(Path::new("/sys/bus/usb/devices"))?;
    let d = usb::find(&devices, id)?;
    if d.vendor != 0x0483 || d.product != 0xdf11 {
        return Err("Select an STM32 USB DFU bootloader (0483:df11)".into());
    }
    let iface = d
        .interfaces
        .iter()
        .find(|i| i.class == "fe" && i.subclass == "01" && i.protocol == "02")
        .ok_or("No DFU interface on this device")?;
    let p = d.path.join(&iface.name);
    let get = |key| fs::read_to_string(p.join(key)).map(|s| s.trim().to_string()).map_err(|e| e.to_string());
    let interface = u16::from_str_radix(&get("bInterfaceNumber")?, 16).map_err(|_| "Invalid DFU interface")?;
    let alternate = get("bAlternateSetting")?.parse::<u32>().map_err(|_| "Invalid DFU alternate setting")?;
    let sectors = memory_map(&get("interface")?)?;
    let mut selected = false;
    let mut transfer_size = None;
    for desc in &d.descriptors {
        if desc.kind == 4 && desc.bytes.len() >= 9 {
            selected = desc.bytes[2] == interface as u8 && desc.bytes[3] == alternate as u8;
        }
        if selected && desc.kind == 0x21 && desc.bytes.len() >= 9 {
            transfer_size = Some(u16::from_le_bytes([desc.bytes[5], desc.bytes[6]]) as usize);
            break;
        }
    }
    let transfer_size = transfer_size.filter(|&n| n > 0 && n <= 4096).ok_or("Missing DFU transfer-size descriptor")?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/bus/usb/{:03}/{:03}", d.bus, d.address))
        .map_err(|e| e.to_string())?;
    // SAFETY: the live USB node is held open; the advisory lock coordinates native maintenance instances.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
        return Err("DFU controller is already in use".into());
    }
    let number = u32::from(interface);
    // SAFETY: CLAIMINTERFACE reads a u32 interface number from a live stack value.
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x8004550fu64 as libc::c_ulong, &number) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut io = LinuxControl { file, interface };
    #[repr(C)]
    struct Alternate {
        interface: u32,
        alternate: u32,
    }
    let alt = Alternate { interface: number, alternate };
    // SAFETY: SETINTERFACE reads the two u32 fields in usbdevfs_setinterface.
    if unsafe { libc::ioctl(io.file.as_raw_fd(), 0x80085504u64 as libc::c_ulong, &alt) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut state = [0];
    if io.transfer(true, 5, 0, &mut state)? != 1 {
        return Err("Invalid DFU state reply".into());
    }
    if state[0] == 10 {
        io.transfer(false, 4, 0, &mut [])?;
    }
    Ok(Dfu { io, transfer_size, sectors })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_st_flash_sector_layout() {
        let s = memory_map("@Internal Flash /0x08000000/04*016Kg,01*064Kg,01*128Kg").unwrap();
        assert_eq!(s.len(), 6);
        assert_eq!(s[5], Sector { address: 0x08020000, length: 131072 });
        assert!(memory_map("@Flash /0xffffffff/02*001Kg").is_err());
    }
    struct Fake {
        memory: Vec<u8>,
        address: usize,
        size: usize,
        writes: usize,
    }
    impl Control for Fake {
        fn transfer(&mut self, input: bool, request: u8, value: u16, bytes: &mut [u8]) -> Result<usize, String> {
            match (input, request) {
                (true, 3) => bytes.copy_from_slice(&[0, 0, 0, 0, 5, 0]),
                (false, 1) if value == 0 => {
                    let addr = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) - 0x08000000;
                    self.address = addr as usize;
                    if bytes[0] == 0x41 {
                        self.memory[self.address..self.address + 64].fill(255);
                    }
                }
                (false, 1) => {
                    let offset = self.address + (value as usize - 2) * self.size;
                    self.memory[offset..offset + bytes.len()].copy_from_slice(bytes);
                    self.writes += 1;
                }
                (true, 2) => {
                    let offset = self.address + (value as usize - 2) * self.size;
                    bytes.copy_from_slice(&self.memory[offset..offset + bytes.len()]);
                }
                _ => {}
            }
            Ok(bytes.len())
        }
    }
    #[test]
    fn shared_sector_bytes_survive_erase_program_and_verify() {
        let io = Fake { memory: (0..128).collect(), address: 0, size: 16, writes: 0 };
        let mut dfu = Dfu {
            io,
            transfer_size: 16,
            sectors: vec![Sector { address: 0x08000000, length: 64 }, Sector { address: 0x08000040, length: 64 }],
        };
        dfu.flash(&Payload { address: 0x08000030, bytes: vec![200; 32] }, |_, _| {}).unwrap();
        assert_eq!(&dfu.io.memory[..48], &(0..48).collect::<Vec<u8>>());
        assert_eq!(&dfu.io.memory[48..80], &[200; 32]);
        assert_eq!(&dfu.io.memory[80..], &(80..128).collect::<Vec<u8>>());
        assert_eq!(dfu.io.writes, 8);
    }
}
