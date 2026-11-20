//! The emulated EEPROM: a byte buffer stored as one NVS blob, with
//! begin, read, write, commit and end, and helpers that store (and commit)
//! and load a length-prefixed string.

use alloc::vec::Vec;

use crate::consts::{EEPROM_NVS_EXPAND_KEY, EEPROM_NVS_NAME, ESP_ERR_NVS_NOT_FOUND};

/// An allocation of `n` bytes: `None` when the allocator has no memory.
/// (On the device the bytes are whatever the heap holds; host fixtures default to zero.)
fn malloc(n: usize) -> Option<Vec<u8>> {
    #[cfg(target_arch = "xtensa")]
    return native_malloc(n);
    #[cfg(not(target_arch = "xtensa"))]
    {
        let mut v = Vec::new();
        v.try_reserve_exact(n).ok()?;
        v.resize(n, 0);
        Some(v)
    }
}
include!("../../../esp32-common/uninitialized.rs");

/// The NVS calls the EEPROM makes (one handle, namespace = blob key = "eeprom").
pub trait NvsBackend {
    /// Opens the namespace `name` for reading and writing.
    fn open(&mut self, namespace: &str) -> i32;
    /// Reads the blob `key`; `out = None` queries the length.
    /// On return `len` holds the stored length.
    fn get_blob(&mut self, key: &str, out: Option<&mut [u8]>, len: &mut usize) -> i32;
    fn set_blob(&mut self, key: &str, data: &[u8]) -> i32;
    fn erase_key(&mut self, key: &str) -> i32;
    fn commit(&mut self) -> i32;
    fn close(&mut self);
}

pub struct Eeprom<B: NvsBackend> {
    pub nvs: B,
    /// The allocated data buffer.
    data: Option<Vec<u8>>,
    size: usize,
    dirty: bool,
}

impl<B: NvsBackend> Eeprom<B> {
    pub fn new(nvs: B) -> Self {
        Self { nvs, data: None, size: 0, dirty: false }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.data {
            Some(d) => &d[..self.size.min(d.len())],
            None => &[],
        }
    }

    /// Begins with a buffer of `size` bytes, loaded from the stored blob.
    pub fn begin(&mut self, size: usize) -> bool {
        if size == 0 {
            return false;
        }
        if self.nvs.open(EEPROM_NVS_NAME) != 0 {
            return false;
        }
        let mut key_size = 0usize;
        let res = self.nvs.get_blob(EEPROM_NVS_NAME, None, &mut key_size);
        if res != 0 && res != ESP_ERR_NVS_NOT_FOUND {
            return false;
        }
        if size < key_size {
            // Shrink: read the whole old blob into a new allocation, write back
            // its first `size` bytes.
            let Some(mut buf) = malloc(key_size) else { return false };
            let mut ks = key_size;
            self.nvs.get_blob(EEPROM_NVS_NAME, Some(&mut buf[..]), &mut ks);
            self.nvs.set_blob(EEPROM_NVS_NAME, &buf[..size]);
            self.nvs.commit();
        } else if key_size < size {
            // Probe that the grown blob fits by writing an "expand" key of the
            // missing size (its bytes are the allocation's contents on the device).
            let expand_size = size - key_size;
            let Some(expand) = malloc(expand_size) else { return false };
            if self.nvs.set_blob(EEPROM_NVS_EXPAND_KEY, &expand) != 0 {
                return res != 0 && res != ESP_ERR_NVS_NOT_FOUND;
            }
            drop(expand);
            self.nvs.erase_key(EEPROM_NVS_EXPAND_KEY);
            let Some(mut buf) = malloc(size) else { return false };
            buf.fill(0xff);
            if key_size != 0 {
                // The blob is read into a buffer of `size`.
                let mut ks = key_size;
                self.nvs.get_blob(EEPROM_NVS_NAME, Some(&mut buf[..]), &mut ks);
                self.nvs.erase_key(EEPROM_NVS_NAME);
            }
            self.nvs.commit();
            self.nvs.set_blob(EEPROM_NVS_NAME, &buf[..size]);
            self.nvs.commit();
        }
        // The old buffer is freed and a new one of `size` bytes allocated.
        self.data = None;
        let Some(mut data) = malloc(size) else { return false };
        self.size = size;
        let mut sz = size;
        self.nvs.get_blob(EEPROM_NVS_NAME, Some(&mut data[..]), &mut sz);
        self.size = sz;
        self.data = Some(data);
        true
    }

    /// Reads the byte at `address`.
    pub fn read(&self, address: i32) -> u8 {
        match &self.data {
            Some(d) if address >= 0 && (address as usize) < self.size => d.get(address as usize).copied().unwrap_or(0),
            _ => 0,
        }
    }

    /// Writes `value` at `address`: marks dirty only on change.
    pub fn write(&mut self, address: i32, value: u8) {
        let size = self.size;
        if let Some(d) = &mut self.data {
            if address >= 0 && (address as usize) < size {
                if let Some(b) = d.get_mut(address as usize) {
                    if *b != value {
                        *b = value;
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Commits a dirty buffer: the blob is set; no NVS commit follows.
    pub fn commit(&mut self) -> bool {
        if self.size == 0 {
            return false;
        }
        let Some(d) = &self.data else { return false };
        if !self.dirty {
            return true;
        }
        if self.nvs.set_blob(EEPROM_NVS_NAME, &d[..self.size.min(d.len())]) == 0 {
            self.dirty = false;
            return true;
        }
        false
    }

    /// Ends the EEPROM.
    pub fn end(&mut self) {
        if self.size != 0 {
            self.commit();
            self.data = None;
            self.size = 0;
            self.nvs.close();
        }
    }

    /// Stores a string at an offset: length byte then the characters
    /// (length truncated to 8 bits), then a commit.
    pub fn write_string(&mut self, addr_offset: i32, s: &[u8]) {
        let len = s.len() as u8;
        self.write(addr_offset, len);
        for i in 0..len as usize {
            // A character past the end counts as 0; len <= s.len() here.
            let v = s.get(i).copied().unwrap_or(0);
            self.write(addr_offset + 1 + i as i32, v);
        }
        self.commit();
    }

    /// Loads the string at an offset: returns the stored bytes up to the
    /// first NUL and their count.
    pub fn read_string(&self, addr_offset: i32, out: &mut [u8; 256]) -> usize {
        let new_len = self.read(addr_offset) as usize;
        for i in 0..new_len {
            out[i] = self.read(addr_offset + 1 + i as i32);
        }
        out[new_len.min(255)] = 0;
        out.iter().position(|&b| b == 0).unwrap_or(new_len)
    }
}
