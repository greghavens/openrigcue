//! Strings of Unicode code points, including the surrogate values that Rust `str` excludes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PythonString(Vec<u32>);

impl PythonString {
    pub fn from_codepoints(points: Vec<u32>) -> Result<Self, String> {
        if points.iter().any(|&point| point > 0x10ffff) {
            return Err("ValueError: chr() arg not in range(0x110000)".into());
        }
        Ok(Self(points))
    }
    pub fn codepoints(&self) -> &[u32] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn scalar_string(&self) -> Option<String> {
        self.0.iter().copied().map(char::from_u32).collect()
    }
    pub fn from_wtf8(bytes: &[u8]) -> Result<Self, std::str::Utf8Error> {
        let mut points = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            match std::str::from_utf8(&bytes[index..]) {
                Ok(text) => {
                    points.extend(text.chars().map(u32::from));
                    break;
                }
                Err(error) => {
                    let count = error.valid_up_to();
                    points.extend(
                        std::str::from_utf8(&bytes[index..index + count]).expect("valid prefix").chars().map(u32::from),
                    );
                    index += count;
                    if bytes.get(index) == Some(&0xed)
                        && bytes.get(index + 1).is_some_and(|b| (0xa0..=0xbf).contains(b))
                        && bytes.get(index + 2).is_some_and(|b| (0x80..=0xbf).contains(b))
                    {
                        points.push(
                            ((u32::from(bytes[index] & 15)) << 12)
                                | ((u32::from(bytes[index + 1] & 63)) << 6)
                                | u32::from(bytes[index + 2] & 63),
                        );
                        index += 3;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        Ok(Self(points))
    }
    pub fn to_wtf8(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for &point in &self.0 {
            if let Some(c) = char::from_u32(point) {
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            } else {
                out.extend_from_slice(&[
                    0xe0 | ((point >> 12) as u8),
                    0x80 | (((point >> 6) & 63) as u8),
                    0x80 | ((point & 63) as u8),
                ]);
            }
        }
        out
    }
    pub fn repr(&self) -> String {
        let quote = if self.0.contains(&39) && !self.0.contains(&34) { '"' } else { '\'' };
        let mut out = String::from(quote);
        for &point in &self.0 {
            if let Some(c) = char::from_u32(point) {
                let repr = crate::python_repr::repr_str(&c.to_string());
                if c == quote || c == '\\' {
                    out.push('\\');
                    out.push(c);
                } else if c == '\'' || c == '"' {
                    out.push(c);
                } else {
                    out.push_str(&repr[1..repr.len() - 1]);
                }
            } else {
                out.push_str(&format!("\\u{point:04x}"));
            }
        }
        out.push(quote);
        out
    }
    pub fn json_escaped(&self) -> String {
        let mut out = String::from("\"");
        for &point in &self.0 {
            if let Some(c) = char::from_u32(point) {
                let encoded = serde_json::to_string(&c.to_string()).expect("scalar string");
                out.push_str(&encoded[1..encoded.len() - 1]);
            } else {
                out.push_str(&format!("\\u{point:04x}"));
            }
        }
        out.push('"');
        out
    }
}
impl From<&str> for PythonString {
    fn from(value: &str) -> Self {
        Self(value.chars().map(u32::from).collect())
    }
}

impl PartialEq<&str> for PythonString {
    fn eq(&self, other: &&str) -> bool {
        self.codepoints().iter().copied().eq(other.chars().map(u32::from))
    }
}
impl PartialEq<str> for PythonString {
    fn eq(&self, other: &str) -> bool {
        self.codepoints().iter().copied().eq(other.chars().map(u32::from))
    }
}
impl serde::Serialize for PythonString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let json = serde_json::value::RawValue::from_string(self.json_escaped()).map_err(serde::ser::Error::custom)?;
        json.serialize(serializer)
    }
}
