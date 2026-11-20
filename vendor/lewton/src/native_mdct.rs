// Native inverse MDCT. Adapted from Xiph's mdct.c algorithm,
// Copyright (c) 1994-2009 Xiph.Org Foundation; BSD terms in COPYING-Xiph.
// The corresponding native stages were verified in the SRS decoder:
// backward 0x18004d980, stage selection 0x18004e260, butterflies
// 0x18004e680 / 0x18004e530 / 0x18004e970 and first stage 0x18004ea50.
// Finite-normal arithmetic is independently verified; exceptional FP modes
// and NaN payload propagation are not certified.
fn butterfly8(x: &mut [f32]) {
    let r0 = x[6] + x[2];
    let r1 = x[6] - x[2];
    let r2 = x[4] + x[0];
    let r3 = x[4] - x[0];
    x[6] = r0 + r2;
    x[4] = r0 - r2;
    let r0 = x[5] - x[1];
    let r2 = x[7] - x[3];
    x[0] = r1 + r0;
    x[2] = r1 - r0;
    let r0 = x[5] + x[1];
    let r1 = x[7] + x[3];
    x[3] = r2 + r3;
    x[1] = r2 - r3;
    x[7] = r1 + r0;
    x[5] = r1 - r0;
}
fn butterfly16(x: &mut [f32]) {
    let p = std::f32::consts::FRAC_1_SQRT_2;
    let r0 = x[1] - x[9];
    let r1 = x[0] - x[8];
    x[8] += x[0];
    x[9] += x[1];
    x[0] = (r0 + r1) * p;
    x[1] = (r0 - r1) * p;
    let r0 = x[3] - x[11];
    let r1 = x[10] - x[2];
    x[10] += x[2];
    x[11] += x[3];
    x[2] = r0;
    x[3] = r1;
    let r0 = x[12] - x[4];
    let r1 = x[13] - x[5];
    x[12] += x[4];
    x[13] += x[5];
    x[4] = (r0 - r1) * p;
    x[5] = (r0 + r1) * p;
    let r0 = x[14] - x[6];
    let r1 = x[15] - x[7];
    x[14] += x[6];
    x[15] += x[7];
    x[6] = r0;
    x[7] = r1;
    butterfly8(x);
    butterfly8(&mut x[8..]);
}
fn butterfly32(x: &mut [f32]) {
    let p1 = f32::from_bits(0x3f6c835e);
    let p2 = std::f32::consts::FRAC_1_SQRT_2;
    let p3 = f32::from_bits(0x3ec3ef15);
    let r0 = x[30] - x[14];
    let r1 = x[31] - x[15];
    x[30] += x[14];
    x[31] += x[15];
    x[14] = r0;
    x[15] = r1;
    let r0 = x[28] - x[12];
    let r1 = x[29] - x[13];
    x[28] += x[12];
    x[29] += x[13];
    x[12] = r0 * p1 - r1 * p3;
    x[13] = r0 * p3 + r1 * p1;
    let r0 = x[26] - x[10];
    let r1 = x[27] - x[11];
    x[26] += x[10];
    x[27] += x[11];
    x[10] = (r0 - r1) * p2;
    x[11] = (r0 + r1) * p2;
    let r0 = x[24] - x[8];
    let r1 = x[25] - x[9];
    x[24] += x[8];
    x[25] += x[9];
    x[8] = r0 * p3 - r1 * p1;
    x[9] = r1 * p3 + r0 * p1;
    let r0 = x[22] - x[6];
    let r1 = x[7] - x[23];
    x[22] += x[6];
    x[23] += x[7];
    x[6] = r1;
    x[7] = r0;
    let r0 = x[4] - x[20];
    let r1 = x[5] - x[21];
    x[20] += x[4];
    x[21] += x[5];
    x[4] = r1 * p1 + r0 * p3;
    x[5] = r1 * p3 - r0 * p1;
    let r0 = x[2] - x[18];
    let r1 = x[3] - x[19];
    x[18] += x[2];
    x[19] += x[3];
    x[2] = (r1 + r0) * p2;
    x[3] = (r1 - r0) * p2;
    let r0 = x[0] - x[16];
    let r1 = x[1] - x[17];
    x[16] += x[0];
    x[17] += x[1];
    x[0] = r1 * p3 + r0 * p1;
    x[1] = r1 * p1 - r0 * p3;
    butterfly16(x);
    butterfly16(&mut x[16..]);
}
fn butterfly_first(t: &[f32], x: &mut [f32]) {
    let mut x1 = x.len() - 8;
    let mut x2 = x.len() / 2 - 8;
    let mut ti = 0;
    loop {
        for j in [6, 4, 2, 0] {
            let r0 = x[x1 + j] - x[x2 + j];
            let r1 = x[x1 + j + 1] - x[x2 + j + 1];
            x[x1 + j] += x[x2 + j];
            x[x1 + j + 1] += x[x2 + j + 1];
            x[x2 + j] = r1 * t[ti + 1] + r0 * t[ti];
            x[x2 + j + 1] = r1 * t[ti] - r0 * t[ti + 1];
            ti += 4;
        }
        if x2 == 0 {
            break;
        }
        x1 -= 8;
        x2 -= 8;
    }
}

fn butterfly_generic(t: &[f32], x: &mut [f32], step: usize) {
    let mut x1 = x.len() - 8;
    let mut x2 = x.len() / 2 - 8;
    let mut ti = 0;
    loop {
        for j in [6, 4, 2, 0] {
            let r0 = x[x1 + j] - x[x2 + j];
            let r1 = x[x1 + j + 1] - x[x2 + j + 1];
            x[x1 + j] += x[x2 + j];
            x[x1 + j + 1] += x[x2 + j + 1];
            x[x2 + j] = r1 * t[ti + 1] + r0 * t[ti];
            x[x2 + j + 1] = r1 * t[ti] - r0 * t[ti + 1];
            ti += step;
        }
        if x2 == 0 {
            break;
        }
        x1 -= 8;
        x2 -= 8;
    }
}
pub(crate) fn transform(input: &[f32], a: &[f32], b: &[f32], c: &[f32]) -> Vec<f32> {
    let n = input.len() * 2;
    assert!(n.is_power_of_two() && (64..=8192).contains(&n));
    let n2 = n / 2;
    let n4 = n / 4;
    let mut out = vec![0f32; n];
    let mut ix = n2 - 7;
    let mut ox = n2 + n4;
    let mut t = n4;
    loop {
        ox -= 4;
        out[ox] = -input[ix + 2] * a[t + 3] - input[ix] * a[t + 2];
        out[ox + 1] = input[ix] * a[t + 3] - input[ix + 2] * a[t + 2];
        out[ox + 2] = -input[ix + 6] * a[t + 1] - input[ix + 4] * a[t];
        out[ox + 3] = input[ix + 4] * a[t + 1] - input[ix + 6] * a[t];
        t += 4;
        if ix < 8 {
            break;
        }
        ix -= 8;
    }
    ix = n2 - 8;
    ox = n2 + n4;
    t = n4;
    loop {
        t -= 4;
        out[ox] = input[ix + 4] * a[t + 3] + input[ix + 6] * a[t + 2];
        out[ox + 1] = input[ix + 4] * a[t + 2] - input[ix + 6] * a[t + 3];
        out[ox + 2] = input[ix] * a[t + 1] + input[ix + 2] * a[t];
        out[ox + 3] = input[ix] * a[t] - input[ix + 2] * a[t + 1];
        ox += 4;
        if ix == 0 {
            break;
        }
        ix -= 8;
    }
    if n >= 128 {
        butterfly_first(a, &mut out[n2..]);
    }
    let mut stage = 1usize;
    while stage < n.trailing_zeros() as usize - 6 {
        let points = n2 >> stage;
        for part in out[n2..].chunks_exact_mut(points) {
            butterfly_generic(a, part, 4 << stage);
        }
        stage += 1;
    }
    for block in out[n2..].as_chunks_mut::<32>().0 {
        butterfly32(block);
    }
    let log = n.trailing_zeros();
    let mask = (1usize << (log - 1)) - 1;
    let msb = 1usize << (log - 2);
    let mut bits = Vec::with_capacity(n4);
    for i in 0..n / 8 {
        let mut acc = 0;
        let mut j = 0;
        while msb >> j != 0 {
            if msb >> j & i != 0 {
                acc |= 1 << j;
            }
            j += 1;
        }
        bits.extend([(!acc & mask) - 1, acc]);
    }
    let mut w0 = 0;
    let mut w1 = n2;
    let mut ti = 0;
    let mut bi = 0;
    loop {
        w1 -= 4;
        for pair in 0..2 {
            let x0 = n2 + bits[bi + pair * 2];
            let x1 = n2 + bits[bi + pair * 2 + 1];
            let r0 = out[x0 + 1] - out[x1 + 1];
            let r1 = out[x0] + out[x1];
            let tc0 = c[ti + pair * 2] * 0.5;
            let tc1 = c[ti + pair * 2 + 1] * 0.5;
            let r2 = r1 * tc0 + r0 * tc1;
            let r3 = r1 * tc1 - r0 * tc0;
            let r0 = (out[x0 + 1] + out[x1 + 1]) * 0.5;
            let r1 = (out[x0] - out[x1]) * 0.5;
            out[w0 + pair * 2] = r0 + r2;
            out[w1 + 2 - pair * 2] = r0 - r2;
            out[w0 + pair * 2 + 1] = r1 + r3;
            out[w1 + 3 - pair * 2] = r3 - r1;
        }
        ti += 4;
        bi += 4;
        w0 += 4;
        if w0 >= w1 {
            break;
        }
    }
    let mut ox1 = n2 + n4;
    let mut ox2 = ox1;
    ix = 0;
    ti = 0;
    loop {
        ox1 -= 4;
        for j in 0..4 {
            let t0 = b[ti + j * 2] * 2.;
            let t1 = b[ti + j * 2 + 1] * 2.;
            out[ox1 + 3 - j] = out[ix + j * 2] * t1 - out[ix + j * 2 + 1] * t0;
            out[ox2 + j] = -(out[ix + j * 2] * t0 + out[ix + j * 2 + 1] * t1);
        }
        ox2 += 4;
        ix += 8;
        ti += 8;
        if ix >= ox1 {
            break;
        }
    }
    ix = n2 + n4;
    ox1 = n4;
    ox2 = n4;
    loop {
        ox1 -= 4;
        ix -= 4;
        for j in 0..4 {
            let v = out[ix + 3 - j];
            out[ox1 + 3 - j] = v;
            out[ox2 + j] = -v;
        }
        ox2 += 4;
        if ox2 >= ix {
            break;
        }
    }
    ix = n2 + n4;
    ox1 = ix;
    loop {
        ox1 -= 4;
        for j in 0..4 {
            out[ox1 + j] = out[ix + 3 - j];
        }
        ix += 4;
        if ox1 <= n2 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use header_cached::CachedBlocksizeDerived;

    fn input(n: usize, k: usize) -> Vec<f32> {
        let mut state = (k as u32).wrapping_mul(2_654_435_761).wrapping_add(12_345);
        (0..n / 2)
            .map(|j| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if k == 0 {
                    if j == 3 {
                        1.0
                    } else {
                        0.0
                    }
                } else if k == 1 {
                    (j as i32 * 13 % 41 - 20) as f32 * 0.01
                } else if k == 2 {
                    ((j as f64 * 0.13).sin() * 0.03) as f32
                } else if k < 3 + n / 2 {
                    if j == k - 3 {
                        1.0
                    } else {
                        0.0
                    }
                } else if k == 3 + n / 2 {
                    if j & 1 != 0 {
                        -0.0
                    } else {
                        0.0
                    }
                } else if k == 4 + n / 2 {
                    if j & 1 != 0 {
                        -1.0
                    } else {
                        1.0
                    }
                } else {
                    f32::from_bits((state & 0x807f_ffff) | ((113 + (k % 20) as u32) << 23))
                }
            })
            .collect()
    }

    #[test]
    fn enabled_blocks_match_independent_native_shaped_reference() {
        // Full output hashes from a separate compiled Xiph mdct.c reference
        // with these same cached coefficients, finite input and separate f32 ops.
        // Cover every spectral basis plus dense, sine, signed-zero,
        // alternating-sign and 33 deterministic finite-magnitude vectors.
        for (bs, hash) in [
            (6, 0xc053_8bb6_e28a_0a79u64),
            (7, 0x63cd_2866_ab90_fc45),
            (8, 0x74b0_4e84_c11b_70cd),
            (9, 0x3916_0d94_68e3_ece9),
            (10, 0xb6ff_6449_e04f_6d91),
            (11, 0x5fda_c981_3d0d_adb5),
            (12, 0x876a_b278_75df_1fb5),
            (13, 0x5ed8_e350_f9b2_4541),
        ] {
            let n = 1usize << bs;
            let mut cached = CachedBlocksizeDerived::from_blocksize(bs);
            cached.native_mdct = true;
            let mut actual = 0xcbf2_9ce4_8422_2325u64;
            for k in 0..38 + n / 2 {
                let mut buffer = input(n, k);
                buffer.resize(n, 0.0);
                ::imdct::inverse_mdct(&cached, &mut buffer, bs);
                for byte in buffer.iter().flat_map(|value| value.to_le_bytes()) {
                    actual = (actual ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                }
            }
            assert_eq!(actual, hash, "block {}", n);
        }
    }
}
