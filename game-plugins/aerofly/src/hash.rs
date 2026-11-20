//! Aerofly SDK `tm_string_hash`: 64-bit FNV-1a over the name's bytes
//! *including* the terminating NUL, each byte sign-extended (`char` is
//! signed on MSVC).
//!
//! `h = 0xcbf29ce484222325;
//! h = (h ^ (i64)c) * 0x100000001b3` for every byte up to and including NUL.

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hash of `name` as the SDK computes it (NUL included).
pub const fn tm_string_hash(name: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    let mut i = 0;
    while i <= name.len() {
        let c = if i < name.len() { name[i] } else { 0 };
        // sign extension of a signed char to 64 bits
        h ^= (c as i8) as i64 as u64;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h
}

/// Names whose hashes Update compares against.
pub const AIRCRAFT_PITCH: u64 = tm_string_hash(b"Aircraft.Pitch");
/// 0xdccdbce630a0100f.
pub const AIRCRAFT_BANK: u64 = tm_string_hash(b"Aircraft.Bank");
pub const AIRCRAFT_RATE_OF_TURN: u64 = tm_string_hash(b"Aircraft.RateOfTurn");
pub const AIRCRAFT_INDICATED_AIRSPEED: u64 = tm_string_hash(b"Aircraft.IndicatedAirspeed");
/// 0x5e834c2d13903e88.
pub const AIRCRAFT_GEAR: u64 = tm_string_hash(b"Aircraft.Gear");
/// 0x6784cc4bb5aed80c.
pub const AIRCRAFT_NAME: u64 = tm_string_hash(b"Aircraft.Name");
pub const AIRCRAFT_ANGULAR_VELOCITY: u64 = tm_string_hash(b"Aircraft.AngularVelocity");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediates_from_original_match() {
        // the 64-bit constants Update compares with
        assert_eq!(AIRCRAFT_BANK, 0xdccd_bce6_30a0_100f);
        assert_eq!(AIRCRAFT_GEAR, 0x5e83_4c2d_1390_3e88);
        assert_eq!(AIRCRAFT_NAME, 0x6784_cc4b_b5ae_d80c);
    }

    #[test]
    fn runtime_hashes_match_independent_reference() {
        // Computed independently (FNV-1a over the name and its NUL).
        assert_eq!(AIRCRAFT_PITCH, 0x9848_9fca_9a0c_6699);
        assert_eq!(AIRCRAFT_RATE_OF_TURN, 0xce9b_1740_8ef4_c81d);
        assert_eq!(AIRCRAFT_INDICATED_AIRSPEED, 0x747f_701e_9298_a469);
        assert_eq!(AIRCRAFT_ANGULAR_VELOCITY, 0xc333_8419_07d7_4f04);
    }

    #[test]
    fn high_bytes_are_sign_extended() {
        // (i64)(char)0x80 = 0xffffffffffffff80
        let expect = ((FNV_OFFSET ^ 0xffff_ffff_ffff_ff80).wrapping_mul(FNV_PRIME))
            .wrapping_mul(FNV_PRIME);
        assert_eq!(tm_string_hash(&[0x80]), expect);
    }
}
