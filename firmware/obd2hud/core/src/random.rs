//! Bounded random numbers. The generator is the hardware RNG;
//! seeding the C random generator does not affect it.

/// A random number below `howbig`: Lemire multiply-shift with rejection
/// (the 64-bit product uses the sign-extended `howbig`).
pub fn random<R: FnMut() -> u32>(howbig: i32, esp_random: &mut R) -> i32 {
    let hb = howbig as u32;
    let wide = |x: u32| -> u64 { (x as u64).wrapping_mul(howbig as i64 as u64) };
    let mut m = wide(esp_random());
    let mut l = m as u32;
    if l < hb {
        let mut t = hb.wrapping_neg();
        if t >= hb {
            t = t.wrapping_sub(hb);
            if t >= hb {
                t %= hb;
            }
        }
        while l < t {
            m = wide(esp_random());
            l = m as u32;
        }
    }
    (m >> 32) as u32 as i32
}

/// A random number from `howsmall` up to, but not including, `howbig`.
pub fn random_range<R: FnMut() -> u32>(howsmall: i32, howbig: i32, esp_random: &mut R) -> i32 {
    if howsmall >= howbig {
        return howsmall;
    }
    howsmall.wrapping_add(random(howbig.wrapping_sub(howsmall), esp_random))
}
