//! The iRacing telemetry memory map (`irsdk_header`, variable buffers and variable headers).

use std::collections::HashMap;

use super::session_info::SessionRegion;

/// Reads a little-endian `i32` at `offset`, `None` outside the memory.
fn i32_at(memory: &[u8], offset: usize) -> Option<i32> {
    let bytes = memory.get(offset..offset.checked_add(4)?)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

/// A negative offset is resolved as a whole relative to the end of the buffer.
fn unpack_bytes(memory: &[u8], offset: i64, size: usize) -> Option<&[u8]> {
    let offset = if offset < 0 {
        if offset.checked_add(i64::try_from(size).ok()?)? > 0 {
            return None;
        }
        offset.checked_add(i64::try_from(memory.len()).ok()?)?
    } else {
        offset
    };
    let start = usize::try_from(offset).ok()?;
    memory.get(start..start.checked_add(size)?)
}

fn signed_i32_at(memory: &[u8], offset: i64) -> Option<i32> {
    Some(i32::from_le_bytes(unpack_bytes(memory, offset, 4)?.try_into().ok()?))
}

/// Header fields used by the listener.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header {
    pub(crate) session: SessionRegion,
    pub(crate) num_vars: i32,
    pub(crate) var_header_offset: i32,
    #[cfg(test)]
    pub(crate) num_buf: i32,
    #[cfg(test)]
    pub(crate) buf_len: i32,
}

impl Header {
    /// The header at the start of the map (offsets 0..36).
    pub(crate) fn read(memory: &[u8]) -> Option<Header> {
        let field = |offset| i32_at(memory, offset);
        Some(Header {
            session: SessionRegion { update: field(12)?, len: field(16)?, offset: field(20)? },
            num_vars: field(24)?,
            var_header_offset: field(28)?,
            #[cfg(test)]
            num_buf: field(32)?,
            #[cfg(test)]
            buf_len: field(36)?,
        })
    }
}

/// The "connected" bit of the header's status field.
pub(crate) const STATUS_CONNECTED: i32 = 1;

/// The variable buffers as fixed at start-up (built once from `num_buf`); their
/// `tick_count` and `buf_offset` are read live.
#[derive(Debug, Clone)]
pub(crate) struct Buffers {
    lengths: Vec<i32>,
}

impl Buffers {
    /// The buffers for the header seen at start-up (a negative count is empty).
    #[cfg(test)]
    pub(crate) fn at_startup(header: &Header) -> Self {
        Buffers { lengths: vec![header.buf_len; usize::try_from(header.num_buf).unwrap_or(0)] }
    }

    pub(crate) fn from_lengths(lengths: Vec<i32>) -> Self {
        Self { lengths }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn descriptor_bytes(&self) -> Option<usize> {
        self.lengths.len().checked_mul(16)?.checked_add(48)
    }

    pub(crate) fn count(&self) -> usize {
        self.lengths.len()
    }

    pub(crate) fn length(&self, index: usize) -> Option<i32> {
        self.lengths.get(index).copied()
    }

    /// The buffer every variable read uses: the buffers sorted by
    /// `tick_count`, newest first (stable), the second one. Returns that buffer's `buf_offset`; `None` when
    /// there are fewer than two buffers, or a buffer descriptor lies outside the memory.
    pub(crate) fn second_newest_offset(&self, memory: &[u8]) -> Option<i64> {
        Some(i64::from(self.ordered(memory)?.get(1)?.2))
    }

    pub(crate) fn second_newest_index(&self, memory: &[u8]) -> Option<usize> {
        Some(self.ordered(memory)?.get(1)?.0)
    }

    /// The newest buffer and a frozen copy of its bytes.
    #[cfg(test)]
    pub(crate) fn freeze_newest(&self, memory: &[u8]) -> Option<(usize, Vec<u8>)> {
        let &(index, _, offset) = self.ordered(memory)?.first()?;
        let bound = |index: i64| {
            let len = memory.len() as i64;
            (if index < 0 { len + index } else { index }).clamp(0, len) as usize
        };
        let start = bound(i64::from(offset));
        let end = bound(i64::from(offset) + i64::from(*self.lengths.get(index)?));
        Some((index, memory[start..end.max(start)].to_vec()))
    }

    fn ordered(&self, memory: &[u8]) -> Option<Vec<(usize, i32, i32)>> {
        let mut buffers = Vec::with_capacity(self.lengths.len());
        for i in 0..self.lengths.len() {
            let descriptor = 48 + 16 * i;
            buffers.push((i, i32_at(memory, descriptor)?, i32_at(memory, descriptor + 4)?));
        }
        buffers.sort_by_key(|&(_, tick_count, _)| std::cmp::Reverse(tick_count));
        Some(buffers)
    }
}

/// One value of a variable: a character, a boolean, a signed or unsigned 32-bit integer, a float or a double.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Scalar {
    /// A character: one byte.
    Char(u8),
    Bool(bool),
    Int(i64),
    Float(f64),
}

impl Scalar {
    /// The value as a number (booleans and integers included); `None` for a character.
    pub(crate) fn number(self) -> Option<f64> {
        match self {
            Scalar::Char(_) => None,
            Scalar::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
            Scalar::Int(i) => Some(i as f64),
            Scalar::Float(f) => Some(f),
        }
    }
}

/// A telemetry variable: a scalar when `count == 1`, otherwise a list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum VarValue {
    One(Scalar),
    Many(Vec<Scalar>),
}

/// A variable type, in type-code order: character, boolean, int, unsigned int, float, double.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarType {
    Char,
    Bool,
    Int,
    BitField,
    Float,
    Double,
}

impl VarType {
    const MAP: [VarType; 6] =
        [VarType::Char, VarType::Bool, VarType::Int, VarType::BitField, VarType::Float, VarType::Double];

    /// The type of a type code: 0..=5, or -6..=-1 counted from the end of the list; `None` for any other code.
    fn from_header(kind: i32) -> Option<VarType> {
        let index = if kind < 0 { kind + 6 } else { kind };
        VarType::MAP.get(usize::try_from(index).ok()?).copied()
    }

    /// Bytes per element.
    fn size(self) -> usize {
        match self {
            VarType::Char | VarType::Bool => 1,
            VarType::Int | VarType::BitField | VarType::Float => 4,
            VarType::Double => 8,
        }
    }

    /// One little-endian element of `size()` bytes.
    fn decode(self, b: &[u8]) -> Scalar {
        let word = || [b[0], b[1], b[2], b[3]];
        match self {
            VarType::Char => Scalar::Char(b[0]),
            VarType::Bool => Scalar::Bool(b[0] != 0),
            VarType::Int => Scalar::Int(i64::from(i32::from_le_bytes(word()))),
            VarType::BitField => Scalar::Int(i64::from(u32::from_le_bytes(word()))),
            VarType::Float => Scalar::Float(f64::from(f32::from_le_bytes(word()))),
            VarType::Double => Scalar::Float(f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])),
        }
    }
}

/// Bytes of one variable header.
const VAR_HEADER_LEN: usize = 144;

/// The position of each variable's header by name (a later duplicate name replaces an
/// earlier one). Only the names are cached; `type`, `offset` and `count` are read from the
/// current memory on every lookup.
#[derive(Debug, Default)]
pub(crate) struct Variables {
    header_at: HashMap<String, i64>,
}

impl Variables {
    pub(crate) fn insert_name(&mut self, name: &[u8], at: i64) {
        self.header_at.insert(trim_nuls(name).iter().map(|&b| char::from(b)).collect(), at);
    }

    pub(crate) fn header_position(&self, name: &str) -> Option<i64> {
        self.header_at.get(name).copied()
    }

    pub(crate) fn decode_value(kind: i32, count: i32, data: &[u8]) -> Option<VarValue> {
        let kind = VarType::from_header(kind)?;
        let size = kind.size();
        let expected = size.checked_mul(usize::try_from(count).unwrap_or(0))?;
        if data.len() != expected {
            return None;
        }
        let mut values: Vec<_> = data.chunks_exact(size).map(|bytes| kind.decode(bytes)).collect();
        Some(if count == 1 { VarValue::One(values.remove(0)) } else { VarValue::Many(values) })
    }

    pub(crate) fn reshape(value: VarValue, count: i32) -> Option<VarValue> {
        let mut values = match value {
            VarValue::One(value) => vec![value],
            VarValue::Many(values) => values,
        };
        Some(if count == 1 { VarValue::One(values.drain(..).next()?) } else { VarValue::Many(values) })
    }

    pub(crate) fn value_size(kind: i32, count: i32) -> Option<usize> {
        VarType::from_header(kind)?.size().checked_mul(usize::try_from(count).unwrap_or(0))
    }

    pub(crate) fn absolute_offset(total: usize, offset: i64, size: usize) -> Option<usize> {
        let start = if offset < 0 {
            if offset.checked_add(i64::try_from(size).ok()?)? > 0 {
                return None;
            }
            offset.checked_add(i64::try_from(total).ok()?)?
        } else {
            offset
        };
        let start = usize::try_from(start).ok()?;
        (start.checked_add(size)? <= total).then_some(start)
    }
    /// Builds the name table on first use: the names of the `num_vars` headers of 144 bytes at
    /// `var_header_offset` (the 32-byte field at +16, NULs stripped, decoded as Latin-1). Also returns whether every
    /// name could be read: reading stops at the first name outside the memory, leaving the
    /// names read before it cached.
    pub(crate) fn read(memory: &[u8], header: &Header) -> (Self, bool) {
        let mut header_at = HashMap::new();
        let count = usize::try_from(header.num_vars).unwrap_or(0);
        let base = i64::from(header.var_header_offset);
        for at in (0..count).map(|i| base + VAR_HEADER_LEN as i64 * i as i64) {
            let Some(name) = unpack_bytes(memory, at + 16, 32) else {
                return (Variables { header_at }, false);
            };
            header_at.insert(trim_nuls(name).iter().map(|&b| char::from(b)).collect(), at);
        }
        (Variables { header_at }, true)
    }

    /// Whether `name` is a telemetry variable (otherwise it is looked up in the session info).
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.header_at.contains_key(name)
    }

    /// Reads a variable: the header's current `type`, `count` and `offset`,
    /// then `count` elements of that type in the buffer starting at `buffer_offset`. `None` for
    /// an unknown type code or data outside the memory. A type of -6..=-1 indexes the type list
    /// from its end.
    pub(crate) fn value(&self, memory: &[u8], buffer_offset: i64, name: &str) -> Option<VarValue> {
        self.value_from(memory, memory, buffer_offset, name)
    }

    /// Headers stay live even when the selected buffer returns its frozen bytes.
    pub(crate) fn value_from(&self, memory: &[u8], values: &[u8], buffer_offset: i64, name: &str) -> Option<VarValue> {
        let &at = self.header_at.get(name)?;
        let var_type = VarType::from_header(signed_i32_at(memory, at)?)?;
        let count = signed_i32_at(memory, at + 8)?;
        let offset = signed_i32_at(memory, at + 4)?;
        let size = var_type.size();
        let elements = usize::try_from(count).unwrap_or(0);
        let start = buffer_offset.checked_add(i64::from(offset))?;
        let data = unpack_bytes(values, start, size.checked_mul(elements)?)?;
        let mut scalars: Vec<Scalar> = data.chunks_exact(size).map(|bytes| var_type.decode(bytes)).collect();
        Some(if count == 1 { VarValue::One(scalars.remove(0)) } else { VarValue::Many(scalars) })
    }
}

/// The bytes without leading and trailing NULs.
fn trim_nuls(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|&b| b != 0).map_or(start, |last| last + 1);
    &bytes[start..end]
}

#[cfg(test)]
pub(crate) mod test_map {
    //! A synthetic iRacing memory map for tests.

    /// Variable type codes.
    pub(crate) const BOOL: i32 = 1;
    pub(crate) const INT: i32 = 2;
    pub(crate) const BITFIELD: i32 = 3;
    pub(crate) const FLOAT: i32 = 4;

    /// Builds a memory map with a header, `buffers` variable buffers, variable headers and a session string.
    pub(crate) struct MapBuilder {
        pub(crate) memory: Vec<u8>,
        vars: Vec<(String, i32, i32)>,
        buffers: usize,
    }

    pub(crate) const VAR_HEADERS: usize = 1024;
    pub(crate) const SESSION: usize = 0x10000;
    pub(crate) const BUFFER_BASE: usize = 0x20000;
    pub(crate) const BUFFER_LEN: usize = 0x1000;

    impl MapBuilder {
        pub(crate) fn new(buffers: usize) -> Self {
            let mut memory = vec![0; 0x30000];
            put(&mut memory, 0, 2); // version
            put(&mut memory, 4, 1); // status connected
            put(&mut memory, 8, 60);
            put(&mut memory, 28, VAR_HEADERS as i32);
            put(&mut memory, 32, buffers as i32);
            put(&mut memory, 36, BUFFER_LEN as i32);
            for i in 0..buffers {
                put(&mut memory, 48 + 16 * i, 100 + i as i32); // tick_count
                put(&mut memory, 52 + 16 * i, (BUFFER_BASE + i * BUFFER_LEN) as i32);
            }
            MapBuilder { memory, vars: Vec::new(), buffers }
        }

        /// Declares a variable (once; later calls with the same name keep the first declaration).
        pub(crate) fn var(&mut self, name: &str, kind: i32, count: i32) -> &mut Self {
            if self.vars.iter().any(|(n, _, _)| n == name) {
                return self;
            }
            let offset = 64 * self.vars.len() as i32;
            let at = VAR_HEADERS + 144 * self.vars.len();
            put(&mut self.memory, at, kind);
            put(&mut self.memory, at + 4, offset);
            put(&mut self.memory, at + 8, count);
            self.memory[at + 16..at + 16 + name.len()].copy_from_slice(name.as_bytes());
            self.vars.push((name.to_owned(), kind, offset));
            put(&mut self.memory, 24, self.vars.len() as i32);
            self
        }

        /// Writes the raw little-endian `bytes` of variable `name` (element `index`) into every buffer.
        pub(crate) fn raw(&mut self, name: &str, index: usize, bytes: &[u8]) -> &mut Self {
            let offset = self.vars.iter().find(|(n, _, _)| n == name).expect("declared variable").2;
            for b in 0..self.buffers {
                let at = BUFFER_BASE + b * BUFFER_LEN + offset as usize + index * bytes.len();
                self.memory[at..at + bytes.len()].copy_from_slice(bytes);
            }
            self
        }

        pub(crate) fn float(&mut self, name: &str, value: f32) -> &mut Self {
            self.var(name, FLOAT, 1).raw(name, 0, &value.to_le_bytes())
        }

        pub(crate) fn int(&mut self, name: &str, value: i32) -> &mut Self {
            self.var(name, INT, 1).raw(name, 0, &value.to_le_bytes())
        }

        pub(crate) fn bits(&mut self, name: &str, value: u32) -> &mut Self {
            self.var(name, BITFIELD, 1).raw(name, 0, &value.to_le_bytes())
        }

        pub(crate) fn boolean(&mut self, name: &str, value: bool) -> &mut Self {
            self.var(name, BOOL, 1).raw(name, 0, &[u8::from(value)])
        }

        /// Writes `text` as the session string and bumps `session_info_update`.
        pub(crate) fn session(&mut self, text: &str) -> &mut Self {
            self.memory[SESSION..SESSION + text.len()].copy_from_slice(text.as_bytes());
            put(&mut self.memory, 16, text.len() as i32 + 4);
            put(&mut self.memory, 20, SESSION as i32);
            let update = i32::from_le_bytes(self.memory[12..16].try_into().unwrap()) + 1;
            put(&mut self.memory, 12, update);
            self
        }

        pub(crate) fn header_field(&mut self, offset: usize, value: i32) -> &mut Self {
            put(&mut self.memory, offset, value);
            self
        }

        pub(crate) fn build(&self) -> Vec<u8> {
            self.memory.clone()
        }
    }

    fn put(memory: &mut [u8], offset: usize, value: i32) {
        memory[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::test_map::*;
    use super::*;

    #[test]
    fn native_iracing_signed_variable_and_buffer_offsets_follow_unpack_from() {
        let mut map = MapBuilder::new(2);
        map.float("Speed", 17.0);
        let mut memory = map.build();
        memory[VAR_HEADERS + 4..VAR_HEADERS + 8].copy_from_slice(&(-4_i32).to_le_bytes());
        memory[BUFFER_BASE - 4..BUFFER_BASE].copy_from_slice(&29_f32.to_le_bytes());
        let header = Header::read(&memory).unwrap();
        let (variables, complete) = Variables::read(&memory, &header);
        assert!(complete);
        let buffers = Buffers::at_startup(&header);
        let offset = buffers.second_newest_offset(&memory).unwrap();
        assert_eq!(variables.value(&memory, offset, "Speed"), Some(VarValue::One(Scalar::Float(29.0))));

        // Add the live signed buffer and variable offsets before resolving from the end.
        memory[52..56].copy_from_slice(&(-8_i32).to_le_bytes());
        memory[VAR_HEADERS + 4..VAR_HEADERS + 8].copy_from_slice(&4_i32.to_le_bytes());
        let len = memory.len();
        memory[len - 4..].copy_from_slice(&31_f32.to_le_bytes());
        let offset = buffers.second_newest_offset(&memory).expect("negative buffer offsets are retained");
        assert_eq!(variables.value(&memory, offset, "Speed"), Some(VarValue::One(Scalar::Float(31.0))));

        for (relative, count, expected) in [
            (-4, 1_i32, Some(VarValue::One(Scalar::Float(31.0)))),
            (-3, 1, None),
            (-(len as i32) - 1, 1, None),
            (-1, 0, Some(VarValue::Many(vec![]))),
            (-1, -2, Some(VarValue::Many(vec![]))),
            (-(len as i32), 0, Some(VarValue::Many(vec![]))),
            (len as i32, 0, Some(VarValue::Many(vec![]))),
            (len as i32 + 1, 0, None),
        ] {
            memory[VAR_HEADERS + 4..VAR_HEADERS + 8].copy_from_slice(&relative.to_le_bytes());
            memory[VAR_HEADERS + 8..VAR_HEADERS + 12].copy_from_slice(&count.to_le_bytes());
            assert_eq!(variables.value(&memory, 0, "Speed"), expected, "offset={relative}, count={count}");
        }
    }

    #[test]
    fn native_iracing_signed_header_offsets_resolve_each_property_separately() {
        let mut map = MapBuilder::new(2);
        map.float("Speed", 17.0);
        let mut memory = map.build();
        let mut header = Header::read(&memory).unwrap();
        header.var_header_offset = -144;
        let len = memory.len();
        let original = memory[VAR_HEADERS..VAR_HEADERS + VAR_HEADER_LEN].to_vec();
        memory[len - VAR_HEADER_LEN..].copy_from_slice(&original);
        let (variables, complete) = Variables::read(&memory, &header);
        assert!(complete);
        assert!(variables.contains("Speed"), "the header can start relative to the map end");
        let offset = Buffers::at_startup(&header).second_newest_offset(&memory).unwrap();
        assert_eq!(variables.value(&memory, offset, "Speed"), Some(VarValue::One(Scalar::Float(17.0))));

        // Base -16 makes name offset 0, while type/offset/count remain relative to the end.
        let mut memory = vec![0; 256];
        header.var_header_offset = -16;
        memory[..8].copy_from_slice(b"Crossing");
        memory[240..244].copy_from_slice(&INT.to_le_bytes());
        memory[244..248].copy_from_slice(&0_i32.to_le_bytes());
        memory[248..252].copy_from_slice(&1_i32.to_le_bytes());
        memory[64..68].copy_from_slice(&42_i32.to_le_bytes());
        let (variables, complete) = Variables::read(&memory, &header);
        assert!(complete);
        assert_eq!(variables.value(&memory, 64, "Crossing"), Some(VarValue::One(Scalar::Int(42))));

        // An invalid later name retains earlier names in the cached table.
        header.var_header_offset = -144;
        header.num_vars = 4;
        memory[128..133].copy_from_slice(b"First");
        memory[16..22].copy_from_slice(b"Second");
        memory[160..165].copy_from_slice(b"Third");
        let (variables, complete) = Variables::read(&memory, &header);
        assert!(!complete);
        for name in ["First", "Second", "Third"] {
            assert!(variables.contains(name), "{name}");
        }
    }

    /// Buffer selection: the second-newest buffer by tick_count, stable for equal ticks; needs two buffers.
    #[test]
    fn second_newest_buffer() {
        let mut map = MapBuilder::new(3);
        map.header_field(48, 500).header_field(64, 700).header_field(80, 600);
        let memory = map.build();
        let header = Header::read(&memory).unwrap();
        assert_eq!(
            Buffers::at_startup(&header).second_newest_offset(&memory),
            Some((BUFFER_BASE + 2 * BUFFER_LEN) as i64)
        );
        map.header_field(48, 700);
        let memory = map.build();
        assert_eq!(Buffers::at_startup(&header).second_newest_offset(&memory), Some((BUFFER_BASE + BUFFER_LEN) as i64));
        let single = MapBuilder::new(1).build();
        let header = Header::read(&single).unwrap();
        assert_eq!(Buffers::at_startup(&header).second_newest_offset(&single), None);
    }

    #[test]
    fn frozen_copy_uses_startup_length_and_python_slice_bounds() {
        let mut map = MapBuilder::new(3);
        map.header_field(36, 2).header_field(52 + 16 * 2, -4);
        let mut memory = map.build();
        let end = memory.len();
        memory[end - 4..].copy_from_slice(b"ABCD");
        let buffers = Buffers::at_startup(&Header::read(&memory).unwrap());
        memory[36..40].copy_from_slice(&99i32.to_le_bytes());
        assert_eq!(buffers.freeze_newest(&memory), Some((2, b"AB".to_vec())));
    }

    /// Variable reads decode by type code; count 1 is a scalar, otherwise a list.
    #[test]
    fn variable_values() {
        let mut map = MapBuilder::new(2);
        map.float("Speed", 33.5).int("Gear", -1).bits("SessionFlags", 0x8000_0000).boolean("IsOnTrackCar", true);
        map.var("LatAccel", FLOAT, 2).raw("LatAccel", 0, &1.5f32.to_le_bytes()).raw(
            "LatAccel",
            1,
            &2.5f32.to_le_bytes(),
        );
        let memory = map.build();
        let header = Header::read(&memory).unwrap();
        let (vars, complete) = Variables::read(&memory, &header);
        assert!(complete);
        let offset = Buffers::at_startup(&header).second_newest_offset(&memory).unwrap();
        assert_eq!(vars.value(&memory, offset, "Speed"), Some(VarValue::One(Scalar::Float(33.5))));
        assert_eq!(vars.value(&memory, offset, "Gear"), Some(VarValue::One(Scalar::Int(-1))));
        assert_eq!(vars.value(&memory, offset, "SessionFlags"), Some(VarValue::One(Scalar::Int(2_147_483_648))));
        assert_eq!(vars.value(&memory, offset, "IsOnTrackCar"), Some(VarValue::One(Scalar::Bool(true))));
        assert_eq!(
            vars.value(&memory, offset, "LatAccel"),
            Some(VarValue::Many(vec![Scalar::Float(1.5), Scalar::Float(2.5)]))
        );
        assert!(vars.contains("Speed"));
        assert!(!vars.contains("DriverInfo"));
    }

    /// A type code indexes the type list, so -1..-6 count from the end
    /// (double, float, unsigned int, int, boolean, character) and only codes >= 6 or < -6 are errors.
    #[test]
    fn negative_types_index_from_the_end() {
        let mut map = MapBuilder::new(2);
        map.var("Double", -1, 1).raw("Double", 0, &2.5f64.to_le_bytes());
        map.var("Float", -2, 1).raw("Float", 0, &1.5f32.to_le_bytes());
        map.var("Bits", -3, 1).raw("Bits", 0, &u32::MAX.to_le_bytes());
        map.var("Int", -4, 1).raw("Int", 0, &(-7i32).to_le_bytes());
        map.var("Flag", -5, 1).raw("Flag", 0, &[1]);
        map.var("Char", -6, 1).raw("Char", 0, b"x");
        map.var("TooLow", -7, 1).var("TooHigh", 6, 1);
        let memory = map.build();
        let header = Header::read(&memory).unwrap();
        let (vars, complete) = Variables::read(&memory, &header);
        assert!(complete);
        let offset = Buffers::at_startup(&header).second_newest_offset(&memory).unwrap();
        let read = |name| vars.value(&memory, offset, name);
        assert_eq!(read("Double"), Some(VarValue::One(Scalar::Float(2.5))));
        assert_eq!(read("Float"), Some(VarValue::One(Scalar::Float(1.5))));
        assert_eq!(read("Bits"), Some(VarValue::One(Scalar::Int(4_294_967_295))));
        assert_eq!(read("Int"), Some(VarValue::One(Scalar::Int(-7))));
        assert_eq!(read("Flag"), Some(VarValue::One(Scalar::Bool(true))));
        assert_eq!(read("Char"), Some(VarValue::One(Scalar::Char(b'x'))));
        assert_eq!(read("TooLow"), None);
        assert_eq!(read("TooHigh"), None);
    }

    /// A header's type, offset and count are read from the memory on every
    /// access, so a header the simulator rewrites after the name table was built is followed at once.
    #[test]
    fn headers_are_read_on_every_lookup() {
        let mut map = MapBuilder::new(2);
        map.var("LatAccel", FLOAT, 1).raw("LatAccel", 0, &1.5f32.to_le_bytes()).raw(
            "LatAccel",
            1,
            &2.5f32.to_le_bytes(),
        );
        let memory = map.build();
        let header = Header::read(&memory).unwrap();
        let (vars, _) = Variables::read(&memory, &header);
        let offset = Buffers::at_startup(&header).second_newest_offset(&memory).unwrap();
        assert_eq!(vars.value(&memory, offset, "LatAccel"), Some(VarValue::One(Scalar::Float(1.5))));
        map.header_field(VAR_HEADERS + 8, 2); // count 1 -> 2
        let memory = map.build();
        assert_eq!(
            vars.value(&memory, offset, "LatAccel"),
            Some(VarValue::Many(vec![Scalar::Float(1.5), Scalar::Float(2.5)]))
        );
        map.header_field(VAR_HEADERS + 8, 1).header_field(VAR_HEADERS + 4, 4); // offset 0 -> 4
        let memory = map.build();
        assert_eq!(vars.value(&memory, offset, "LatAccel"), Some(VarValue::One(Scalar::Float(2.5))));
        map.header_field(VAR_HEADERS, 7); // unknown type code
        assert_eq!(vars.value(&map.build(), offset, "LatAccel"), None);
    }

    /// The name table is stored before it is filled, so a name outside the memory is an error
    /// that leaves the names read before it as the cached table.
    #[test]
    fn names_outside_the_memory_stop_the_dictionary() {
        let mut map = MapBuilder::new(2);
        map.float("Speed", 3.0).float("RPM", 900.0);
        let len = map.memory.len();
        // Three headers, the third starting 20 bytes before the end of the memory (its name does not fit).
        map.header_field(28, (len - 20 - 2 * VAR_HEADER_LEN) as i32).header_field(24, 3);
        let at = len - 20 - 2 * VAR_HEADER_LEN;
        map.memory.copy_within(VAR_HEADERS..VAR_HEADERS + 2 * VAR_HEADER_LEN, at);
        let memory = map.build();
        let (vars, complete) = Variables::read(&memory, &Header::read(&memory).unwrap());
        assert!(!complete);
        assert!(vars.contains("Speed") && vars.contains("RPM"));
    }

    /// A header name has its NULs stripped on both sides.
    #[test]
    fn names_strip_nuls() {
        assert_eq!(trim_nuls(b"\0Speed\0\0"), b"Speed");
        assert_eq!(trim_nuls(b"\0\0"), b"");
    }
}
