//! The Deflate encoder of 7-Zip at its default ("normal", `-mx=5`) level.
//!
//! The FS19 and FS22 mod zips use 7-Zip's container conventions ("version
//! made by" 63 and an NTFS time extra field on the central entries), and
//! this encoder gives every entry (`icon.png`, `modDesc.xml`,
//! `TelemetryMod.lua`) the raw deflate stream that `7z a -tzip -mx=5` writes
//! for the file (checked with 7-Zip 26.02).
//!
//! This is a port of that encoder as it is in 7-Zip 21.07 (December 2021):
//! `CPP/7zip/Compress/DeflateEncoder.cpp`, the `Bt3Zip` binary-tree match
//! finder of `C/LzFind.c`, `Huffman_Generate` of `C/HuffEnc.c` and the LSB
//! bit writer of `CPP/7zip/Compress/BitlEncoder.h`.  Level 5 normalises to
//! the optimal parser (`algo` 1) with 32 fast bytes, binary-tree mode, 32
//! match-finder cycles and one pass, which leaves out multi-pass block
//! division and the static-table trial (`m_CheckStatic` is false); blocks
//! are dynamic Huffman or stored, whichever is cheaper.

const MATCH_MIN_LEN: u32 = 3;
const MATCH_MAX_LEN: u32 = 258;
const NUM_LEN_SYMBOLS: usize = 256;
const NUM_LEN_SLOTS: usize = 29;
const FIXED_MAIN_TABLE_SIZE: usize = 257 + 31; // kSymbolMatch + kFixedLenTableSize
const MAIN_TABLE_SIZE: usize = 257 + NUM_LEN_SLOTS;
const DIST_TABLE_SIZE64: usize = 32;
const FIXED_DIST_TABLE_SIZE: usize = 32;
const LEVEL_TABLE_SIZE: usize = 19;
const SYMBOL_END_OF_BLOCK: usize = 256;
const SYMBOL_MATCH: usize = 257;
const TABLE_DIRECT_LEVELS: usize = 16;
const TABLE_LEVEL_REP_NUMBER: usize = 16;
const TABLE_LEVEL0_NUMBER: usize = 17;
const TABLE_LEVEL0_NUMBER2: usize = 18;
const NUM_LIT_LEN_CODES_MIN: usize = 257;
const NUM_DIST_CODES_MIN: usize = 1;
const NUM_LEVEL_CODES_MIN: usize = 4;
const MAX_LEVEL_BIT_LENGTH: u32 = 7;
const NO_LITERAL_STAT_PRICE: u8 = 11;
const NO_LEN_STAT_PRICE: u8 = 11;
const NO_POS_STAT_PRICE: u8 = 6;

const NUM_OPTS_BASE: u32 = 1 << 12;
const NUM_OPTS: usize = NUM_OPTS_BASE as usize + MATCH_MAX_LEN as usize;
const MAX_UNCOMPRESSED_BLOCK_SIZE: u32 = (1 << 16) - 1;
const BLOCK_UNCOMPRESSED_SIZE_THRESHOLD: u32 = MAX_UNCOMPRESSED_BLOCK_SIZE - MATCH_MAX_LEN - NUM_OPTS as u32;
const INFINITY_PRICE: u32 = 0x0FFF_FFFF;
const HISTORY_SIZE: u32 = 1 << 15;
const CODE_VALUE_LITERAL: u16 = 1 << 15;

/// Level-5 properties after `CEncProps::Normalize`.
const NUM_FAST_BYTES: u32 = 32;
const MATCH_FINDER_CYCLES: u32 = 16 + (NUM_FAST_BYTES >> 1);
const NUM_DIV_PASSES: u32 = 1;
const VALUE_BLOCK_SIZE: u32 = (7 << 10) + (1 << 12) * NUM_DIV_PASSES;

const LEN_START32: [u8; 31] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 255, 0,
    0,
];
const LEN_DIRECT_BITS32: [u8; 31] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0, 0, 0];
const DIST_START: [u32; 32] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 3072, 4096, 6144,
    8192, 12288, 16384, 24576, 32768, 49152,
];
const DIST_DIRECT_BITS: [u8; 32] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14];
const LEVEL_DIRECT_BITS: [u8; 3] = [2, 3, 7];
const CODE_LENGTH_ALPHABET_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

struct Tables {
    len_slots: [u8; NUM_LEN_SYMBOLS],
    fast_pos: [u8; 1 << 9],
    crc: [u32; 256],
}

/// `CFastPosInit` and the match finder's CRC table.
fn tables() -> Tables {
    let mut len_slots = [0u8; NUM_LEN_SYMBOLS];
    for i in 0..NUM_LEN_SLOTS {
        let start = LEN_START32[i] as usize;
        for slot in &mut len_slots[start..start + (1usize << LEN_DIRECT_BITS32[i])] {
            *slot = i as u8;
        }
    }
    let mut fast_pos = [0u8; 1 << 9];
    let mut c = 0usize;
    for slot in 0..18u8 {
        for _ in 0..(1usize << DIST_DIRECT_BITS[slot as usize]) {
            fast_pos[c] = slot;
            c += 1;
        }
    }
    let mut crc = [0u32; 256];
    for (i, e) in crc.iter_mut().enumerate() {
        let mut r = i as u32;
        for _ in 0..8 {
            r = (r >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(r & 1));
        }
        *e = r;
    }
    Tables { len_slots, fast_pos, crc }
}

/// `GetPosSlot` (distance - 1 to distance slot).
fn pos_slot(t: &Tables, pos: u32) -> usize {
    let zz = (9 - 1) & ((((1u32 << 9) - 1).wrapping_sub(pos)) >> (31 - 3));
    t.fast_pos[(pos >> zz) as usize] as usize + (zz as usize) * 2
}

/// `Bt3Zip` match finder of `LzFind.c` over the whole input.  Positions
/// start at 1 (`kEmptyHashValue` is 0), so byte `i` of the input is
/// position `i + 1` and the first byte can be matched.
struct MatchFinder<'a> {
    data: &'a [u8],
    pos: u32,
    cyclic_pos: u32,
    cyclic_size: u32,
    hash: Vec<u32>,
    son: Vec<u32>,
}

impl<'a> MatchFinder<'a> {
    fn new(data: &'a [u8]) -> Self {
        let cyclic_size = HISTORY_SIZE + 1;
        MatchFinder {
            data,
            pos: 1,
            cyclic_pos: 1,
            cyclic_size,
            hash: vec![0; 1 << 16],
            son: vec![0; cyclic_size as usize * 2],
        }
    }

    /// Index into `data` of the current position.
    fn cur(&self) -> usize {
        self.pos as usize - 1
    }

    fn avail(&self) -> u32 {
        (self.data.len() - self.cur()) as u32
    }

    fn len_limit(&self) -> u32 {
        self.avail().min(NUM_FAST_BYTES)
    }

    fn move_pos(&mut self) {
        self.cyclic_pos += 1;
        self.pos += 1;
        if self.cyclic_pos == self.cyclic_size {
            self.cyclic_pos = 0;
        }
    }

    fn hash_zip(&self, cur: usize, crc: &[u32; 256]) -> usize {
        let d = self.data;
        (((d[cur + 2] as u32) | ((d[cur] as u32) << 8)) ^ crc[d[cur + 1] as usize]) as usize & 0xFFFF
    }

    /// `Bt3Zip_MatchFinder_GetMatches`: (length, distance - 1) pairs of
    /// increasing length, appended to `out`.
    fn get_matches(&mut self, crc: &[u32; 256], out: &mut Vec<u32>) {
        let len_limit = self.len_limit();
        if len_limit < 3 {
            self.move_pos();
            return;
        }
        let cur = self.cur();
        let hv = self.hash_zip(cur, crc);
        let cur_match = self.hash[hv];
        self.hash[hv] = self.pos;
        self.tree(len_limit, cur_match, Some((out, 2)));
        self.move_pos();
    }

    /// `Bt3Zip_MatchFinder_Skip`.
    fn skip(&mut self, crc: &[u32; 256], mut num: u32) {
        while num > 0 {
            let len_limit = self.len_limit();
            if len_limit < 3 {
                self.move_pos();
            } else {
                let cur = self.cur();
                let hv = self.hash_zip(cur, crc);
                let cur_match = self.hash[hv];
                self.hash[hv] = self.pos;
                self.tree(len_limit, cur_match, None);
                self.move_pos();
            }
            num -= 1;
        }
    }

    /// `GetMatchesSpec1` (with `found`) / `SkipMatchesSpec` (without).
    fn tree(&mut self, len_limit: u32, mut cur_match: u32, mut found: Option<(&mut Vec<u32>, u32)>) {
        let pos = self.pos;
        let cyc = self.cyclic_pos;
        let size = self.cyclic_size;
        let cur = self.cur();
        let d = self.data;
        let mut ptr0 = (cyc as usize) * 2 + 1;
        let mut ptr1 = (cyc as usize) * 2;
        let (mut len0, mut len1) = (0u32, 0u32);
        let cm_check = pos.saturating_sub(size);
        let mut cut = MATCH_FINDER_CYCLES;
        if cm_check < cur_match {
            loop {
                let delta = pos - cur_match;
                let pair = ((cyc.wrapping_sub(delta).wrapping_add(if delta > cyc { size } else { 0 })) as usize) * 2;
                let pb = cur - delta as usize;
                let mut len = len0.min(len1);
                if d[pb + len as usize] == d[cur + len as usize] {
                    match found.as_mut() {
                        Some((out, max_len)) => {
                            let pair0 = self.son[pair];
                            len += 1;
                            if len != len_limit && d[pb + len as usize] == d[cur + len as usize] {
                                len += 1;
                                while len != len_limit {
                                    if d[pb + len as usize] != d[cur + len as usize] {
                                        break;
                                    }
                                    len += 1;
                                }
                            }
                            if *max_len < len {
                                *max_len = len;
                                out.push(len);
                                out.push(delta - 1);
                                if len == len_limit {
                                    self.son[ptr1] = pair0;
                                    self.son[ptr0] = self.son[pair + 1];
                                    return;
                                }
                            }
                        }
                        None => {
                            len += 1;
                            while len != len_limit {
                                if d[pb + len as usize] != d[cur + len as usize] {
                                    break;
                                }
                                len += 1;
                            }
                            if len == len_limit {
                                self.son[ptr1] = self.son[pair];
                                self.son[ptr0] = self.son[pair + 1];
                                return;
                            }
                        }
                    }
                }
                if d[pb + len as usize] < d[cur + len as usize] {
                    self.son[ptr1] = cur_match;
                    cur_match = self.son[pair + 1];
                    ptr1 = pair + 1;
                    len1 = len;
                } else {
                    self.son[ptr0] = cur_match;
                    cur_match = self.son[pair];
                    ptr0 = pair;
                    len0 = len;
                }
                cut -= 1;
                if cut == 0 || cm_check >= cur_match {
                    break;
                }
            }
        }
        self.son[ptr0] = 0;
        self.son[ptr1] = 0;
    }
}

/// `CBitlEncoder`: least significant bit first.
struct BitWriter {
    out: Vec<u8>,
    bit_pos: u32,
    cur: u8,
}

impl BitWriter {
    fn write_bits(&mut self, mut value: u32, mut num_bits: u32) {
        while num_bits > 0 {
            if num_bits < self.bit_pos {
                self.cur |= ((value & ((1u32 << num_bits) - 1)) << (8 - self.bit_pos)) as u8;
                self.bit_pos -= num_bits;
                return;
            }
            num_bits -= self.bit_pos;
            self.out.push(self.cur | (value << (8 - self.bit_pos)) as u8);
            value >>= self.bit_pos;
            self.bit_pos = 8;
            self.cur = 0;
        }
    }

    fn flush_byte(&mut self) {
        if self.bit_pos < 8 {
            self.out.push(self.cur);
        }
        self.bit_pos = 8;
        self.cur = 0;
    }
}

/// `Huffman_Generate` of 7-Zip 21.07 `HuffEnc.c`: code lengths limited to
/// `max_len` into `lens`, canonical codes into `p`.
fn huffman_generate(freqs: &[u32], p: &mut [u32], lens: &mut [u8], max_len: u32) {
    const NUM_BITS: u32 = 10;
    const MASK: u32 = (1 << NUM_BITS) - 1;
    let num_symbols = freqs.len();
    // Counting sort into frequency buckets, the last bucket heap-sorted: the
    // result is the non-zero (freq << 10 | symbol) keys in ascending order.
    let mut num = 0usize;
    for (i, &f) in freqs.iter().enumerate() {
        if f == 0 {
            lens[i] = 0;
        } else {
            p[num] = i as u32 | (f << NUM_BITS);
            num += 1;
        }
    }
    p[..num].sort_unstable();

    if num < 2 {
        let min_code = 0usize;
        let mut max_code = 1usize;
        if num == 1 {
            max_code = (p[0] & MASK) as usize;
            if max_code == 0 {
                max_code += 1;
            }
        }
        p[min_code] = 0;
        p[max_code] = 1;
        lens[min_code] = 1;
        lens[max_code] = 1;
        return;
    }

    let num = num as u32;
    let (mut b, mut e, mut i) = (0u32, 0u32, 0u32);
    loop {
        let pick = |i: &mut u32, b: &mut u32, e: u32, p: &[u32]| -> u32 {
            if *i != num && (*b == e || (p[*i as usize] >> NUM_BITS) <= (p[*b as usize] >> NUM_BITS)) {
                *i += 1;
                *i - 1
            } else {
                *b += 1;
                *b - 1
            }
        };
        let n = pick(&mut i, &mut b, e, p) as usize;
        let mut freq = p[n] & !MASK;
        p[n] = (p[n] & MASK) | (e << NUM_BITS);
        let m = pick(&mut i, &mut b, e, p) as usize;
        freq = freq.wrapping_add(p[m] & !MASK);
        p[m] = (p[m] & MASK) | (e << NUM_BITS);
        p[e as usize] = (p[e as usize] & MASK) | freq;
        e += 1;
        if num - e <= 1 {
            break;
        }
    }

    let mut len_counters = [0u32; 17];
    e -= 1;
    p[e as usize] &= MASK;
    len_counters[1] = 2;
    while e > 0 {
        e -= 1;
        let mut len = (p[(p[e as usize] >> NUM_BITS) as usize] >> NUM_BITS) + 1;
        p[e as usize] = (p[e as usize] & MASK) | (len << NUM_BITS);
        if len >= max_len {
            len = max_len - 1;
            while len_counters[len as usize] == 0 {
                len -= 1;
            }
        }
        len_counters[len as usize] -= 1;
        len_counters[len as usize + 1] += 2;
    }

    let mut k = 0usize;
    let mut len = max_len;
    while len != 0 {
        for _ in 0..len_counters[len as usize] {
            lens[(p[k] & MASK) as usize] = len as u8;
            k += 1;
        }
        len -= 1;
    }

    let mut next_codes = [0u32; 17];
    let mut code = 0u32;
    for len in 1..=16usize {
        code = (code + len_counters[len - 1]) << 1;
        next_codes[len] = code;
    }
    for k in 0..num_symbols {
        let l = lens[k] as usize;
        p[k] = next_codes[l];
        next_codes[l] = next_codes[l].wrapping_add(1);
    }
}

/// `Huffman_ReverseBits` of 7-Zip 21.07.
fn reverse_bits(codes: &mut [u32], lens: &[u8]) {
    for (c, &l) in codes.iter_mut().zip(lens) {
        let mut x = *c;
        x = ((x & 0x5555) << 1) | ((x & 0xAAAA) >> 1);
        x = ((x & 0x3333) << 2) | ((x & 0xCCCC) >> 2);
        x = ((x & 0x0F0F) << 4) | ((x & 0xF0F0) >> 4);
        *c = (((x & 0x00FF) << 8) | ((x & 0xFF00) >> 8)) >> (16 - l as u32);
    }
}

fn huffman_price(freqs: &[u32], lens: &[u8]) -> u32 {
    freqs.iter().zip(lens).map(|(&f, &l)| l as u32 * f).sum()
}

/// `Huffman_GetPrice_Spec`.
fn huffman_price_spec(freqs: &[u32], lens: &[u8], extra_bits: &[u8], extra_base: usize) -> u32 {
    let n = freqs.len();
    huffman_price(freqs, &lens[..n]) + huffman_price(&freqs[extra_base..], &extra_bits[..n - extra_base])
}

#[derive(Clone, Copy)]
struct Levels {
    lit_len: [u8; FIXED_MAIN_TABLE_SIZE],
    dist: [u8; FIXED_DIST_TABLE_SIZE],
}

#[derive(Clone, Copy, Default)]
struct Optimal {
    price: u32,
    pos_prev: u16,
    back_prev: u16,
}

#[derive(Clone, Copy)]
struct CodeValue {
    len: u16,
    pos: u16,
}

/// `CCoder` with the level-5 configuration.
struct Encoder<'a> {
    t: Tables,
    mf: MatchFinder<'a>,
    bits: BitWriter,
    values: Vec<CodeValue>,
    match_distances: Vec<u16>,
    dist_tmp: Vec<u32>,
    optimum: Vec<Optimal>,
    optimum_end_index: u32,
    optimum_current_index: u32,
    additional_offset: u32,
    second_pass: bool,
    block_size_res: u32,
    literal_prices: [u8; 256],
    len_prices: [u8; NUM_LEN_SYMBOLS],
    pos_prices: [u8; DIST_TABLE_SIZE64],
    new_levels: Levels,
    main_freqs: [u32; FIXED_MAIN_TABLE_SIZE],
    dist_freqs: [u32; DIST_TABLE_SIZE64],
    main_codes: [u32; FIXED_MAIN_TABLE_SIZE],
    dist_codes: [u32; DIST_TABLE_SIZE64],
    level_codes: [u32; LEVEL_TABLE_SIZE],
    level_lens: [u8; LEVEL_TABLE_SIZE],
    level_levels: [u8; LEVEL_TABLE_SIZE],
    num_lit_len_levels: usize,
    num_dist_levels: usize,
    num_level_codes: usize,
    // m_Tables[1]
    table_levels: Levels,
    table_block_size_res: u32,
    table_store_mode: bool,
}

impl<'a> Encoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        // CTables::InitStructures
        let mut lit_len = [5u8; FIXED_MAIN_TABLE_SIZE];
        lit_len[..256].fill(8);
        lit_len[256] = 13;
        let table_levels = Levels { lit_len, dist: [5; FIXED_DIST_TABLE_SIZE] };
        Encoder {
            t: tables(),
            mf: MatchFinder::new(data),
            bits: BitWriter { out: Vec::with_capacity(data.len() / 2 + 64), bit_pos: 8, cur: 0 },
            values: Vec::with_capacity(MAX_UNCOMPRESSED_BLOCK_SIZE as usize),
            match_distances: Vec::with_capacity(MATCH_MAX_LEN as usize * 2 + 3),
            dist_tmp: Vec::with_capacity(MATCH_MAX_LEN as usize * 2 + 3),
            optimum: vec![Optimal::default(); NUM_OPTS],
            optimum_end_index: 0,
            optimum_current_index: 0,
            additional_offset: 0,
            second_pass: false,
            block_size_res: 0,
            literal_prices: [0; 256],
            len_prices: [0; NUM_LEN_SYMBOLS],
            pos_prices: [0; DIST_TABLE_SIZE64],
            new_levels: Levels { lit_len: [0; FIXED_MAIN_TABLE_SIZE], dist: [0; FIXED_DIST_TABLE_SIZE] },
            main_freqs: [0; FIXED_MAIN_TABLE_SIZE],
            dist_freqs: [0; DIST_TABLE_SIZE64],
            main_codes: [0; FIXED_MAIN_TABLE_SIZE],
            dist_codes: [0; DIST_TABLE_SIZE64],
            level_codes: [0; LEVEL_TABLE_SIZE],
            level_lens: [0; LEVEL_TABLE_SIZE],
            level_levels: [0; LEVEL_TABLE_SIZE],
            num_lit_len_levels: 0,
            num_dist_levels: 0,
            num_level_codes: 0,
            table_levels,
            table_block_size_res: 0,
            table_store_mode: false,
        }
    }

    /// Byte at the coding position plus `offset` (`GetPointerToCurrentPos() + offset - m_AdditionalOffset`).
    fn byte_at(&self, offset: u32) -> u8 {
        self.mf.data[self.mf.cur() + offset as usize - self.additional_offset as usize]
    }

    /// `GetMatches` (single pass): `match_distances` = [count, len, dist-1, ...].
    fn get_matches(&mut self) {
        self.dist_tmp.clear();
        let mut tmp = std::mem::take(&mut self.dist_tmp);
        self.mf.get_matches(&self.t.crc, &mut tmp);
        let num_pairs = tmp.len();
        self.match_distances.clear();
        self.match_distances.push(num_pairs as u16);
        for &v in &tmp {
            self.match_distances.push(v as u16);
        }
        if num_pairs != 0 {
            let mut len = tmp[num_pairs - 2];
            if len == NUM_FAST_BYTES && NUM_FAST_BYTES != MATCH_MAX_LEN {
                let mut num_avail = self.mf.avail() + 1;
                let pby = self.mf.cur() - 1;
                let pby2 = pby - (tmp[num_pairs - 1] as usize + 1);
                if num_avail > MATCH_MAX_LEN {
                    num_avail = MATCH_MAX_LEN;
                }
                let d = self.mf.data;
                while len < num_avail && d[pby + len as usize] == d[pby2 + len as usize] {
                    len += 1;
                }
                self.match_distances[num_pairs - 1] = len as u16;
            }
        }
        self.dist_tmp = tmp;
        if !self.second_pass {
            self.additional_offset += 1;
        }
    }

    fn move_pos(&mut self, num: u32) {
        if !self.second_pass && num > 0 {
            self.mf.skip(&self.t.crc, num);
            self.additional_offset += num;
        }
    }

    fn backward(&mut self, cur: u32) -> (u32, u32) {
        let mut cur = cur;
        self.optimum_end_index = cur;
        let mut pos_mem = self.optimum[cur as usize].pos_prev as u32;
        let mut back_mem = self.optimum[cur as usize].back_prev;
        loop {
            let pos_prev = pos_mem;
            let back_cur = back_mem;
            back_mem = self.optimum[pos_prev as usize].back_prev;
            pos_mem = self.optimum[pos_prev as usize].pos_prev as u32;
            self.optimum[pos_prev as usize].back_prev = back_cur;
            self.optimum[pos_prev as usize].pos_prev = cur as u16;
            cur = pos_prev;
            if cur == 0 {
                break;
            }
        }
        let back_res = self.optimum[0].back_prev as u32;
        self.optimum_current_index = self.optimum[0].pos_prev as u32;
        (self.optimum_current_index, back_res)
    }

    /// `GetOptimal`: (length, distance - 1) of the next token; length 1 is a literal.
    fn get_optimal(&mut self) -> (u32, u32) {
        if self.optimum_end_index != self.optimum_current_index {
            let o = self.optimum[self.optimum_current_index as usize];
            let len = o.pos_prev as u32 - self.optimum_current_index;
            self.optimum_current_index = o.pos_prev as u32;
            return (len, o.back_prev as u32);
        }
        self.optimum_current_index = 0;
        self.optimum_end_index = 0;

        self.get_matches();
        let mut len_end;
        {
            let num_pairs = self.match_distances[0] as usize;
            if num_pairs == 0 {
                return (1, 0);
            }
            let md = &self.match_distances[1..];
            len_end = md[num_pairs - 2] as u32;
            if len_end > NUM_FAST_BYTES {
                let back = md[num_pairs - 1] as u32;
                self.move_pos(len_end - 1);
                return (len_end, back);
            }
            self.optimum[1].price = self.literal_prices[self.byte_at(0) as usize] as u32;
            self.optimum[1].pos_prev = 0;
            self.optimum[2].price = INFINITY_PRICE;
            self.optimum[2].pos_prev = 1;
            let mut offs = 0usize;
            let md = self.match_distances[1..].to_vec();
            for i in MATCH_MIN_LEN..=len_end {
                let distance = md[offs + 1] as u32;
                let o = &mut self.optimum[i as usize];
                o.pos_prev = 0;
                o.back_prev = distance as u16;
                o.price = self.len_prices[(i - MATCH_MIN_LEN) as usize] as u32
                    + self.pos_prices[pos_slot(&self.t, distance)] as u32;
                if i == md[offs] as u32 {
                    offs += 2;
                }
            }
        }

        let mut cur = 0u32;
        loop {
            cur += 1;
            if cur == len_end || cur == NUM_OPTS_BASE {
                let (len, back) = self.backward(cur);
                return (len, back);
            }
            self.get_matches();
            let num_pairs = self.match_distances[0] as usize;
            let mut new_len = 0u32;
            if num_pairs != 0 {
                new_len = self.match_distances[1 + num_pairs - 2] as u32;
                if new_len > NUM_FAST_BYTES {
                    let back_new = self.match_distances[1 + num_pairs - 1];
                    let (len, back) = self.backward(cur);
                    self.optimum[cur as usize].back_prev = back_new;
                    self.optimum_end_index = cur + new_len;
                    self.optimum[cur as usize].pos_prev = self.optimum_end_index as u16;
                    self.move_pos(new_len - 1);
                    return (len, back);
                }
            }
            let mut cur_price = self.optimum[cur as usize].price;
            {
                let cur_and1_price = cur_price + self.literal_prices[self.byte_at(cur) as usize] as u32;
                let o = &mut self.optimum[cur as usize + 1];
                if cur_and1_price < o.price {
                    o.price = cur_and1_price;
                    o.pos_prev = cur as u16;
                }
            }
            if num_pairs == 0 {
                continue;
            }
            while len_end < cur + new_len {
                len_end += 1;
                self.optimum[len_end as usize].price = INFINITY_PRICE;
            }
            let md = &self.match_distances[1..];
            let mut offs = 0usize;
            let mut distance = md[offs + 1] as u32;
            cur_price += self.pos_prices[pos_slot(&self.t, distance)] as u32;
            let mut len_test = MATCH_MIN_LEN;
            loop {
                let cur_and_len_price = cur_price + self.len_prices[(len_test - MATCH_MIN_LEN) as usize] as u32;
                let o = &mut self.optimum[(cur + len_test) as usize];
                if cur_and_len_price < o.price {
                    o.price = cur_and_len_price;
                    o.pos_prev = cur as u16;
                    o.back_prev = distance as u16;
                }
                if len_test == md[offs] as u32 {
                    offs += 2;
                    if offs == num_pairs {
                        break;
                    }
                    cur_price -= self.pos_prices[pos_slot(&self.t, distance)] as u32;
                    distance = md[offs + 1] as u32;
                    cur_price += self.pos_prices[pos_slot(&self.t, distance)] as u32;
                }
                len_test += 1;
            }
        }
    }

    /// `TryBlock`.
    fn try_block(&mut self) {
        self.main_freqs = [0; FIXED_MAIN_TABLE_SIZE];
        self.dist_freqs = [0; DIST_TABLE_SIZE64];
        self.values.clear();
        let block_size = self.block_size_res;
        self.block_size_res = 0;
        loop {
            if self.optimum_current_index == self.optimum_end_index
                && (self.block_size_res >= block_size
                    || (!self.second_pass && (self.mf.avail() == 0 || self.values.len() as u32 >= VALUE_BLOCK_SIZE)))
            {
                break;
            }
            let (len, pos) = self.get_optimal();
            if len >= MATCH_MIN_LEN {
                let new_len = len - MATCH_MIN_LEN;
                self.main_freqs[SYMBOL_MATCH + self.t.len_slots[new_len as usize] as usize] += 1;
                self.dist_freqs[pos_slot(&self.t, pos)] += 1;
                self.values.push(CodeValue { len: new_len as u16, pos: pos as u16 });
            } else {
                let b = self.byte_at(0);
                self.main_freqs[b as usize] += 1;
                self.values.push(CodeValue { len: CODE_VALUE_LITERAL, pos: b as u16 });
            }
            self.additional_offset -= len;
            self.block_size_res += len;
        }
        self.main_freqs[SYMBOL_END_OF_BLOCK] += 1;
        self.additional_offset += self.block_size_res;
        self.second_pass = true;
    }

    /// `SetPrices`.
    fn set_prices(&mut self, levels: &Levels) {
        for i in 0..256 {
            let p = levels.lit_len[i];
            self.literal_prices[i] = if p != 0 { p } else { NO_LITERAL_STAT_PRICE };
        }
        for i in 0..NUM_LEN_SYMBOLS {
            let slot = self.t.len_slots[i] as usize;
            let p = levels.lit_len[SYMBOL_MATCH + slot];
            self.len_prices[i] = (if p != 0 { p } else { NO_LEN_STAT_PRICE }) + LEN_DIRECT_BITS32[slot];
        }
        for (i, price) in self.pos_prices.iter_mut().enumerate() {
            let p = levels.dist[i];
            *price = (if p != 0 { p } else { NO_POS_STAT_PRICE }) + DIST_DIRECT_BITS[i];
        }
    }

    fn make_tables(&mut self, max_huff_len: u32) {
        huffman_generate(&self.main_freqs, &mut self.main_codes, &mut self.new_levels.lit_len, max_huff_len);
        huffman_generate(&self.dist_freqs, &mut self.dist_codes, &mut self.new_levels.dist, max_huff_len);
    }

    fn lz_block_price(&self) -> u32 {
        huffman_price_spec(&self.main_freqs, &self.new_levels.lit_len, &LEN_DIRECT_BITS32, SYMBOL_MATCH)
            + huffman_price_spec(&self.dist_freqs, &self.new_levels.dist, &DIST_DIRECT_BITS, 0)
    }

    /// `TryDynBlock(1, 1)`.
    fn try_dyn_block(&mut self) -> u32 {
        self.block_size_res = self.table_block_size_res;
        let t = self.table_levels;
        self.set_prices(&t);
        self.try_block();
        let n = self.values.len();
        let num_huff_bits = if n > 18000 {
            12
        } else if n > 7000 {
            11
        } else if n > 2000 {
            10
        } else {
            9
        };
        self.make_tables(num_huff_bits);
        let nl = self.new_levels;
        self.set_prices(&nl);
        self.table_levels = self.new_levels;

        self.num_lit_len_levels = MAIN_TABLE_SIZE;
        while self.num_lit_len_levels > NUM_LIT_LEN_CODES_MIN
            && self.new_levels.lit_len[self.num_lit_len_levels - 1] == 0
        {
            self.num_lit_len_levels -= 1;
        }
        self.num_dist_levels = DIST_TABLE_SIZE64;
        while self.num_dist_levels > NUM_DIST_CODES_MIN && self.new_levels.dist[self.num_dist_levels - 1] == 0 {
            self.num_dist_levels -= 1;
        }

        let mut level_freqs = [0u32; LEVEL_TABLE_SIZE];
        level_table_dummy(&self.new_levels.lit_len[..self.num_lit_len_levels], &mut level_freqs);
        level_table_dummy(&self.new_levels.dist[..self.num_dist_levels], &mut level_freqs);
        huffman_generate(&level_freqs, &mut self.level_codes, &mut self.level_lens, MAX_LEVEL_BIT_LENGTH);

        self.num_level_codes = NUM_LEVEL_CODES_MIN;
        for (i, &symbol) in CODE_LENGTH_ALPHABET_ORDER.iter().enumerate() {
            let level = self.level_lens[symbol];
            if level > 0 && i >= self.num_level_codes {
                self.num_level_codes = i + 1;
            }
            self.level_levels[i] = level;
        }

        self.lz_block_price()
            + huffman_price_spec(&level_freqs, &self.level_lens, &LEVEL_DIRECT_BITS, TABLE_DIRECT_LEVELS)
            + 5
            + 5
            + 4
            + self.num_level_codes as u32 * 3
            + 1
            + 2
    }

    /// `GetBlockPrice(1, 1)`.
    fn get_block_price(&mut self) {
        let price = self.try_dyn_block();
        self.table_block_size_res = self.block_size_res;
        let additional_offset_end = self.additional_offset;
        let store_price = store_price(self.block_size_res, 0);
        self.table_store_mode = store_price <= price;
        self.additional_offset = additional_offset_end;
    }

    fn write_store_block(&mut self, mut block_size: u32, mut additional_offset: u32, final_block: bool) {
        loop {
            let cur_block_size = block_size.min((1 << 16) - 1);
            block_size -= cur_block_size;
            self.bits.write_bits((final_block && block_size == 0) as u32, 1);
            self.bits.write_bits(0, 2);
            self.bits.flush_byte();
            self.bits.write_bits(cur_block_size & 0xFFFF, 16);
            self.bits.write_bits(!cur_block_size & 0xFFFF, 16);
            let start = self.mf.cur() - additional_offset as usize;
            let data = self.mf.data;
            self.bits.out.extend_from_slice(&data[start..start + cur_block_size as usize]);
            additional_offset -= cur_block_size;
            if block_size == 0 {
                break;
            }
        }
    }

    fn write_block(&mut self) {
        reverse_bits(&mut self.main_codes, &self.new_levels.lit_len);
        reverse_bits(&mut self.dist_codes, &self.new_levels.dist);
        for i in 0..self.values.len() {
            let v = self.values[i];
            let lit_len = &self.new_levels.lit_len;
            if v.len == CODE_VALUE_LITERAL {
                self.bits.write_bits(self.main_codes[v.pos as usize], lit_len[v.pos as usize] as u32);
            } else {
                let len = v.len as u32;
                let len_slot = self.t.len_slots[len as usize] as usize;
                let sym = SYMBOL_MATCH + len_slot;
                self.bits.write_bits(self.main_codes[sym], lit_len[sym] as u32);
                self.bits.write_bits(len - LEN_START32[len_slot] as u32, LEN_DIRECT_BITS32[len_slot] as u32);
                let dist = v.pos as u32;
                let ps = pos_slot(&self.t, dist);
                self.bits.write_bits(self.dist_codes[ps], self.new_levels.dist[ps] as u32);
                self.bits.write_bits(dist - DIST_START[ps], DIST_DIRECT_BITS[ps] as u32);
            }
        }
        self.bits.write_bits(self.main_codes[SYMBOL_END_OF_BLOCK], self.new_levels.lit_len[SYMBOL_END_OF_BLOCK] as u32);
    }

    /// `CodeBlock(1, final)`.
    fn code_block(&mut self, final_block: bool) {
        if self.table_store_mode {
            self.write_store_block(self.table_block_size_res, self.additional_offset, final_block);
        } else {
            self.bits.write_bits(final_block as u32, 1);
            self.bits.write_bits(2, 2);
            self.bits.write_bits((self.num_lit_len_levels - NUM_LIT_LEN_CODES_MIN) as u32, 5);
            self.bits.write_bits((self.num_dist_levels - NUM_DIST_CODES_MIN) as u32, 5);
            self.bits.write_bits((self.num_level_codes - NUM_LEVEL_CODES_MIN) as u32, 4);
            for i in 0..self.num_level_codes {
                self.bits.write_bits(self.level_levels[i] as u32, 3);
            }
            reverse_bits(&mut self.level_codes, &self.level_lens);
            let lit_len = self.new_levels.lit_len;
            let dist = self.new_levels.dist;
            level_table_code(&mut self.bits, &lit_len[..self.num_lit_len_levels], &self.level_lens, &self.level_codes);
            level_table_code(&mut self.bits, &dist[..self.num_dist_levels], &self.level_lens, &self.level_codes);
            self.write_block();
        }
        self.additional_offset -= self.table_block_size_res;
    }

    /// `CodeReal`.
    fn run(mut self) -> Vec<u8> {
        loop {
            self.table_block_size_res = BLOCK_UNCOMPRESSED_SIZE_THRESHOLD;
            self.second_pass = false;
            self.get_block_price();
            let final_block = self.mf.avail() == 0;
            self.code_block(final_block);
            if self.mf.avail() == 0 {
                break;
            }
        }
        self.bits.flush_byte();
        self.bits.out
    }
}

/// `GetStorePrice`.
fn store_price(mut block_size: u32, mut bit_position: u32) -> u32 {
    let mut price = 0u32;
    loop {
        let next_bit_position = (bit_position + 1 + 2) & 7;
        let num_bits_for_align = if next_bit_position > 0 { 8 - next_bit_position } else { 0 };
        let cur_block_size = block_size.min((1 << 16) - 1);
        price += 1 + 2 + num_bits_for_align + (2 + 2) * 8 + cur_block_size * 8;
        bit_position = 0;
        block_size -= cur_block_size;
        if block_size == 0 {
            break;
        }
    }
    price
}

/// Run-length state machine shared by `LevelTableDummy` and `LevelTableCode`:
/// calls `emit(symbol, extra_value, extra_bits)` for each code-length code.
fn level_table(levels: &[u8], mut emit: impl FnMut(usize, u32, u32)) {
    let num_levels = levels.len();
    let mut prev_len = 0xFFusize;
    let mut next_len = levels[0] as usize;
    let mut count = 0u32;
    let (mut max_count, mut min_count) = (7u32, 4u32);
    if next_len == 0 {
        max_count = 138;
        min_count = 3;
    }
    for n in 0..num_levels {
        let cur_len = next_len;
        next_len = if n < num_levels - 1 { levels[n + 1] as usize } else { 0xFF };
        count += 1;
        if count < max_count && cur_len == next_len {
            continue;
        }
        if count < min_count {
            for _ in 0..count {
                emit(cur_len, 0, 0);
            }
        } else if cur_len != 0 {
            if cur_len != prev_len {
                emit(cur_len, 0, 0);
                count -= 1;
            }
            emit(TABLE_LEVEL_REP_NUMBER, count - 3, 2);
        } else if count <= 10 {
            emit(TABLE_LEVEL0_NUMBER, count - 3, 3);
        } else {
            emit(TABLE_LEVEL0_NUMBER2, count - 11, 7);
        }
        count = 0;
        prev_len = cur_len;
        if next_len == 0 {
            max_count = 138;
            min_count = 3;
        } else if cur_len == next_len {
            max_count = 6;
            min_count = 3;
        } else {
            max_count = 7;
            min_count = 4;
        }
    }
}

/// `LevelTableDummy`.  (A run shorter than `min_count` is counted one symbol
/// at a time.)
fn level_table_dummy(levels: &[u8], freqs: &mut [u32; LEVEL_TABLE_SIZE]) {
    level_table(levels, |sym, _, _| freqs[sym] += 1);
}

/// `LevelTableCode`.
fn level_table_code(
    bits: &mut BitWriter,
    levels: &[u8],
    lens: &[u8; LEVEL_TABLE_SIZE],
    codes: &[u32; LEVEL_TABLE_SIZE],
) {
    level_table(levels, |sym, extra, extra_bits| {
        bits.write_bits(codes[sym], lens[sym] as u32);
        if extra_bits != 0 {
            bits.write_bits(extra, extra_bits);
        }
    });
}

/// Raw deflate stream of `data` as 7-Zip writes it at level 5 (the default,
/// `-mx=5`) into a zip entry.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    Encoder::new(data).run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn inflate(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(b).read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn round_trips() {
        let mut seed = 0x1234_5678u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut inputs: Vec<Vec<u8>> = vec![b"a".to_vec(), b"ab".to_vec(), b"abc".to_vec(), vec![0; 100_000]];
        inputs.push((0..70_000).map(|_| rnd() as u8).collect());
        inputs.push((0..200_000).map(|i| b"the quick brown fox "[(i * 7 + (rnd() as usize % 3)) % 20]).collect());
        for data in inputs {
            assert_eq!(inflate(&deflate(&data)), data, "len {}", data.len());
        }
    }

    #[test]
    fn huffman_two_symbols_and_limit() {
        let freqs = [5u32, 0, 3];
        let mut p = [0u32; 3];
        let mut lens = [0u8; 3];
        huffman_generate(&freqs, &mut p, &mut lens, 7);
        assert_eq!(lens, [1, 0, 1]);
        // Fibonacci frequencies force the length limit.
        let freqs: Vec<u32> = [1u32, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144].to_vec();
        let mut p = vec![0u32; freqs.len()];
        let mut lens = vec![0u8; freqs.len()];
        huffman_generate(&freqs, &mut p, &mut lens, 7);
        assert!(lens.iter().all(|&l| (1..=7).contains(&l)));
        let kraft: f64 = lens.iter().map(|&l| 0.5f64.powi(l as i32)).sum();
        assert!((kraft - 1.0).abs() < 1e-12);
    }

    #[test]
    fn pos_slots() {
        let t = tables();
        for (slot, &start) in DIST_START[..30].iter().enumerate() {
            assert_eq!(pos_slot(&t, start), slot);
        }
        assert_eq!(pos_slot(&t, 32767), 29);
    }
}
