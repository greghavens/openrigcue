//! CRC-16/XMODEM (polynomial 0x1021, initial value 0, no reflection, no final XOR).

/// The table entry for byte `c`.
const fn table_entry(c: u16) -> u16 {
    let mut crc: u16 = 0;
    let mut c = c << 8;
    let mut bit = 0;
    while bit < 8 {
        crc = if (crc ^ c) & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        c <<= 1;
        bit += 1;
    }
    crc
}

/// The table of all 256 byte entries.
const TABLE: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = table_entry(i as u16);
        i += 1;
    }
    t
};

/// The CRC-16/XMODEM of `data`.
pub fn crc16_xmodem(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |crc, &b| {
        let index = (crc >> 8) ^ u16::from(b);
        (crc << 8) ^ TABLE[usize::from(index & 0xFF)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmodem_check_value() {
        // CRC-16/XMODEM check value for "123456789".
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
        assert_eq!(crc16_xmodem(b""), 0);
    }

    #[test]
    fn table_matches_initial() {
        assert_eq!(TABLE[1], 0x1021);
        assert_eq!(TABLE[0x80], 0x9188);
        assert_eq!(TABLE[0xFF], 0x1EF0);
    }
}
