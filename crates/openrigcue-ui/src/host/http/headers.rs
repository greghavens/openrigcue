//! Request header lookup for the UI handler, with lenient (email-style) header parsing.
//! MIME payloads and defect reports are not consumed by the UI.

use std::sync::Arc;

#[derive(Clone, Default)]
pub(super) struct HeaderMessage {
    fields: Arc<Vec<Field>>,
}

struct Field {
    name: Vec<u8>,
    value: Vec<u8>,
}

fn lines(raw: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < raw.len() {
        let Some(index) = raw[start..].iter().position(|byte| matches!(byte, b'\r' | b'\n')) else {
            lines.push(&raw[start..]);
            break;
        };
        let mut end = start + index + 1;
        if raw[end - 1] == b'\r' && raw.get(end) == Some(&b'\n') {
            end += 1;
        }
        lines.push(&raw[start..end]);
        start = end;
    }
    lines
}

fn header_line(line: &[u8]) -> bool {
    if line.starts_with(b"From ") || matches!(line.first(), Some(b' ' | b'\t')) {
        return true;
    }
    line.iter()
        .position(|byte| *byte == b':')
        .is_some_and(|end| line[..end].iter().all(|byte| matches!(*byte, 0x21..=0x39 | 0x3b..=0x7e)))
}

fn finish(fields: &mut Vec<Field>, current: &mut Option<Field>) {
    let Some(mut field) = current.take() else { return };
    let start =
        field.value.iter().position(|byte| !matches!(*byte, b' ' | b'\t' | b'\r' | b'\n')).unwrap_or(field.value.len());
    let value = &field.value[start..];
    let end = value.iter().rposition(|byte| !matches!(*byte, b'\r' | b'\n')).map_or(0, |index| index + 1);
    field.value = value[..end].to_vec();
    fields.push(field);
}

impl HeaderMessage {
    pub(super) fn parse(raw: &[u8]) -> Self {
        let lines: Vec<_> = lines(raw).into_iter().take_while(|line| header_line(line)).collect();
        let mut fields = Vec::new();
        let mut current: Option<Field> = None;
        for (index, line) in lines.iter().enumerate() {
            if matches!(line.first(), Some(b' ' | b'\t')) {
                if let Some(field) = &mut current {
                    field.value.extend_from_slice(line);
                }
                continue;
            }
            finish(&mut fields, &mut current);
            if line.starts_with(b"From ") {
                if index != 0 && index == lines.len() - 1 {
                    break;
                }
                continue;
            }
            let colon = line.iter().position(|byte| *byte == b':').unwrap();
            if colon == 0 {
                continue;
            }
            current = Some(Field { name: line[..colon].to_vec(), value: line[colon + 1..].to_vec() });
        }
        finish(&mut fields, &mut current);
        Self { fields: Arc::new(fields) }
    }

    pub(super) fn get(&self, name: &str) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case(name.as_bytes()))
            .map(|field| field.value.as_slice())
    }

    pub(super) fn project(&self, target: &mut axum::http::HeaderMap) {
        use axum::http::{HeaderName, HeaderValue};
        target.clear();
        for field in self.fields.iter() {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(&field.name), HeaderValue::from_bytes(&field.value))
            {
                if target.try_append(name, value).is_err() {
                    break;
                }
            }
        }
    }
}
