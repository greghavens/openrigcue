//! Keep the API's Hyper framing while returning completed requests to the shared parser.

use axum::body::HttpBody;
use axum::http::{header, HeaderMap, HeaderValue, Version};
use axum::response::Response;

use super::NativeHeaderPolicy;

fn has_token(headers: &HeaderMap, token: &str) -> bool {
    headers.get_all(header::CONNECTION).iter().any(|value| {
        value.to_str().is_ok_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case(token)))
    })
}

pub(super) struct RequestPolicy {
    version: Version,
    keep_alive: bool,
    connect: bool,
}

impl RequestPolicy {
    pub(super) fn new(request: &hyper::Request<hyper::body::Incoming>, conflicting_framing: bool) -> Self {
        let headers = request.headers();
        Self {
            version: request.version(),
            keep_alive: !conflicting_framing
                && !has_token(headers, "close")
                && (request.version() == Version::HTTP_11 || has_token(headers, "keep-alive"))
                && !(headers.contains_key(header::TRANSFER_ENCODING) && headers.contains_key(header::CONTENT_LENGTH)),
            connect: request.method() == axum::http::Method::CONNECT,
        }
    }

    pub(super) fn response_policy(&self, response: &Response) -> NativeHeaderPolicy {
        let close = has_token(response.headers(), "close");
        let explicit_keep_alive = has_token(response.headers(), "keep-alive");
        let mut keep_alive = self.keep_alive && !close;
        let mut connection: Vec<_> = response.headers().get_all(header::CONNECTION).iter().cloned().collect();
        if self.version == Version::HTTP_10 {
            if response.version() == Version::HTTP_10 && !explicit_keep_alive {
                keep_alive = false;
            } else if self.keep_alive && !explicit_keep_alive {
                connection.push(HeaderValue::from_static("keep-alive"));
            }
            if response.body().size_hint().exact().is_none() && !response.headers().contains_key(header::CONTENT_LENGTH)
            {
                keep_alive = false;
            }
        } else if !self.keep_alive && !close {
            connection.push(HeaderValue::from_static("close"));
        }
        let connection = (!connection.is_empty()).then(|| {
            let mut joined = Vec::new();
            for value in connection {
                if !joined.is_empty() {
                    joined.extend_from_slice(b", ");
                }
                joined.extend_from_slice(value.as_bytes());
            }
            HeaderValue::from_bytes(&joined).expect("joined validated Connection values")
        });
        NativeHeaderPolicy {
            api: true,
            upgraded: response.status() == axum::http::StatusCode::SWITCHING_PROTOCOLS
                || (self.connect && response.status().is_success()),
            connection,
            keep_alive,
            ..Default::default()
        }
    }
}

/// Count only bytes actually supplied to Hyper. Its returned read buffer is
/// excluded when deciding whether an ignored/partially read body was drained.
#[derive(Default)]
pub(super) struct InputProgress {
    read: u64,
    header_end: Option<u64>,
    line: Vec<u8>,
    pending: Vec<u8>,
    body: Option<BodyProgress>,
    transfer_encoding: bool,
    content_length: bool,
}

impl InputProgress {
    pub(super) fn record(&mut self, bytes: &[u8]) {
        let mut body_start = 0;
        if self.header_end.is_none() {
            body_start = bytes.len();
            for (index, byte) in bytes.iter().enumerate() {
                if *byte == b'\n' {
                    let empty = self.line.is_empty() || self.line == b"\r";
                    if let Some(colon) = self.line.iter().position(|byte| *byte == b':') {
                        let name = &self.line[..colon];
                        self.transfer_encoding |= name.eq_ignore_ascii_case(b"transfer-encoding");
                        self.content_length |= name.eq_ignore_ascii_case(b"content-length");
                    }
                    self.line.clear();
                    if empty {
                        self.header_end = Some(self.read + index as u64 + 1);
                        body_start = index + 1;
                        break;
                    }
                } else {
                    self.line.push(*byte);
                }
            }
        }
        self.read += bytes.len() as u64;
        match &mut self.body {
            Some(body) => body.record(&bytes[body_start..]),
            None => self.pending.extend_from_slice(&bytes[body_start..]),
        }
    }

    pub(super) fn initialize(&mut self, length: Option<u64>) {
        let mut body = BodyProgress::new(length);
        body.record(&std::mem::take(&mut self.pending));
        self.body = Some(body);
    }

    pub(super) fn conflicting_framing(&self) -> bool {
        self.transfer_encoding && self.content_length
    }

    pub(super) fn drained(&self, unread: usize) -> bool {
        self.header_end
            .zip(self.body.as_ref())
            .is_some_and(|(head, body)| body.complete() && self.read.saturating_sub(unread as u64) >= head + body.read)
    }
}

enum Phase {
    Length(u64),
    ChunkSize,
    ChunkData(u64),
    ChunkEnd(u8),
    Trailers,
    Complete,
    Invalid,
}

struct BodyProgress {
    phase: Phase,
    read: u64,
    line: Vec<u8>,
    trailer_lines: usize,
}

impl BodyProgress {
    fn new(length: Option<u64>) -> Self {
        Self {
            phase: match length {
                Some(0) => Phase::Complete,
                Some(length) => Phase::Length(length),
                None => Phase::ChunkSize,
            },
            read: 0,
            line: Vec::new(),
            trailer_lines: 0,
        }
    }

    fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    fn record(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match &mut self.phase {
                Phase::Complete | Phase::Invalid => break,
                Phase::Length(remaining) | Phase::ChunkData(remaining) => {
                    let count = (*remaining).min(bytes.len() as u64) as usize;
                    *remaining -= count as u64;
                    self.read += count as u64;
                    bytes = &bytes[count..];
                    if *remaining == 0 {
                        self.phase =
                            if matches!(self.phase, Phase::Length(_)) { Phase::Complete } else { Phase::ChunkEnd(0) };
                    }
                }
                Phase::ChunkEnd(offset) => {
                    self.read += 1;
                    if bytes[0] != b"\r\n"[usize::from(*offset)] {
                        self.phase = Phase::Invalid;
                    } else if *offset == 1 {
                        self.phase = Phase::ChunkSize;
                    } else {
                        *offset = 1;
                    }
                    bytes = &bytes[1..];
                }
                Phase::ChunkSize | Phase::Trailers => {
                    let count = bytes.iter().position(|byte| *byte == b'\n').map_or(bytes.len(), |index| index + 1);
                    let control = &bytes[..count];
                    // Hyper accepts arbitrarily many leading zeroes in a chunk
                    // size. They do not need to be retained to find its boundary.
                    let start = if matches!(self.phase, Phase::ChunkSize) && self.line.is_empty() {
                        control.iter().position(|byte| *byte != b'0').unwrap_or(control.len())
                    } else {
                        0
                    };
                    self.line.extend_from_slice(&control[start..]);
                    self.read += count as u64;
                    bytes = &bytes[count..];
                    // Hyper bounds chunk/trailer control lines; no body payload is buffered here.
                    if self.line.len() > 8192 + 4096 * 100 {
                        self.phase = Phase::Invalid;
                    } else if self.line.ends_with(b"\n") {
                        if !self.line.ends_with(b"\r\n") {
                            if matches!(self.phase, Phase::ChunkSize) {
                                self.phase = Phase::Invalid;
                            }
                            continue;
                        }
                        if matches!(self.phase, Phase::Trailers) {
                            let start = self.line[..self.line.len() - 2]
                                .windows(2)
                                .rposition(|pair| pair == b"\r\n")
                                .map_or(0, |index| index + 2);
                            if self.line[start..] == *b"\r\n" {
                                let mut headers = [httparse::EMPTY_HEADER; 100];
                                let valid = match httparse::parse_headers(&self.line, &mut headers) {
                                    Ok(httparse::Status::Complete((_, headers))) => {
                                        headers.len() <= self.trailer_lines
                                            && headers.iter().all(|field| {
                                                axum::http::HeaderName::from_bytes(field.name.as_bytes()).is_ok()
                                                    && HeaderValue::from_bytes(field.value).is_ok()
                                            })
                                    }
                                    _ => false,
                                };
                                self.phase = if valid { Phase::Complete } else { Phase::Invalid };
                                self.line.clear();
                            } else {
                                self.trailer_lines += 1;
                                if self.trailer_lines > 100 {
                                    self.phase = Phase::Invalid;
                                }
                            }
                        } else {
                            self.phase = match httparse::parse_chunk_size(&self.line) {
                                Ok(httparse::Status::Complete((_, 0))) => Phase::Trailers,
                                Ok(httparse::Status::Complete((_, length))) => Phase::ChunkData(length),
                                _ => Phase::Invalid,
                            };
                            self.line.clear();
                        }
                    }
                }
            }
        }
    }
}
