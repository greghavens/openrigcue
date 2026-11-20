//! Minimal FlatBuffers table reader
//! (field offsets, indirection, vectors and vector lengths). Any read outside the
//! buffer is an error.

use super::wire::{Fields, Short};

/// A table at an absolute position in the buffer.
#[derive(Debug, Clone, Copy)]
pub struct Table<'a> {
    buf: Fields<'a>,
    pos: usize,
}

fn at(base: usize, delta: i64) -> Result<usize, Short> {
    usize::try_from(base as i64 + delta).map_err(|_| Short)
}

impl<'a> Table<'a> {
    /// The root table: the root offset is read at 0.
    pub fn root(data: &'a [u8]) -> Result<Self, Short> {
        let buf = Fields::new(data);
        let pos = buf.u32(0)? as usize;
        Ok(Table { buf, pos })
    }

    /// Whether the buffer has the file identifier: bytes 4..8 equal it (a short buffer never matches).
    pub fn has_identifier(data: &[u8], ident: &[u8; 4]) -> bool {
        data.get(4..8) == Some(ident.as_slice())
    }

    /// The field's offset from the table start, 0 when absent.
    fn field(&self, slot: u16) -> Result<usize, Short> {
        let vtable = at(self.pos, -i64::from(self.buf.i32(self.pos)?))?;
        let vtable_len = self.buf.u16(vtable)?;
        if slot < vtable_len {
            Ok(usize::from(self.buf.u16(vtable + usize::from(slot))?))
        } else {
            Ok(0)
        }
    }

    /// A scalar `f32` field, `default` when absent.
    pub fn f32(&self, slot: u16, default: f64) -> Result<f64, Short> {
        match self.field(slot)? {
            0 => Ok(default),
            o => self.buf.f32(self.pos + o),
        }
    }

    /// A scalar `u8` field.
    pub fn u8(&self, slot: u16) -> Result<u8, Short> {
        match self.field(slot)? {
            0 => Ok(0),
            o => self.buf.u8(self.pos + o),
        }
    }

    /// A scalar `i8` field.
    pub fn i8(&self, slot: u16) -> Result<i8, Short> {
        match self.field(slot)? {
            0 => Ok(0),
            o => self.buf.i8(self.pos + o),
        }
    }

    /// A sub-table field (an indirect offset), `None` when absent.
    pub fn table(&self, slot: u16) -> Result<Option<Table<'a>>, Short> {
        match self.field(slot)? {
            0 => Ok(None),
            o => {
                let off = self.pos + o;
                let pos = off.checked_add(self.buf.u32(off)? as usize).ok_or(Short)?;
                Ok(Some(Table { buf: self.buf, pos }))
            }
        }
    }

    /// Length of a vector field, 0 when absent.
    pub fn vector_len(&self, slot: u16) -> Result<usize, Short> {
        match self.field(slot)? {
            0 => Ok(0),
            o => {
                let off = self.pos + o;
                let vec = off.checked_add(self.buf.u32(off)? as usize).ok_or(Short)?;
                Ok(self.buf.u32(vec)? as usize)
            }
        }
    }

    /// Element `index` of a vector of tables (the vector start + 4·index, then an indirect offset), `None` when absent.
    pub fn vector_table(&self, slot: u16, index: usize) -> Result<Option<Table<'a>>, Short> {
        match self.field(slot)? {
            0 => Ok(None),
            o => {
                let off = self.pos + o;
                let start = off.checked_add(self.buf.u32(off)? as usize).ok_or(Short)? + 4;
                let elem = start.checked_add(index.checked_mul(4).ok_or(Short)?).ok_or(Short)?;
                let pos = elem.checked_add(self.buf.u32(elem)? as usize).ok_or(Short)?;
                Ok(Some(Table { buf: self.buf, pos }))
            }
        }
    }
}

/// Builds FlatBuffers for tests (tables are written back to front;
/// only the reader's view matters).
#[cfg(test)]
pub mod build {
    /// A field value inside a test table.
    #[derive(Debug, Clone)]
    pub enum Value {
        F32(f32),
        U8(u8),
        I8(i8),
        Table(Vec<(u16, Value)>),
        Tables(Vec<Vec<(u16, Value)>>),
    }

    /// Serialise a root table with file identifier `ident`.
    pub fn buffer(ident: &[u8; 4], root: &[(u16, Value)]) -> Vec<u8> {
        let mut out = vec![0u8; 8];
        out[4..8].copy_from_slice(ident);
        let root_pos = table(&mut out, root);
        out[0..4].copy_from_slice(&(root_pos as u32).to_le_bytes());
        out
    }

    fn align(out: &mut Vec<u8>) {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    /// Append a table (vtable first, then the table) and return the table position.
    fn table(out: &mut Vec<u8>, fields: &[(u16, Value)]) -> usize {
        // Lay out scalars inline after a 4-byte soffset; children are appended later and patched.
        let max_slot = fields.iter().map(|(s, _)| *s).max().unwrap_or(2);
        let vt_len = (max_slot + 2) as usize;
        align(out);
        let vtable = out.len();
        out.resize(vtable + vt_len, 0);
        out[vtable..vtable + 2].copy_from_slice(&(vt_len as u16).to_le_bytes());
        align(out);
        let pos = out.len();
        out.extend_from_slice(&((pos - vtable) as i32).to_le_bytes());
        let mut pending = Vec::new();
        for (slot, value) in fields {
            align(out);
            let field_off = out.len() - pos;
            out[vtable + *slot as usize..vtable + *slot as usize + 2]
                .copy_from_slice(&(field_off as u16).to_le_bytes());
            match value {
                Value::F32(v) => out.extend_from_slice(&v.to_le_bytes()),
                Value::U8(v) => out.push(*v),
                Value::I8(v) => out.push(*v as u8),
                Value::Table(_) | Value::Tables(_) => {
                    pending.push((out.len(), value.clone()));
                    out.extend_from_slice(&[0; 4]);
                }
            }
        }
        for (at, value) in pending {
            align(out);
            match value {
                Value::Table(children) => {
                    let child = table(out, &children);
                    out[at..at + 4].copy_from_slice(&((child - at) as u32).to_le_bytes());
                }
                Value::Tables(items) => {
                    align(out);
                    let vec = out.len();
                    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                    let slots = out.len();
                    out.resize(slots + 4 * items.len(), 0);
                    out[at..at + 4].copy_from_slice(&((vec - at) as u32).to_le_bytes());
                    for (i, item) in items.iter().enumerate() {
                        let child = table(out, item);
                        let elem = slots + 4 * i;
                        out[elem..elem + 4].copy_from_slice(&((child - elem) as u32).to_le_bytes());
                    }
                }
                _ => unreachable!(),
            }
        }
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::build::{buffer, Value};
    use super::*;

    #[test]
    fn reads_scalars_tables_and_vectors() {
        let data = buffer(
            b"KKFB",
            &[
                (4, Value::F32(1.5)),
                (6, Value::Table(vec![(4, Value::F32(-2.0)), (14, Value::I8(-3))])),
                (8, Value::Tables(vec![vec![(4, Value::U8(6))], vec![(4, Value::U8(4))]])),
            ],
        );
        assert!(Table::has_identifier(&data, b"KKFB"));
        let root = Table::root(&data).unwrap();
        assert_eq!(root.f32(4, 0.0), Ok(1.5));
        assert_eq!(root.f32(30, 0.0), Ok(0.0), "slot beyond the vtable reads the default");
        let child = root.table(6).unwrap().unwrap();
        assert_eq!(child.f32(4, 0.0), Ok(-2.0));
        assert_eq!(child.i8(14), Ok(-3));
        assert_eq!(root.vector_len(8), Ok(2));
        assert_eq!(root.vector_table(8, 1).unwrap().unwrap().u8(4), Ok(4));
        assert!(root.table(10).unwrap().is_none());
    }

    #[test]
    fn out_of_range_reads_fail() {
        assert!(Table::root(&[1, 0]).is_err());
        let bogus = [200u8, 0, 0, 0, b'K', b'K', b'F', b'B'];
        assert!(Table::root(&bogus).unwrap().f32(4, 0.0).is_err());
    }
}
