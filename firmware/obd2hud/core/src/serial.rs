//! Serial number and EEPROM identity bytes.

use crate::consts::{EEPROM_ID_LEN, EEPROM_MAC_OFFSET, EEPROM_SALT_OFFSET};
use crate::eeprom::{Eeprom, NvsBackend};
use crate::random::random;

/// The unique id: id[0] = id[1] = 0, id[2..8] = the eFuse MAC bytes in
/// ascending significance, i.e. the factory MAC in its natural byte order.
pub fn unique_id(mac: &[u8; 6]) -> [u8; 8] {
    let chip = u64::from_le_bytes([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0, 0]);
    let mut id = [0u8; 8];
    for (i, slot) in id[2..].iter_mut().enumerate() {
        *slot = (chip >> (8 * i)) as u8;
    }
    id
}

/// Checks the identity bytes: for i in 6..12, if EEPROM[i-6] != id[i-4] then write the
/// MAC byte to EEPROM[i-6] and a random byte to EEPROM[i]; commit if anything changed.
pub fn check_eeprom<B: NvsBackend, R: FnMut() -> u32>(ee: &mut Eeprom<B>, id: &[u8; 8], esp_random: &mut R) {
    let mut need_commit = false;
    for i in (EEPROM_SALT_OFFSET as i32)..(EEPROM_SALT_OFFSET + EEPROM_ID_LEN) as i32 {
        let mac_addr = i - EEPROM_SALT_OFFSET as i32 + EEPROM_MAC_OFFSET as i32;
        let want = id[(i - 4) as usize];
        if ee.read(mac_addr) != want {
            ee.write(mac_addr, want);
            let r = random(0x100, esp_random);
            ee.write(i, r as u8);
            need_commit = true;
        }
    }
    if need_commit {
        ee.commit();
    }
}

/// The serial number: hex(EEPROM[6..12]) followed by hex(id[2..8]),
/// 24 lower-case characters.
pub fn serial_number<B: NvsBackend>(ee: &Eeprom<B>, id: &[u8; 8]) -> [u8; 24] {
    let mut raw = [0u8; 12];
    for k in 0..EEPROM_ID_LEN {
        raw[k] = ee.read((EEPROM_SALT_OFFSET + k) as i32);
    }
    raw[EEPROM_ID_LEN..].copy_from_slice(&id[2..8]);
    crate::gate::hex_lower(&raw)
}
