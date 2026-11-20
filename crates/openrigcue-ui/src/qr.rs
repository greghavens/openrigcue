//! A QR symbol of a URL (error correction H, byte mode, the smallest version holding the data, the mask with the
//! lowest penalty) and its PNG as base64 text (four modules of quiet zone), for the mobile companion window.

/// Level H blocks per version: ec codewords per block, then (blocks, data codewords) groups.
const H_BLOCKS: [(usize, &[(usize, usize)]); 10] = [
    (17, &[(1, 9)]),
    (28, &[(1, 16)]),
    (22, &[(2, 13)]),
    (16, &[(4, 9)]),
    (22, &[(2, 11), (2, 12)]),
    (28, &[(4, 15)]),
    (26, &[(4, 13), (1, 14)]),
    (26, &[(4, 14), (2, 15)]),
    (24, &[(4, 12), (4, 13)]),
    (28, &[(6, 15), (2, 16)]),
];
const ALIGNMENT: [&[usize]; 10] =
    [&[], &[6, 18], &[6, 22], &[6, 26], &[6, 30], &[6, 34], &[6, 22, 38], &[6, 24, 42], &[6, 26, 46], &[6, 28, 50]];
const LEVEL_H: u32 = 0b10;

#[allow(clippy::needless_range_loop)]
fn gf_tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u32 = 1;
    for i in 0..255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
    }
    for i in 255..512 {
        exp[i] = exp[i - 255];
    }
    (exp, log)
}

#[allow(clippy::needless_range_loop)]
fn ec_codewords(data: &[u8], count: usize) -> Vec<u8> {
    let (exp, log) = gf_tables();
    let mul =
        |a: u8, b: u8| if a == 0 || b == 0 { 0 } else { exp[log[a as usize] as usize + log[b as usize] as usize] };
    let mut generator = vec![1u8];
    for i in 0..count {
        let mut next = vec![0u8; generator.len() + 1];
        for (j, g) in generator.iter().enumerate() {
            next[j] ^= *g;
            next[j + 1] ^= mul(*g, exp[i]);
        }
        generator = next;
    }
    let mut rest: Vec<u8> = data.iter().copied().chain(std::iter::repeat_n(0, count)).collect();
    for i in 0..data.len() {
        let factor = rest[i];
        if factor == 0 {
            continue;
        }
        for (j, g) in generator.iter().enumerate() {
            rest[i + j] ^= mul(*g, factor);
        }
    }
    rest[data.len()..].to_vec()
}

fn capacity(version: usize) -> usize {
    H_BLOCKS[version - 1].1.iter().map(|(blocks, size)| blocks * size).sum()
}

/// ISO-8859-1 bytes of the text (the default), else UTF-8.
fn bytes_of(text: &str) -> Vec<u8> {
    if text.chars().all(|c| (c as u32) < 256) {
        text.chars().map(|c| c as u32 as u8).collect()
    } else {
        text.as_bytes().to_vec()
    }
}

fn codewords(bytes: &[u8], version: usize) -> Vec<u8> {
    let mut bits: Vec<u8> = Vec::new();
    let put = |bits: &mut Vec<u8>, value: usize, length: usize| {
        for i in (0..length).rev() {
            bits.push(((value >> i) & 1) as u8);
        }
    };
    put(&mut bits, 0b0100, 4);
    put(&mut bits, bytes.len(), if version < 10 { 8 } else { 16 });
    for b in bytes {
        put(&mut bits, usize::from(*b), 8);
    }
    let total = capacity(version) * 8;
    let terminator = 4.min(total.saturating_sub(bits.len()));
    put(&mut bits, 0, terminator);
    while !bits.len().is_multiple_of(8) {
        bits.push(0);
    }
    let mut data: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |v, b| (v << 1) | b)).collect();
    let mut pad = 0;
    while data.len() < capacity(version) {
        data.push(if pad % 2 == 1 { 0x11 } else { 0xec });
        pad += 1;
    }
    let (ec_count, groups) = H_BLOCKS[version - 1];
    let mut blocks: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut offset = 0;
    for (count, size) in groups {
        for _ in 0..*count {
            let block = data[offset..offset + size].to_vec();
            offset += size;
            let ec = ec_codewords(&block, ec_count);
            blocks.push((block, ec));
        }
    }
    let longest = blocks.iter().map(|b| b.0.len()).max().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..longest {
        for b in &blocks {
            if i < b.0.len() {
                out.push(b.0[i]);
            }
        }
    }
    for i in 0..ec_count {
        for b in &blocks {
            out.push(b.1[i]);
        }
    }
    out
}

fn bch(value: u32, poly: u32, poly_bits: u32) -> u32 {
    let mut v = value << (poly_bits - 1);
    for i in (poly_bits - 1..=31).rev() {
        if (v >> i) & 1 == 1 {
            v ^= poly << (i + 1 - poly_bits);
        }
    }
    v
}

fn mask_bit(mask: u32, r: usize, c: usize) -> bool {
    match mask {
        0 => (r + c).is_multiple_of(2),
        1 => r.is_multiple_of(2),
        2 => c.is_multiple_of(3),
        3 => (r + c).is_multiple_of(3),
        4 => (r / 2 + c / 3).is_multiple_of(2),
        5 => (r * c) % 2 + (r * c) % 3 == 0,
        6 => ((r * c) % 2 + (r * c) % 3).is_multiple_of(2),
        _ => ((r + c) % 2 + (r * c) % 3).is_multiple_of(2),
    }
}

type Grid = Vec<Vec<bool>>;

fn base_matrix(version: usize) -> (usize, Grid, Grid) {
    let n = version * 4 + 17;
    let mut dark = vec![vec![false; n]; n];
    let mut fixed = vec![vec![false; n]; n];
    let mut set = |dark: &mut Grid, r: usize, c: usize, value: bool| {
        dark[r][c] = value;
        fixed[r][c] = true;
    };
    for (r0, c0) in [(0i64, 0i64), (0, n as i64 - 7), (n as i64 - 7, 0)] {
        for r in -1i64..=7 {
            for c in -1i64..=7 {
                let (rr, cc) = (r0 + r, c0 + c);
                if rr < 0 || cc < 0 || rr >= n as i64 || cc >= n as i64 {
                    continue;
                }
                let ring = (r - 3).abs().max((c - 3).abs());
                set(&mut dark, rr as usize, cc as usize, (0..=6).contains(&r) && (0..=6).contains(&c) && ring != 2);
            }
        }
    }
    for i in 8..n - 8 {
        set(&mut dark, 6, i, i % 2 == 0);
        set(&mut dark, i, 6, i % 2 == 0);
    }
    let centres = ALIGNMENT[version - 1];
    let last = centres.last().copied().unwrap_or(0);
    for &r in centres {
        for &c in centres {
            if (r == 6 && (c == 6 || c == last)) || (r == last && c == 6) {
                continue;
            }
            for dr in -2i64..=2 {
                for dc in -2i64..=2 {
                    set(&mut dark, (r as i64 + dr) as usize, (c as i64 + dc) as usize, dr.abs().max(dc.abs()) != 1);
                }
            }
        }
    }

    let set = |dark: &mut Grid, fixed: &mut Grid, r: usize, c: usize, value: bool| {
        dark[r][c] = value;
        fixed[r][c] = true;
    };
    for i in 0..9 {
        if !fixed[8][i] {
            set(&mut dark, &mut fixed, 8, i, false);
        }
        if !fixed[i][8] {
            set(&mut dark, &mut fixed, i, 8, false);
        }
    }
    for i in 0..8 {
        set(&mut dark, &mut fixed, 8, n - 1 - i, false);
        set(&mut dark, &mut fixed, n - 1 - i, 8, false);
    }
    set(&mut dark, &mut fixed, n - 8, 8, true);
    if version >= 7 {
        let bits = ((version as u32) << 12) | bch(version as u32, 0x1f25, 13);
        for i in 0..18 {
            let on = (bits >> i) & 1 == 1;
            set(&mut dark, &mut fixed, i / 3, n - 11 + i % 3, on);
            set(&mut dark, &mut fixed, n - 11 + i % 3, i / 3, on);
        }
    }
    (n, dark, fixed)
}

fn place_data(n: usize, base: &Grid, fixed: &Grid, words: &[u8]) -> Grid {
    let mut dark = base.clone();
    let mut bit = 0usize;
    let total = words.len() * 8;
    let mut right = n as i64 - 1;
    while right >= 1 {
        if right == 6 {
            right = 5;
        }
        let upward = ((n as i64 - 1 - right) >> 1) % 2 == 0;
        for k in 0..n {
            let r = if upward { n - 1 - k } else { k };
            for c in [right as usize, right as usize - 1] {
                if fixed[r][c] {
                    continue;
                }
                dark[r][c] = bit < total && (words[bit >> 3] >> (7 - (bit & 7))) & 1 == 1;
                bit += 1;
            }
        }
        right -= 2;
    }
    dark
}

fn apply_mask(n: usize, fixed: &Grid, data: &Grid, mask: u32) -> Grid {
    let mut dark: Grid = (0..n)
        .map(|r| (0..n).map(|c| if fixed[r][c] { data[r][c] } else { data[r][c] != mask_bit(mask, r, c) }).collect())
        .collect();
    let format = (LEVEL_H << 3) | mask;
    let bits = ((format << 10) | bch(format, 0x537, 11)) ^ 0x5412;
    for i in 0..15 {
        let on = (bits >> i) & 1 == 1;
        if i < 6 {
            dark[i][8] = on;
        } else if i < 8 {
            dark[i + 1][8] = on;
        } else {
            dark[n - 15 + i][8] = on;
        }
        if i < 8 {
            dark[8][n - 1 - i] = on;
        } else if i < 9 {
            dark[8][7] = on;
        } else {
            dark[8][14 - i] = on;
        }
    }
    dark
}

/// The mask penalty, with a truncated-percent fourth rule.
#[allow(clippy::needless_range_loop)]
fn penalty(m: &Grid) -> usize {
    let n = m.len();
    let mut score = 0;
    for pass in 0..2 {
        for i in 0..n {
            let line = |j: usize| if pass == 0 { m[i][j] } else { m[j][i] };
            let mut run = 1;
            for j in 1..n {
                if line(j) == line(j - 1) {
                    run += 1;
                } else {
                    if run >= 5 {
                        score += run - 2;
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                score += run - 2;
            }
            for j in 0..n.saturating_sub(10) {
                let s: String = (0..11).map(|k| if line(j + k) { '1' } else { '0' }).collect();
                if s == "10111010000" || s == "00001011101" {
                    score += 40;
                }
            }
        }
    }
    for r in 0..n - 1 {
        for c in 0..n - 1 {
            let v = m[r][c];
            if m[r][c + 1] == v && m[r + 1][c] == v && m[r + 1][c + 1] == v {
                score += 3;
            }
        }
    }
    let dark = m.iter().flatten().filter(|v| **v).count();
    let ratio = dark as f64 / (n * n) as f64;
    let percent = ratio * 100.0 - 50.0;
    // The truncated percent's distance, divided by 5, times 10, truncated; these are not standard ten-point steps.
    score += ((percent.trunc().abs() / 5.0) * 10.0) as usize;
    score
}

/// The symbol of `text` under one mask pattern.
#[cfg(test)]
fn matrix_with_mask(text: &str, mask: u32) -> Option<Grid> {
    let bytes = bytes_of(text);
    let version = (1..=10).find(|&v| capacity(v) >= bytes.len() + 2 + usize::from(v >= 10))?;
    let (n, base, fixed) = base_matrix(version);
    let data = place_data(n, &base, &fixed, &codewords(&bytes, version));
    Some(apply_mask(n, &fixed, &data, mask))
}

/// The symbol of `text` (true = dark), without quiet zone; `None` when the text does not fit version 10.
pub fn matrix(text: &str) -> Option<Grid> {
    let bytes = bytes_of(text);
    let version = (1..=10).find(|&v| capacity(v) >= bytes.len() + 2 + usize::from(v >= 10))?;
    let (n, base, fixed) = base_matrix(version);
    let data = place_data(n, &base, &fixed, &codewords(&bytes, version));
    let mut best = None;
    let mut best_score = usize::MAX;
    for mask in 0..8 {
        let candidate = apply_mask(n, &fixed, &data, mask);
        let score = penalty(&candidate);
        if score < best_score {
            best_score = score;
            best = Some(candidate);
        }
    }
    best
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for b in bytes {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// The symbol as base64 PNG text: a 1-bit grey PNG, `scale` pixels per module, four modules of quiet zone.
pub fn png(grid: &Grid, scale: usize) -> Vec<u8> {
    let quiet = 4;
    let size = (grid.len() + 2 * quiet) * scale;
    let row_bytes = size.div_ceil(8);
    let mut raw = Vec::with_capacity((row_bytes + 1) * size);
    for y in 0..size {
        raw.push(0);
        let mut row = vec![0u8; row_bytes];
        for x in 0..size {
            let (mr, mc) = ((y / scale).wrapping_sub(quiet), (x / scale).wrapping_sub(quiet));
            let dark = mr < grid.len() && mc < grid.len() && grid[mr][mc];
            if !dark {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        raw.extend_from_slice(&row);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &raw).expect("in memory");
    let compressed = encoder.finish().expect("in memory");
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&(size as u32).to_be_bytes());
    header.extend_from_slice(&(size as u32).to_be_bytes());
    header.extend_from_slice(&[1, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &compressed);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The `data:image/png;base64,` URL of `text` at scale 8 (empty when it does not fit).
pub fn png_data_url(text: &str) -> String {
    match matrix(text) {
        Some(grid) => format!("data:image/png;base64,{}", base64(&png(&grid, 8))),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Symbols made by libqrencode 4.1.1 (`tests/qr-reference.json`).
    #[test]
    fn matches_reference_symbols() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../tests/qr-reference.json")).expect("reference");
        let cases = reference.as_array().expect("cases");
        assert!(!cases.is_empty());
        for case in cases {
            let text = case["text"].as_str().expect("text");
            let rows: Vec<String> =
                case["rows"].as_array().expect("rows").iter().map(|r| r.as_str().unwrap().to_owned()).collect();
            let rows_of = |grid: Grid| -> Vec<String> {
                grid.iter().map(|r| r.iter().map(|d| if *d { '1' } else { '0' }).collect()).collect()
            };
            let matching = (0..8).filter(|&mask| rows_of(matrix_with_mask(text, mask).expect("fits")) == rows).count();
            assert_eq!(matching, 1, "{text}");
            assert_eq!(matrix(text).expect("fits").len(), rows.len());
        }
    }

    #[test]
    fn png_is_well_formed() {
        let url = png_data_url("http://192.168.1.20:8090");
        assert!(url.starts_with("data:image/png;base64,iVBORw0KGgo"));
    }
}
