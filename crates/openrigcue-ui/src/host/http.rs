//! HTTP transport for the shared UI/API server. A UI request can end
//! without an HTTP response being written; carry that outcome through the router.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::SystemTime;

use axum::body::HttpBody;
use axum::http::{header, HeaderName, HeaderValue, StatusCode, Version};
use axum::response::{IntoResponse, Response};
use axum::serve::Listener;
use axum::Router;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tower::ServiceExt;

mod api;
mod headers;
use headers::HeaderMessage;

pub(super) fn request_header<'a>(parts: &'a axum::http::request::Parts, name: &str) -> Option<&'a [u8]> {
    match parts.extensions.get::<HeaderMessage>() {
        Some(message) => message.get(name),
        None => parts.headers.get(name).map(HeaderValue::as_bytes),
    }
}

#[derive(Clone, Copy)]
pub(super) struct CloseWithoutResponse;

// The Server header value of the UI's HTTP responses.
pub(super) const NATIVE_SERVER: &str = "BaseHTTP/0.6 Python/3.13.13";

#[derive(Clone)]
struct NativeReason(String);

/// The request target as received, before the framework's URI interpretation.
#[derive(Clone)]
pub(super) struct NativeTarget(pub(super) String);

/// The HTML error response, for the pre-dispatch errors below.
fn native_error_response(status: StatusCode, message: &str, explain: &str) -> Response {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        concat!(
            "<!DOCTYPE HTML>\n",
            "<html lang=\"en\">\n",
            "    <head>\n",
            "        <meta charset=\"utf-8\">\n",
            "        <title>Error response</title>\n",
            "    </head>\n",
            "    <body>\n",
            "        <h1>Error response</h1>\n",
            "        <p>Error code: {code}</p>\n",
            "        <p>Message: {message}.</p>\n",
            "        <p>Error code explanation: {code} - {explain}.</p>\n",
            "    </body>\n",
            "</html>\n",
        ),
        code = status.as_u16(),
        message = escape(message),
        explain = escape(explain),
    );
    let length = body.len().to_string();
    let mut response = native_headers(
        (status, body).into_response(),
        [
            ("Connection".into(), "close".into()),
            ("Content-Type".into(), "text/html;charset=utf-8".into()),
            ("Content-Length".into(), length),
        ],
    );
    response.extensions_mut().insert(NativeReason(message.into()));
    native_response(response)
}

fn native_method_response(method: &str) -> Option<Response> {
    match method {
        "AUTHHEAD" => Some(native_response(native_headers(
            StatusCode::UNAUTHORIZED.into_response(),
            [
                ("WWW-Authenticate".into(), "Basic realm=\"Protected\"".into()),
                ("Content-type".into(), "text/html".into()),
            ],
        ))),
        // These resolve as methods, then fail before an HTTP write: the
        // fresh request lacks update_lock, or the callback arguments.
        "gui_update" | "not_show_again" => Some(close_without_response()),
        // POST upload is omitted with licensing.
        "GET" | "HEAD" | "POST" => None,
        _ => Some(native_error_response(
            StatusCode::NOT_IMPLEMENTED,
            &format!("Unsupported method ({})", openrigcue_model::python_repr::repr_str(method)),
            "Server does not support this operation",
        )),
    }
}

#[derive(Clone)]
struct NativeHeaders(Vec<(Vec<u8>, Vec<u8>)>);

#[derive(Clone, Copy)]
struct NativeKeepAlive(bool);

fn connection_keep_alive(current: bool, value: &[u8]) -> bool {
    if value.eq_ignore_ascii_case(b"close") {
        false
    } else if value.eq_ignore_ascii_case(b"keep-alive") {
        true
    } else {
        current
    }
}

fn response_keep_alive(response: &Response) -> bool {
    response.extensions().get::<NativeKeepAlive>().map_or_else(
        || {
            response
                .headers()
                .get_all(header::CONNECTION)
                .iter()
                .fold(false, |state, value| connection_keep_alive(state, value.as_bytes()))
        },
        |state| state.0,
    )
}

#[derive(Clone, Copy)]
struct HeaderEncodingFailure;

/// Keep each header's spelling and the order they were sent in before HeaderMap
/// normalizes names. The body conversion may have supplied extra headers.
pub(super) fn native_headers(mut response: Response, headers: impl IntoIterator<Item = (String, String)>) -> Response {
    let headers: Vec<_> = headers.into_iter().collect();
    // HTTP/0.9 skips header encoding but still applies every Connection call's state change.
    let keep_alive = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .fold(false, |state, (_, value)| connection_keep_alive(state, value.as_bytes()));
    response.extensions_mut().insert(NativeKeepAlive(keep_alive));
    response.headers_mut().clear();
    let mut ordered = Vec::new();
    for (name, value) in headers {
        // Headers are buffered as Latin-1 with strict encoding. A failed value
        // prevents the flush of even the earlier fields.
        let encode = |text: &str| text.chars().map(|ch| u8::try_from(u32::from(ch)).ok()).collect::<Option<Vec<_>>>();
        let (Some(encoded_name), Some(encoded_value)) = (encode(&name), encode(&value)) else {
            // HTTP/0.9 skips header encoding entirely. Keep the body
            // until the connection's request-version state is available.
            response.extensions_mut().insert(HeaderEncodingFailure);
            response.extensions_mut().insert(NativeHeaders(ordered));
            return response;
        };
        if let (Ok(key), Ok(value)) = (HeaderName::from_bytes(&encoded_name), HeaderValue::from_bytes(&encoded_value)) {
            response.headers_mut().insert(key, value);
        }
        ordered.push((encoded_name, encoded_value));
    }
    response.extensions_mut().insert(NativeHeaders(ordered));
    response
}

#[derive(Clone)]
struct NativeFraming {
    length: Option<HeaderValue>,
    transfer: Option<HeaderValue>,
    connection: Option<HeaderValue>,
    websocket_accept: Option<HeaderValue>,
    ordinary: Option<Vec<u8>>,
    keep_alive: bool,
}

/// Remember headers supplied by the UI handler before Axum adds a body length.
pub(super) fn native_response(mut response: Response) -> Response {
    let ordinary = (response.status() != StatusCode::SWITCHING_PROTOCOLS).then(|| {
        let mut wire = format!(
            "HTTP/1.0 {} {}\r\nServer: {NATIVE_SERVER}\r\nDate: {}\r\n",
            response.status().as_u16(),
            response
                .extensions()
                .get::<NativeReason>()
                .map_or_else(|| response.status().canonical_reason().unwrap_or_default(), |reason| reason.0.as_str(),),
            httpdate::fmt_http_date(SystemTime::now()),
        )
        .chars()
        .map(|ch| u8::try_from(u32::from(ch)).expect("native status reason is Latin-1"))
        .collect::<Vec<_>>();
        let headers = response.extensions().get::<NativeHeaders>().cloned().unwrap_or_else(|| {
            NativeHeaders(
                response
                    .headers()
                    .iter()
                    .map(|(name, value)| (name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()))
                    .collect(),
            )
        });
        for (name, value) in headers.0 {
            wire.extend_from_slice(&name);
            wire.extend_from_slice(b": ");
            wire.extend_from_slice(&value);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"\r\n");
        wire
    });
    let framing = NativeFraming {
        keep_alive: response_keep_alive(&response),
        length: response.headers().get(header::CONTENT_LENGTH).cloned(),
        transfer: response.headers().get(header::TRANSFER_ENCODING).cloned(),
        connection: response.headers().get(header::CONNECTION).cloned(),
        websocket_accept: (response.status() == StatusCode::SWITCHING_PROTOCOLS)
            .then(|| response.headers().get(header::SEC_WEBSOCKET_ACCEPT).cloned())
            .flatten(),
        ordinary,
    };
    response.extensions_mut().insert(framing);
    response
}

/// The request parser retains literal #/< in the target and collapses its
/// leading slashes. Escape those bytes before Hyper can discard/reject them;
/// the handler's unquote restores the target as received before classification.
fn native_request_target(line: &[u8]) -> (Vec<u8>, bool) {
    let Some(method_end) = line.iter().position(u8::is_ascii_whitespace) else { return (line.to_vec(), false) };
    let start = method_end + line[method_end..].iter().take_while(|byte| byte.is_ascii_whitespace()).count();
    let end = start + line[start..].iter().take_while(|byte| !byte.is_ascii_whitespace()).count();
    let target = &line[start..end];
    // The added diagnostic API keeps Hyper's normal persistent connections.
    if target.is_empty() || target.starts_with(b"/api/") {
        return (line.to_vec(), false);
    }
    let mut output = line[..start].to_vec();
    let target = if target.starts_with(b"//") {
        output.push(b'/');
        &target[target.iter().take_while(|byte| **byte == b'/').count()..]
    } else {
        target
    };
    for byte in target {
        match byte {
            b'#' => output.extend_from_slice(b"%23"),
            b'<' => output.extend_from_slice(b"%3C"),
            byte => output.push(*byte),
        }
    }
    output.extend_from_slice(&line[end..]);
    (output, true)
}

/// Whitespace splitting after ISO-8859-1 decoding, including the C0 separators,
/// NEL and no-break space which ASCII whitespace helpers do not all include.
fn native_request_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t'..=b'\r' | 0x1c..=0x20 | 0x85 | 0xa0)
}

fn native_version(version: &[u8]) -> Option<(u64, u64)> {
    let version = version.strip_prefix(b"HTTP/")?;
    let mut parts = version.split(|byte| *byte == b'.');
    let (Some(major), Some(minor), None) = (parts.next(), parts.next(), parts.next()) else { return None };
    if [major, minor].iter().any(|part| part.is_empty() || part.len() > 10 || !part.iter().all(u8::is_ascii_digit)) {
        return None;
    }
    let number = |part: &[u8]| part.iter().fold(0u64, |number, digit| number * 10 + u64::from(digit - b'0'));
    Some((number(major), number(minor)))
}

fn supported_native_version(version: &[u8]) -> bool {
    native_version(version).is_some_and(|(major, _)| major < 2)
}

/// The request line: the last word's version is checked before the word count, and the
/// request version is only assigned after rejecting 2.x+. The body-only error
/// state is independent of the unparsed method (including HEAD).
fn native_request_error(line: &[u8]) -> Option<(Response, bool)> {
    let fields: Vec<_> =
        line.split(|byte| native_request_whitespace(*byte)).filter(|field| !field.is_empty()).collect();
    if fields.is_empty() || fields.get(1).is_some_and(|target| target.starts_with(b"/api/")) {
        return None;
    }
    let text = |bytes: &[u8]| bytes.iter().copied().map(char::from).collect::<String>();
    let repr = |bytes: &[u8]| openrigcue_model::python_repr::repr_str(&text(bytes));
    let bad = |message: String, body_only| {
        Some((
            native_error_response(StatusCode::BAD_REQUEST, &message, "Bad request syntax or unsupported method"),
            body_only,
        ))
    };
    let mut body_only = true;
    if fields.len() >= 3 {
        let version = *fields.last().unwrap();
        let Some((major, _minor)) = native_version(version) else {
            return bad(format!("Bad request version ({})", repr(version)), true);
        };
        if major >= 2 {
            return Some((
                native_error_response(
                    StatusCode::HTTP_VERSION_NOT_SUPPORTED,
                    &format!("Invalid HTTP version ({})", text(&version[5..])),
                    "Cannot fulfill request",
                ),
                true,
            ));
        }
        body_only = version == b"HTTP/0.9";
    }
    if !(2..=3).contains(&fields.len()) {
        let requestline = text(line);
        return bad(
            format!(
                "Bad request syntax ({})",
                openrigcue_model::python_repr::repr_str(requestline.trim_end_matches(['\r', '\n']))
            ),
            body_only,
        );
    }
    if fields.len() == 2 && fields[0] != b"GET" {
        return bad(format!("Bad HTTP/0.9 request type ({})", repr(fields[0])), true);
    }
    None
}

async fn write_native_error<S: AsyncWrite + Unpin>(
    source: &mut S,
    mut response: Response,
    body_only: bool,
    send_body: bool,
) -> io::Result<()> {
    let header = response.extensions_mut().remove::<NativeFraming>().unwrap().ordinary.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.map_err(io::Error::other)?;
    if !body_only {
        source.write_all(&header).await?;
    }
    if send_body {
        source.write_all(&body).await?;
    }
    source.flush().await
}

enum HeaderRead {
    Ready(Vec<u8>),
    Rejected(Box<Response>),
}

/// The header reader counts every physical line read, including the
/// terminating blank/EOF line, before the later message parser sees syntax.
async fn read_native_headers<R: AsyncBufRead + Unpin>(source: &mut R) -> io::Result<HeaderRead> {
    let mut headers = Vec::new();
    for count in 1.. {
        let mut line = Vec::new();
        (&mut *source).take(65537).read_until(b'\n', &mut line).await?;
        let rejection = if line.len() > 65536 {
            Some(("Line too long", "got more than 65536 bytes when reading header line"))
        } else if count > 100 {
            Some(("Too many headers", "got more than 100 headers"))
        } else {
            None
        };
        if let Some((message, explain)) = rejection {
            return Ok(HeaderRead::Rejected(Box::new(native_error_response(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                message,
                explain,
            ))));
        }
        if line.is_empty() {
            if headers.last() == Some(&b'\r') {
                headers.push(b'\n');
            } else if !headers.is_empty() && !headers.ends_with(b"\n") {
                headers.extend_from_slice(b"\r\n");
            }
            headers.extend_from_slice(b"\r\n");
            return Ok(HeaderRead::Ready(headers));
        }
        let complete = line == b"\r\n" || line == b"\n";
        headers.extend_from_slice(&line);
        if complete {
            return Ok(HeaderRead::Ready(headers));
        }
    }
    unreachable!()
}

/// Normalize valid request forms for Hyper while keeping the
/// body-only rule separate from the version Hyper needs for parsing/upgrades.
fn native_request_line(line: &[u8]) -> (Vec<u8>, bool, bool, Option<String>, Option<NativeTarget>) {
    let fields: Vec<_> =
        line.split(|byte| native_request_whitespace(*byte)).filter(|field| !field.is_empty()).collect();
    let short_get = fields.len() == 2 && fields[0] == b"GET";
    let versioned = fields.len() == 3 && supported_native_version(fields[2]);
    if (short_get || versioned) && !fields[1].starts_with(b"/api/") {
        // Any method string is accepted for the handler lookup. Carry
        // non-routing methods beside a valid placeholder so Hyper cannot
        // reject their token or an unused target before header parsing.
        let original_method = (!matches!(fields[0], b"GET" | b"HEAD" | b"POST"))
            .then(|| fields[0].iter().copied().map(char::from).collect::<String>());
        let original_target = original_method.is_none().then(|| {
            let target: String = fields[1].iter().copied().map(char::from).collect();
            NativeTarget(if target.starts_with("//") { format!("/{}", target.trim_start_matches('/')) } else { target })
        });
        let method = if original_method.is_some() { b"GET".as_slice() } else { fields[0] };
        let target = if original_method.is_some() { b"/".as_slice() } else { fields[1] };
        let mut normalized = method.to_vec();
        normalized.push(b' ');
        for byte in target {
            if byte.is_ascii() {
                normalized.push(*byte);
            } else {
                // Each raw byte of the request line is decoded as Latin-1. Carry that
                // character through the URI parser until the handler's unquote.
                let mut utf8 = [0; 2];
                for encoded in char::from(*byte).encode_utf8(&mut utf8).bytes() {
                    let hex = b"0123456789ABCDEF";
                    normalized.extend_from_slice(&[
                        b'%',
                        hex[usize::from(encoded >> 4)],
                        hex[usize::from(encoded & 15)],
                    ]);
                }
            }
        }
        normalized.extend_from_slice(b" HTTP/1.1\r\n");
        if short_get {
            // HTTP/0.9's two-word form never consumes request headers.
            normalized.extend_from_slice(b"\r\n");
        }
        let (mut line, native) = native_request_target(&normalized);
        let target_start = method.len() + 1;
        let target_end = target_start + line[target_start..].iter().position(u8::is_ascii_whitespace).unwrap();
        let uri_supported =
            axum::http::Uri::try_from(&line[target_start..target_end]).is_ok_and(|uri| uri.path().starts_with('/'));
        if !uri_supported && original_target.is_some() {
            // The UI handler accepts targets outside Uri's syntax/length constraints.
            // Route to its root and let the handler classify the saved path.
            line = method.to_vec();
            line.extend_from_slice(b" / HTTP/1.1\r\n");
            if short_get {
                line.extend_from_slice(b"\r\n");
            }
        }
        (line, native, short_get || fields[2] == b"HTTP/0.9", original_method, original_target)
    } else {
        let (line, native) = native_request_target(line);
        (line, native, false, None, None)
    }
}

/// Bytes buffered after a response must precede the next transport read.
struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    offset: usize,
}

impl<S> PrefixedStream<S> {
    fn new(inner: S) -> Self {
        Self { inner, prefix: Vec::new(), offset: 0 }
    }

    fn prepend(&mut self, mut bytes: Vec<u8>) {
        bytes.extend_from_slice(&self.prefix[self.offset..]);
        self.prefix = bytes;
        self.offset = 0;
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, output: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let stream = self.get_mut();
        if stream.offset < stream.prefix.len() && output.remaining() != 0 {
            let count = (stream.prefix.len() - stream.offset).min(output.remaining());
            output.put_slice(&stream.prefix[stream.offset..stream.offset + count]);
            stream.offset += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut stream.inner).poll_read(cx, output)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bytes)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

struct RequestStream<S> {
    source: BufReader<S>,
    line: Vec<u8>,
    offset: usize,
    output: Option<NativeOutput>,
    api_input: Option<Arc<Mutex<api::InputProgress>>>,
    retain_transport: bool,
}

/// Restore the handler's own header block after Hyper's normalization,
/// then pass body/socket bytes through unchanged. API replies restore only
/// the Connection header used by the standard persistent transport.
#[derive(Default)]
struct NativeOutput {
    policy: Arc<Mutex<NativeHeaderPolicy>>,
    header: Vec<u8>,
    pending: Vec<u8>,
    offset: usize,
    complete: bool,
    headerless: bool,
    api: bool,
}

#[derive(Clone, Default)]
struct NativeHeaderPolicy {
    connection: Option<HeaderValue>,
    websocket_accept: Option<HeaderValue>,
    ordinary: Option<Vec<u8>>,
    keep_alive: bool,
    api: bool,
    upgraded: bool,
}

impl NativeOutput {
    fn finish_header(&mut self) {
        let policy = self.policy.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let status = self.header.split(|byte| *byte == b'\n').next().unwrap_or_default();
        let code = status.split(u8::is_ascii_whitespace).nth(1).unwrap_or_default();
        self.complete = !code.starts_with(b"1") || code == b"101";
        // An interim response precedes the handler's final header decision.
        if !self.complete {
            self.pending = std::mem::take(&mut self.header);
            return;
        }
        if self.api && !policy.api {
            // Hyper can reject an API request before invoking the service.
            self.pending = std::mem::take(&mut self.header);
            return;
        }
        if code == b"101" {
            if let Some(accept) = policy.websocket_accept {
                // The websocket handler writes this header block directly,
                // without the Server/Date headers.
                self.pending.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
                self.pending.extend_from_slice(accept.as_bytes());
                self.pending.extend_from_slice(b"\r\n\r\n");
                self.header.clear();
                return;
            }
        }
        if self.headerless {
            self.header.clear();
            return;
        }
        if let Some(ordinary) = policy.ordinary {
            self.pending = ordinary;
            self.header.clear();
            return;
        }
        let mut written = false;
        for line in self.header.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                break;
            }
            let name_end = line.iter().position(|byte| *byte == b':');
            if name_end.is_some_and(|end| line[..end].eq_ignore_ascii_case(b"connection")) {
                if !written {
                    if let Some(value) = &policy.connection {
                        self.pending.extend_from_slice(&line[..name_end.unwrap()]);
                        self.pending.extend_from_slice(b": ");
                        self.pending.extend_from_slice(value.as_bytes());
                        self.pending.extend_from_slice(b"\r\n");
                    }
                    written = true;
                }
            } else {
                self.pending.extend_from_slice(line);
                self.pending.extend_from_slice(b"\r\n");
            }
        }
        if !written {
            if let Some(value) = policy.connection {
                self.pending.extend_from_slice(b"connection: ");
                self.pending.extend_from_slice(value.as_bytes());
                self.pending.extend_from_slice(b"\r\n");
            }
        }
        self.pending.extend_from_slice(b"\r\n");
        self.header.clear();
    }

    fn poll_drain<S: AsyncWrite + Unpin>(&mut self, source: &mut S, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.offset < self.pending.len() {
            let count = std::task::ready!(Pin::new(&mut *source).poll_write(context, &self.pending[self.offset..]))?;
            if count == 0 {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::WriteZero, "HTTP header write returned zero")));
            }
            self.offset += count;
        }
        self.pending.clear();
        self.offset = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for RequestStream<S> {
    fn poll_read(self: Pin<&mut Self>, context: &mut Context<'_>, output: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.offset < this.line.len() {
            let count = output.remaining().min(this.line.len() - this.offset);
            if let Some(progress) = &this.api_input {
                progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record(&this.line[this.offset..this.offset + count]);
            }
            output.put_slice(&this.line[this.offset..this.offset + count]);
            this.offset += count;
            Poll::Ready(Ok(()))
        } else {
            let before = output.filled().len();
            let result = Pin::new(&mut this.source).poll_read(context, output);
            if matches!(result, Poll::Ready(Ok(()))) {
                if let Some(progress) = &this.api_input {
                    progress
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .record(&output.filled()[before..]);
                }
            }
            result
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RequestStream<S> {
    fn poll_write(self: Pin<&mut Self>, context: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(output) = &mut this.output {
            std::task::ready!(output.poll_drain(&mut this.source, context))?;
            if !output.complete {
                for (index, byte) in bytes.iter().enumerate() {
                    output.header.push(*byte);
                    if output.header.ends_with(b"\r\n\r\n") {
                        output.finish_header();
                        return Poll::Ready(Ok(index + 1));
                    }
                }
                return Poll::Ready(Ok(bytes.len()));
            }
        }
        Pin::new(&mut this.source).poll_write(context, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(output) = &mut this.output {
            std::task::ready!(output.poll_drain(&mut this.source, context))?;
        }
        Pin::new(&mut this.source).poll_flush(context)
    }
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(output) = &mut this.output {
            std::task::ready!(output.poll_drain(&mut this.source, context))?;
            if this.retain_transport
                && !output.policy.lock().unwrap_or_else(std::sync::PoisonError::into_inner).upgraded
            {
                // One Hyper exchange has ended; the outer parser owns the
                // next request. Actual upgraded sockets retain shutdown.
                return Pin::new(&mut this.source).poll_flush(context);
            }
        }
        Pin::new(&mut this.source).poll_shutdown(context)
    }
}

pub(super) fn close_without_response() -> Response {
    let mut response = ().into_response();
    response.extensions_mut().insert(CloseWithoutResponse);
    response
}

pub(super) async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    router: Router,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    tokio::pin!(shutdown);
    let mut source = BufReader::new(PrefixedStream::new(stream));
    loop {
        let mut line = Vec::new();
        let count = {
            // The request line read is bounded at 65537 bytes.
            let mut bounded = (&mut source).take(65537);
            tokio::select! {
                result = bounded.read_until(b'\n', &mut line) => result?,
                () = &mut shutdown => return Ok(()),
            }
        };
        if count == 0 {
            return Ok(());
        }
        if count > 65536 {
            // The method is cleared before the error response, so this body is
            // written even when the unparsed request line starts with HEAD.
            return write_native_error(
                &mut source,
                native_error_response(StatusCode::URI_TOO_LONG, "URI Too Long", "URI is too long"),
                false,
                true,
            )
            .await;
        }
        if line.iter().all(|byte| native_request_whitespace(*byte)) {
            return Ok(());
        }
        if let Some((response, body_only)) = native_request_error(&line) {
            return write_native_error(&mut source, response, body_only, true).await;
        }
        let fields: Vec<_> =
            line.split(|byte| native_request_whitespace(*byte)).filter(|field| !field.is_empty()).collect();
        let read_headers = fields.len() == 3 && !fields[1].starts_with(b"/api/");
        let (mut line, native, headerless, original_method, original_target) = native_request_line(&line);
        let mut original_headers = native.then(HeaderMessage::default);
        if native && read_headers {
            let headers = tokio::select! {
                result = read_native_headers(&mut source) => result?,
                () = &mut shutdown => return Ok(()),
            };
            match headers {
                HeaderRead::Ready(headers) => {
                    let message = HeaderMessage::parse(&headers);
                    let upgrade = original_method.is_none()
                        && line.starts_with(b"GET ")
                        && message.get("upgrade").is_some_and(|value| value.eq_ignore_ascii_case(b"websocket"));
                    // The UI handlers do not consume GET/HEAD request bodies;
                    // keep their header values separately from Hyper framing.
                    if upgrade {
                        line.extend_from_slice(b"Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n");
                    } else {
                        line.extend_from_slice(b"\r\n");
                    }
                    original_headers = Some(message);
                }
                HeaderRead::Rejected(response) => {
                    return write_native_error(&mut source, *response, headerless, !line.starts_with(b"HEAD ")).await;
                }
            }
        }
        let response_headers = Arc::new(Mutex::new(NativeHeaderPolicy::default()));
        let api_input = (!native).then(|| Arc::new(Mutex::new(api::InputProgress::default())));
        let stream = RequestStream {
            source,
            line,
            offset: 0,
            output: Some(NativeOutput {
                policy: response_headers.clone(),
                headerless,
                api: !native,
                ..Default::default()
            }),
            api_input: api_input.clone(),
            retain_transport: true,
        };
        let response_policy = response_headers.clone();
        let request_router = router.clone();
        let service = hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
            let router = request_router.clone();
            let response_headers = response_policy.clone();
            let original_method = original_method.clone();
            let original_target = original_target.clone();
            let original_headers = original_headers.clone();
            let api_policy = api_input.as_ref().map(|progress| {
                let mut progress = progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let policy = api::RequestPolicy::new(&request, progress.conflicting_framing());
                progress.initialize(request.body().size_hint().exact());
                policy
            });
            Box::pin(async move {
                if let Some(target) = original_target {
                    request.extensions_mut().insert(target);
                }
                if let Some(headers) = original_headers {
                    headers.project(request.headers_mut());
                    request.extensions_mut().insert(headers);
                }
                let method_response = native
                    .then(|| native_method_response(original_method.as_deref().unwrap_or(request.method().as_str())))
                    .flatten();
                let mut response = match method_response {
                    Some(response) => response,
                    None => router.oneshot(request).await.expect("router is infallible"),
                };
                if response.extensions().get::<CloseWithoutResponse>().is_some()
                    || (!headerless && response.extensions().get::<HeaderEncodingFailure>().is_some())
                {
                    Err(io::Error::new(io::ErrorKind::ConnectionAborted, "Remi closed the request without a response"))
                } else {
                    let framing = response.extensions().get::<NativeFraming>().cloned();
                    *response_headers.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = NativeHeaderPolicy {
                        keep_alive: framing
                            .as_ref()
                            .map_or_else(|| response_keep_alive(&response), |framing| framing.keep_alive),
                        connection: framing.as_ref().map_or_else(
                            || response.headers().get(header::CONNECTION).cloned(),
                            |framing| framing.connection.clone(),
                        ),
                        websocket_accept: framing.as_ref().and_then(|framing| framing.websocket_accept.clone()),
                        ordinary: framing.as_ref().and_then(|framing| framing.ordinary.clone()),
                        api: false,
                        upgraded: response.status() == StatusCode::SWITCHING_PROTOCOLS,
                    };
                    if (native || framing.is_some()) && response.status() != StatusCode::SWITCHING_PROTOCOLS {
                        *response.version_mut() = Version::HTTP_10;
                        for (name, value) in [
                            (header::CONTENT_LENGTH, framing.as_ref().and_then(|framing| framing.length.clone())),
                            (header::TRANSFER_ENCODING, framing.as_ref().and_then(|framing| framing.transfer.clone())),
                        ] {
                            // The raw header block carries explicit framing,
                            // which is neither enforced nor encoded here.
                            let value = value.filter(|_| !native);
                            match value {
                                Some(value) => {
                                    response.headers_mut().insert(name, value);
                                }
                                None => {
                                    response.headers_mut().remove(name);
                                }
                            }
                        }
                        // Unknown body size makes HTTP/1.0 use the connection close
                        // as the boundary, without an inferred length or chunk framing.
                        let (parts, body) = response.into_parts();
                        response = Response::from_parts(parts, axum::body::Body::from_stream(body.into_data_stream()));
                    }
                    if let Some(policy) = api_policy {
                        *response_headers.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            policy.response_policy(&response);
                    }
                    Ok(response)
                }
            })
        });
        let mut connection = hyper::server::conn::http1::Builder::new()
            // Each following request gets the request parser and its own
            // headers/target, including when an API request came first.
            .keep_alive(false)
            // Completing the input side does not cancel the UI handler's
            // response, including a headerless GET terminated by EOF.
            .half_close(native)
            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            .with_upgrades();
        tokio::select! {
            result = &mut connection => result.map_err(io::Error::other)?,
            () = &mut shutdown => {
                Pin::new(&mut connection).graceful_shutdown();
                connection.await.map_err(io::Error::other)?;
                return Ok(());
            }
        }
        let Some(completed) = connection.into_parts() else { return Ok(()) };
        if !response_headers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).keep_alive {
            return Ok(());
        }
        let stream = completed.io.into_inner();
        if stream.api_input.as_ref().is_some_and(|progress| {
            !progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drained(completed.read_buf.len())
        }) {
            return Ok(());
        }
        let mut buffered = completed.read_buf.to_vec();
        buffered.extend_from_slice(stream.source.buffer());
        let mut transport = stream.source.into_inner();
        transport.prepend(buffered);
        source = BufReader::new(transport);
    }
}

/// Serve the UI and API with HTTP/1 upgrades and drain active HTTP requests on
/// shutdown. The listener retains Axum's existing accept/retry policy.
pub async fn serve<L: Listener>(mut listener: L, router: Router, shutdown: impl Future<Output = ()>) -> io::Result<()> {
    serve_until(&mut listener, router, shutdown, || {}).await
}

/// The product server acknowledges HTTP accept-loop shutdown before draining
/// requests, so a request can synchronously invoke App.close without waiting
/// for its own response to finish.
pub async fn serve_with_stop<L: Listener>(mut listener: L, router: Router, stop: super::ServerStop) -> io::Result<()> {
    serve_until(&mut listener, router, stop.requested(), || stop.accept_stopped()).await
}

async fn serve_until<L: Listener>(
    listener: &mut L,
    router: Router,
    shutdown: impl Future<Output = ()>,
    accept_stopped: impl FnOnce(),
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    let (stopping, _) = watch::channel(false);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            (stream, _) = listener.accept() => {
                let router = router.clone();
                let mut stopped = stopping.subscribe();
                connections.spawn(async move {
                    let shutdown = async move {
                        let already_stopped = *stopped.borrow();
                        if !already_stopped {
                            let _ = stopped.changed().await;
                        }
                    };
                    if let Err(error) = serve_connection(stream, router, shutdown).await {
                        tracing::debug!(%error, "HTTP connection closed");
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    tracing::error!(%error, "HTTP connection task stopped");
                }
            }
        }
    }
    finish_connections(connections, stopping, accept_stopped).await;
    Ok(())
}

async fn finish_connections(
    mut connections: JoinSet<()>,
    stopping: watch::Sender<bool>,
    accept_stopped: impl FnOnce(),
) {
    accept_stopped();
    stopping.send_replace(true);
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "HTTP connection task stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::sync::{mpsc, oneshot, Notify};

    #[tokio::test]
    async fn close_acknowledges_http_stop_before_a_pending_handler_drains() {
        use std::task::Poll;
        let (finish, pending) = oneshot::channel::<()>();
        let mut handlers = JoinSet::new();
        // Only a metadata future: no listener, request, socket or UI worker.
        handlers.spawn(async move {
            let _ = pending.await;
        });
        let (stopping, _) = watch::channel(false);
        let stopped = stopping.subscribe();
        let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let acknowledgement = accepted.clone();
        let drain = finish_connections(handlers, stopping, move || {
            acknowledgement.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::pin!(drain);
        let first_poll = std::future::poll_fn(|context| Poll::Ready(drain.as_mut().poll(context))).await;
        assert!(first_poll.is_pending());
        assert!(accepted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(*stopped.borrow());
        finish.send(()).unwrap();
        drain.await;
    }

    struct MemoryListener(mpsc::UnboundedReceiver<DuplexStream>);
    impl Listener for MemoryListener {
        type Io = DuplexStream;
        type Addr = ();
        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            (self.0.recv().await.expect("fixture listener stays open"), ())
        }
        fn local_addr(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn shutdown_closes_a_connection_waiting_for_its_request_line() {
        let (stream, mut client) = tokio::io::duplex(64);
        tokio::time::timeout(Duration::from_secs(5), serve_connection(stream, Router::new(), async {}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(client.read_u8().await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    async fn native_first_line_wire(line: &[u8], expect_probe: bool) -> Vec<u8> {
        let routes = Router::new().route(
            "/fixture",
            get(move |headers: axum::http::HeaderMap| async move {
                assert_eq!(headers.contains_key("x-probe"), expect_probe);
                native_response(native_headers(
                    b"\0\xffbody\r\nHTTP/1.1".to_vec().into_response(),
                    [("X-Fixture".into(), "present".into())],
                ))
            }),
        );
        let (stream, mut client) = tokio::io::duplex(64);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        client.write_all(line).await.unwrap();
        if line.ends_with(b"\n") {
            client.write_all(b"Host: fixture\r\nX-Probe: yes\r\n\r\n").await.unwrap();
        }
        client.shutdown().await.unwrap();
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        let _ = server.await.unwrap();
        wire
    }

    #[tokio::test]
    async fn native_request_line_whitespace_uses_latin1_string_split_rules() {
        for space in [b' ', b'\t', 0x0b, 0x0c, b'\r', 0x1c, 0x1d, 0x1e, 0x1f, 0x85, 0xa0] {
            let mut line = vec![space];
            line.extend_from_slice(b"GET");
            line.extend_from_slice(&[space, space]);
            line.extend_from_slice(b"/fixture");
            line.push(space);
            line.extend_from_slice(b"HTTP/1.1");
            line.extend_from_slice(&[space, b'\r', b'\n']);
            let wire = native_first_line_wire(&line, true).await;
            assert!(wire.starts_with(b"HTTP/1.0 200 OK\r\n"), "space {space:#x}: {wire:?}");
            assert!(wire.ends_with(b"\0\xffbody\r\nHTTP/1.1"));
        }
    }

    #[tokio::test]
    async fn native_http09_and_other_valid_versions_keep_original_header_rules() {
        for (line, headerless, probe) in [
            (b"GET /fixture\n".as_slice(), true, false),
            (b"GET /fixture", true, false),
            (b"GET /fixture HTTP/0.9\r\n", true, true),
            (b"GET /fixture HTTP/0.09\r\n", false, true),
            (b"GET /fixture HTTP/0.8\r\n", false, true),
            (b"GET /fixture HTTP/1.9\r\n", false, true),
            (b"GET /fixture HTTP/0000000001.0000000009\r\n", false, true),
        ] {
            let wire = native_first_line_wire(line, probe).await;
            let body = b"\0\xffbody\r\nHTTP/1.1";
            if headerless {
                assert_eq!(wire, body, "{line:?}");
            } else {
                assert!(wire.starts_with(b"HTTP/1.0 200 OK\r\n"), "{line:?}: {wire:?}");
                assert!(wire.ends_with(body));
            }
        }
    }

    #[tokio::test]
    async fn a_whitespace_only_first_line_closes_before_a_second_request() {
        let routes = Router::new().fallback(|| async { "must not be routed" });
        let (stream, mut client) = tokio::io::duplex(64);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        client.write_all(b" \t\x85\xa0\r\nGET /fixture HTTP/1.1\r\n\r\n").await.unwrap();
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        let _ = server.await.unwrap();
        assert!(wire.is_empty(), "{wire:?}");
    }

    async fn method_wire(method: &str, target: &str, version: &str) -> Vec<u8> {
        let routes = Router::new()
            .route("/fixture", get(|| async { "fixture" }))
            .route("/api/fixture", axum::routing::put(|| async { "api" }));
        let (stream, mut client) = tokio::io::duplex(64);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        let request: Vec<_> = format!("{method} {target} {version}\r\nHost: fixture\r\nConnection: close\r\n\r\n")
            .chars()
            .map(|ch| u8::try_from(u32::from(ch)).unwrap())
            .collect();
        client.write_all(&request).await.unwrap();
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        let _ = server.await.unwrap();
        wire
    }

    #[tokio::test]
    async fn native_method_authhead_retains_challenge_and_api_methods_keep_routing() {
        let wire = method_wire("AUTHHEAD", "/fixture", "HTTP/1.1").await;
        assert!(wire.starts_with(b"HTTP/1.0 401 Unauthorized\r\n"), "{wire:?}");
        let boundary = wire.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let fields = std::str::from_utf8(&wire[..boundary]).unwrap().lines().collect::<Vec<_>>();
        assert_eq!(fields[3..], ["WWW-Authenticate: Basic realm=\"Protected\"", "Content-type: text/html"]);
        assert!(wire[boundary + 4..].is_empty());
        for method in ["gui_update", "not_show_again"] {
            assert!(method_wire(method, "/fixture", "HTTP/1.1").await.is_empty(), "{method}");
        }
        let wire = method_wire("PUT", "/api/fixture", "HTTP/1.1").await;
        assert!(wire.starts_with(b"HTTP/1.1 200 OK\r\n"), "{wire:?}");
        assert!(wire.ends_with(b"api"));
    }

    async fn header_wire(method: &str, version: &str, headers: &[u8], eof: bool, api: bool) -> (Vec<u8>, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let head_calls = calls.clone();
        let routes = Router::new()
            .route(
                "/fixture",
                get(move || {
                    let called = called.clone();
                    async move {
                        called.fetch_add(1, Ordering::SeqCst);
                        native_response("ok".into_response())
                    }
                })
                .head(move || {
                    let called = head_calls.clone();
                    async move {
                        called.fetch_add(1, Ordering::SeqCst);
                        native_response(StatusCode::OK.into_response())
                    }
                }),
            )
            .route("/api/fixture", get(|| async { "api" }));
        let target = if api { "/api/fixture" } else { "/fixture" };
        let (stream, mut client) = tokio::io::duplex(128);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        client.write_all(format!("{method} {target} {version}\r\n").as_bytes()).await.unwrap();
        client.write_all(headers).await.unwrap();
        if eof {
            client.shutdown().await.unwrap();
        }
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        let _ = server.await.unwrap();
        (wire, calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn native_header_limits_count_the_terminator_and_keep_head_http09_boundaries() {
        for (method, version, count) in [
            ("GET", "HTTP/1.1", 99),
            ("GET", "HTTP/1.1", 100),
            ("HEAD", "HTTP/1.1", 100),
            ("GET", "HTTP/0.9", 100),
            ("HEAD", "HTTP/0.9", 100),
        ] {
            let mut headers = b"Host: fixture\r\n".to_vec();
            for _ in 1..count {
                headers.extend_from_slice(b"X-Probe: yes\r\n");
            }
            headers.extend_from_slice(b"\r\n");
            let (wire, calls) = header_wire(method, version, &headers, false, false).await;
            if count == 99 {
                assert_eq!(calls, 1);
                assert!(wire.starts_with(b"HTTP/1.0 200 OK\r\n"));
                assert!(wire.ends_with(b"ok"));
                continue;
            }
            assert_eq!(calls, 0, "{method} {version}");
            let body = if version == "HTTP/0.9" {
                wire.as_slice()
            } else {
                assert!(wire.starts_with(b"HTTP/1.0 431 Too many headers\r\n"), "{wire:?}");
                let boundary = wire.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap();
                let fields = std::str::from_utf8(&wire[..boundary]).unwrap();
                assert!(fields.contains("\r\nContent-Length: "));
                &wire[boundary + 4..]
            };
            if method == "HEAD" {
                assert!(body.is_empty());
            } else {
                assert!(body.starts_with(b"<!DOCTYPE HTML>\n"));
                assert!(std::str::from_utf8(body).unwrap().contains("431 - got more than 100 headers."));
            }
        }
        let mut headers = b"Host: fixture\r\nConnection: close\r\n".to_vec();
        for _ in 2..100 {
            headers.extend_from_slice(b"X-Probe: yes\r\n");
        }
        headers.extend_from_slice(b"\r\n");
        let (wire, _) = header_wire("GET", "HTTP/1.1", &headers, false, true).await;
        assert!(wire.starts_with(b"HTTP/1.1 200 OK\r\n"), "{wire:?}");
        assert!(wire.ends_with(b"api"));
    }

    #[tokio::test]
    async fn native_header_length_and_eof_boundaries_precede_later_parsing() {
        for length in [65536, 65537] {
            let mut line = b"X: ".to_vec();
            line.resize(length - 2, b'a');
            line.extend_from_slice(b"\r\n\r\n");
            let (wire, calls) = header_wire("GET", "HTTP/1.1", &line, false, false).await;
            if length == 65536 {
                assert_eq!(calls, 1);
                assert!(wire.starts_with(b"HTTP/1.0 200 OK\r\n"));
            } else {
                assert_eq!(calls, 0);
                assert!(wire.starts_with(b"HTTP/1.0 431 Line too long\r\n"), "{wire:?}");
                assert!(std::str::from_utf8(&wire)
                    .unwrap()
                    .contains("got more than 65536 bytes when reading header line."));
            }
        }
        let (wire, calls) = header_wire("GET", "HTTP/1.1", b"Host: fixture\r\nX: eof", true, false).await;
        assert_eq!(calls, 1);
        assert!(wire.starts_with(b"HTTP/1.0 200 OK\r\n"), "{wire:?}");
        assert!(wire.ends_with(b"ok"));
        let mut headers = b"bad header\r\n".to_vec();
        for _ in 1..100 {
            headers.extend_from_slice(b"X: y\r\n");
        }
        headers.extend_from_slice(b"\r\n");
        let (wire, calls) = header_wire("GET", "HTTP/1.1", &headers, false, false).await;
        assert_eq!(calls, 0);
        assert!(wire.starts_with(b"HTTP/1.0 431 Too many headers\r\n"), "{wire:?}");
    }

    #[tokio::test]
    async fn response_header_buffering_preserves_fragmented_headers_and_raw_socket_bytes() {
        let (source, mut client) = tokio::io::duplex(3);
        let writer = tokio::spawn(async move {
            let mut stream = RequestStream {
                source: BufReader::new(source),
                line: Vec::new(),
                offset: 0,
                api_input: None,
                retain_transport: false,
                output: Some(NativeOutput {
                    policy: Arc::new(Mutex::new(NativeHeaderPolicy {
                        connection: Some(HeaderValue::from_static("Upgrade")),
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
            };
            for fragment in [
                b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 101 Switching Protocols\r\nconnec".as_slice(),
                b"tion: Upgrade, close\r\nX-Fixture: connection: close\r\n\r",
                b"\n",
                b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n\x00\xff",
            ] {
                stream.write_all(fragment).await.unwrap();
            }
            stream.shutdown().await.unwrap();
        });
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        writer.await.unwrap();
        assert_eq!(wire, b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nX-Fixture: connection: close\r\n\r\nHTTP/1.1 200 OK\r\nconnection: close\r\n\r\n\x00\xff");
    }

    #[tokio::test]
    async fn native_http_framing_keeps_explicit_lengths_and_head_has_no_body() {
        for (method, explicit, close) in
            [("GET", false, false), ("HEAD", false, false), ("GET", true, false), ("GET", false, true)]
        {
            let content = b"\x00\xff#<\n";
            let routes = Router::new().route(
                "/fixture",
                get(move || async move {
                    let mut response = content.to_vec().into_response();
                    if explicit {
                        response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_static("5"));
                    }
                    if close {
                        response.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
                    }
                    native_response(response)
                })
                .head(|| async { native_response(StatusCode::OK.into_response()) }),
            );
            let (stream, mut client) = tokio::io::duplex(64);
            let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
            client.write_all(format!("{method} /fixture HTTP/1.1\r\nHost: fixture\r\n\r\n").as_bytes()).await.unwrap();
            let mut wire = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
            server.await.unwrap().unwrap();
            let boundary = wire.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
            let headers = String::from_utf8_lossy(&wire[..boundary]);
            assert!(headers.starts_with("HTTP/1.0 200 OK\r\n"), "{headers}");
            assert!(headers.contains("\r\nServer: BaseHTTP/0.6 Python/3.13.13\r\nDate: "), "{headers}");
            if method == "HEAD" {
                assert_eq!(headers.lines().count(), 3, "{headers}");
            }
            assert_eq!(headers.lines().any(|line| line == "content-length: 5"), explicit);
            assert_eq!(headers.lines().any(|line| line == "connection: close"), close);
            assert!(!headers.contains("transfer-encoding:"));
            assert_eq!(&wire[boundary + 4..], if method == "HEAD" { &[] } else { content.as_slice() });
        }
    }

    #[tokio::test]
    async fn download_headers_keep_original_order_case_and_distinct_case_keys() {
        let routes = Router::new().route(
            "/fixture",
            get(|| async {
                native_response(native_headers(
                    b"\0\xffabc".to_vec().into_response(),
                    [
                        ("X-Second".into(), "first".into()),
                        ("X-First".into(), "second".into()),
                        ("x-second".into(), "third".into()),
                        ("Content-Length".into(), "5".into()),
                    ],
                ))
            }),
        );
        let (stream, mut client) = tokio::io::duplex(3);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        client.write_all(b"GET /fixture HTTP/1.1\r\nHost: fixture\r\n\r\n").await.unwrap();
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        server.await.unwrap().unwrap();
        let boundary = wire.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let headers = std::str::from_utf8(&wire[..boundary]).unwrap();
        assert_eq!(
            headers.lines().skip(3).collect::<Vec<_>>(),
            ["X-Second: first", "X-First: second", "x-second: third", "Content-Length: 5"]
        );
        assert_eq!(&wire[boundary + 4..], b"\0\xffabc");
    }

    async fn download_filename_wire(filename: &'static str) -> Vec<u8> {
        download_filename_wire_version(filename, "HTTP/1.1").await
    }

    async fn download_filename_wire_version(filename: &'static str, version: &str) -> Vec<u8> {
        let routes = Router::new().route(
            "/fixture",
            get(move || async move {
                native_response(native_headers(
                    b"\0\xffprofile".to_vec().into_response(),
                    [("Content-Disposition".into(), format!("attachment; filename=\"{filename}\""))],
                ))
            }),
        );
        let (stream, mut client) = tokio::io::duplex(3);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        client.write_all(format!("GET /fixture {version}\r\nHost: fixture\r\n\r\n").as_bytes()).await.unwrap();
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        let _ = server.await.unwrap();
        wire
    }

    #[tokio::test]
    async fn native_header_values_use_latin1_for_exported_filenames() {
        let wire = download_filename_wire("café £\u{85}ÿ.srs").await;
        assert!(
            wire.ends_with(
                b"Content-Disposition: attachment; filename=\"caf\xe9 \xa3\x85\xff.srs\"\r\n\r\n\0\xffprofile"
            ),
            "{wire:?}"
        );
    }

    #[tokio::test]
    async fn native_header_values_outside_latin1_close_before_flushing() {
        for filename in ["car\u{100}.srs", "日本.srs", "car😀.srs"] {
            assert!(download_filename_wire(filename).await.is_empty(), "{filename}");
        }
    }

    async fn native_reply_wire(headers: Vec<(String, String)>) -> Vec<u8> {
        let routes = Router::new().route(
            "/fixture",
            get(move || {
                let headers = headers.clone();
                async move { native_response(native_headers(b"\0\xffraw".to_vec().into_response(), headers)) }
            }),
        );
        let (stream, mut client) = tokio::io::duplex(3);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        client.write_all(b"GET /fixture HTTP/1.1\r\nHost: fixture\r\n\r\n").await.unwrap();
        let mut wire = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
        let _ = server.await.unwrap();
        wire
    }

    #[tokio::test]
    async fn native_response_headers_keep_unvalidated_latin1_name_and_value_bytes() {
        for (name, value) in [
            ("X-Fixture", "one\0two"),
            ("X-Fixture", "one\r\nOther: two"),
            ("X-é", "latin1"),
            ("X Bad", "value"),
            ("X:Bad", "value"),
        ] {
            let wire = native_reply_wire(vec![(name.into(), value.into())]).await;
            let mut expected: Vec<_> =
                format!("{name}: {value}\r\n\r\n").chars().map(|ch| u8::try_from(u32::from(ch)).unwrap()).collect();
            expected.extend_from_slice(b"\0\xffraw");
            assert!(wire.ends_with(&expected), "{name}: {wire:?}");
        }
        assert!(native_reply_wire(vec![("X-日本".into(), "value".into())]).await.is_empty());
    }

    #[tokio::test]
    async fn native_response_body_is_not_reencoded_or_limited_by_declared_framing() {
        for (name, value) in [
            ("Content-Length", "0"),
            ("Content-Length", "1"),
            ("Content-Length", "20"),
            ("Content-Length", "invalid"),
            ("Transfer-Encoding", "chunked"),
        ] {
            let wire = native_reply_wire(vec![(name.into(), value.into())]).await;
            let mut expected = format!("{name}: {value}\r\n\r\n").into_bytes();
            expected.extend_from_slice(b"\0\xffraw");
            assert!(wire.ends_with(&expected), "{name}={value}: {wire:?}");
        }
    }

    #[tokio::test]
    async fn headerless_response_skips_latin1_encoding_failures() {
        assert_eq!(download_filename_wire_version("日本.srs", "HTTP/0.9").await, b"\0\xffprofile");
    }

    #[tokio::test]
    async fn explicit_native_response_keep_alive_reparses_the_following_request() {
        for (headers, short_get, expected) in [
            (vec![("Connection", "keep-alive")], false, 2),
            (vec![("Connection", "KeEp-AlIvE")], false, 2),
            (vec![("Connection", "keep-alive"), ("Connection", "other")], false, 2),
            (vec![("Connection", "close"), ("Connection", "keep-alive")], false, 2),
            (vec![("Connection", "keep-alive"), ("Connection", "close")], false, 1),
            (vec![("Connection", " keep-alive")], false, 1),
            (vec![], false, 1),
            (vec![("X-日本", "skipped"), ("Connection", "keep-alive")], true, 2),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let captured = calls.clone();
            let first_headers: Vec<_> =
                headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
            let routes = Router::new().fallback(
                move |axum::extract::Extension(target): axum::extract::Extension<NativeTarget>,
                      request_headers: axum::http::HeaderMap| {
                    let calls = captured.clone();
                    let first_headers = first_headers.clone();
                    async move {
                        calls.lock().unwrap().push((
                            target.0.clone(),
                            request_headers.get("x-probe").map(|value| value.as_bytes().to_vec()),
                        ));
                        let headers = if target.0 == "/first" {
                            first_headers
                        } else {
                            vec![("Connection".into(), "close".into())]
                        };
                        native_response(native_headers(target.0.into_response(), headers))
                    }
                },
            );
            let (stream, mut client) = tokio::io::duplex(65536);
            let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
            let first = if short_get { "GET /first\r\n" } else { "GET /first HTTP/1.1\r\nX-Probe: first\r\n\r\n" };
            client
                .write_all(format!("{first}GET /second#< HTTP/1.1\r\nX-Probe: second\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut wire = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire)).await.unwrap().unwrap();
            server.await.unwrap().unwrap();
            let calls = calls.lock().unwrap();
            assert_eq!(
                calls.len(),
                expected,
                "headers={headers:?}, short_get={short_get}: {}",
                String::from_utf8_lossy(&wire)
            );
            if expected == 2 {
                assert_eq!(calls[1], ("/second#<".into(), Some(b"second".to_vec())));
                assert!(wire.ends_with(b"\r\n\r\n/second#<"));
            }
        }
    }

    #[tokio::test]
    async fn native_keep_alive_waits_for_the_next_request_and_honors_shutdown() {
        for stop_after_first in [false, true] {
            let routes = Router::new().fallback(
                |axum::extract::Extension(target): axum::extract::Extension<NativeTarget>| async move {
                    let connection = if target.0 == "/first" { "keep-alive" } else { "close" };
                    let length = target.0.len().to_string();
                    native_response(native_headers(
                        target.0.into_response(),
                        [("Connection".into(), connection.into()), ("Content-Length".into(), length)],
                    ))
                },
            );
            let (stream, mut client) = tokio::io::duplex(65536);
            let (stop, stopped) = oneshot::channel();
            let server = tokio::spawn(serve_connection(stream, routes, async {
                let _ = stopped.await;
            }));
            client.write_all(b"GET /first HTTP/1.1\r\n\r\n").await.unwrap();
            let mut headers = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(client.read_u8().await.unwrap());
                }
                let mut body = [0; 6];
                client.read_exact(&mut body).await.unwrap();
                assert_eq!(&body, b"/first");
            })
            .await
            .unwrap();
            if stop_after_first {
                stop.send(()).unwrap();
            } else {
                client.write_all(b"GET /second HTTP/1.1\r\n\r\n").await.unwrap();
            }
            let mut rest = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut rest)).await.unwrap().unwrap();
            server.await.unwrap().unwrap();
            if stop_after_first {
                assert!(rest.is_empty());
            } else {
                assert!(rest.starts_with(b"HTTP/1.0 200 OK\r\n"));
                assert!(rest.ends_with(b"\r\n\r\n/second"));
            }
        }
    }

    #[tokio::test]
    async fn native_http_closes_after_one_response_while_api_connections_remain_persistent() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (path, expected) in [("/fixture", 1), ("/api/fixture", 2)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let captured = calls.clone();
            let routes = Router::new().route(
                path,
                get(move || {
                    let calls = captured.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        "fixture"
                    }
                }),
            );
            let (stream, mut client) = tokio::io::duplex(65536);
            let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
            client.write_all(format!("GET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: keep-alive\r\n\r\nGET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response)).await.unwrap().unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), expected, "{path}: {}", String::from_utf8_lossy(&response));
            let _ = server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn api_first_connections_reparse_later_native_targets_and_headers() {
        let routes = Router::new()
            .route("/api/fixture", axum::routing::any(|_body: axum::body::Bytes| async { "api" }))
            .fallback(
                |axum::extract::Extension(target): axum::extract::Extension<NativeTarget>,
                 headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["x-probe"], "native");
                    native_response(native_headers(target.0.into_response(), [("X-Native".into(), "yes".into())]))
                },
            );
        for first in [
            "GET /api/fixture HTTP/1.1\r\nHost: fixture\r\nX-Probe: api\r\n\r\n",
            "POST /api/fixture HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello",
            "POST /api/fixture HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhe\r\n3;ext=yes\r\nllo\r\n0\r\nX-End: trailer\r\n\r\n",
        ] {
            for capacity in [3, 65536] {
                let (stream, client) = tokio::io::duplex(capacity);
                let server = tokio::spawn(serve_connection(stream, routes.clone(), std::future::pending()));
                let (mut reader, mut writer) = tokio::io::split(client);
                let input = format!("{first}GET /native#< HTTP/1.1\r\nX-Probe: native\r\n\r\n");
                let writing = tokio::spawn(async move { writer.write_all(input.as_bytes()).await.unwrap() });
                let mut wire = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut wire)).await.unwrap().unwrap();
                writing.await.unwrap();
                server.await.unwrap().unwrap();
                let text = String::from_utf8_lossy(&wire);
                assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
                assert!(text.contains("HTTP/1.0 200 OK\r\n"), "{text}");
                assert!(wire.ends_with(b"\r\n\r\n/native#<"), "{text}");
            }
        }
    }

    #[tokio::test]
    async fn api_persistence_and_framing_match_the_standard_transport() {
        let routes = Router::new().fallback(|request: axum::extract::Request| async move {
            let path = request.uri().path().to_owned();
            let mut failed = false;
            let body = if path == "/api/echo" {
                match axum::body::to_bytes(request.into_body(), 4096).await {
                    Ok(body) => body,
                    Err(_) => {
                        failed = true;
                        axum::body::Bytes::from_static(b"invalid body")
                    }
                }
            } else {
                axum::body::Bytes::from_static(b"api")
            };
            let mut response = body.into_response();
            if failed {
                *response.status_mut() = StatusCode::BAD_REQUEST;
            }
            response.headers_mut().insert(header::DATE, HeaderValue::from_static("Mon, 01 Jan 2024 00:00:00 GMT"));
            if path == "/api/close" {
                response.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("close"));
            }
            if path == "/api/connection" {
                response.headers_mut().append(header::CONNECTION, HeaderValue::from_static("other"));
                response.headers_mut().append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
            }
            if path == "/api/v10" {
                *response.version_mut() = Version::HTTP_10;
            }
            if path == "/api/stream" {
                let (parts, body) = response.into_parts();
                response = Response::from_parts(parts, axum::body::Body::from_stream(body.into_data_stream()));
            }
            response
        });
        for first in [
            "GET /api/one HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "HEAD /api/one HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "GET /api/one HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
            "GET /api/one HTTP/1.0\r\n\r\n",
            "GET /api/one HTTP/1.1\r\nConnection: close\r\n\r\n",
            "GET /api/close HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "GET /api/close HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
            "GET /api/connection HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "GET /api/connection HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
            "GET /api/v10 HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "GET /api/stream HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "GET /api/stream HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
            "POST /api/echo HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello",
            "POST /api/echo HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
            "POST /api/echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            "POST /api/echo HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello",
            "POST /api/echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2;extension=yes\r\nhe\r\n3\r\nllo\r\n0\r\nX-Trailer: end\r\n\r\n",
            "POST /api/echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n000000000000000000005\r\nhello\r\n000000000000000000000\r\n\r\n",
            "POST /api/echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nBad\0Trailer: invalid\r\n\r\n",
            "POST /api/echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\nX-Trailer: end\r\n\r\n",
            "POST /api/echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\r\n\r\n",
            "POST /api/ignore HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello",
            "POST /api/ignore HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            "POST /api/ignore HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ] {
            let input = format!("{first}GET /api/two HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n");
            for capacity in [3, 65536] {
            let mut observed = Vec::new();
            for direct in [true, false] {
                let (stream, client) = tokio::io::duplex(capacity);
                let router = routes.clone();
                let server = tokio::spawn(async move {
                    if direct {
                        let service = hyper::service::service_fn(move |request| router.clone().oneshot(request));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                            .with_upgrades().await;
                    } else {
                        let _ = serve_connection(stream, router, std::future::pending()).await;
                    }
                });
                let (mut reader, mut writer) = tokio::io::split(client);
                let sending = input.clone();
                let writing = tokio::spawn(async move { let _ = writer.write_all(sending.as_bytes()).await; });
                let mut wire = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut wire)).await
                    .unwrap_or_else(|_| panic!("capacity={capacity}, direct={direct}, request={first:?}, wire={}", String::from_utf8_lossy(&wire))).unwrap();
                writing.await.unwrap();
                server.await.unwrap();
                observed.push(wire);
            }
            assert!(observed[1] == observed[0], "capacity={capacity}: {first:?}\nshared: {}\nstandard: {}", String::from_utf8_lossy(&observed[1]), String::from_utf8_lossy(&observed[0]));
            }
        }
    }

    #[tokio::test]
    async fn request_line_buffering_preserves_api_headers_and_binary_upload_bytes() {
        let routes = Router::new().route(
            "/api/fixture",
            axum::routing::post(|headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                assert_eq!(headers["x-probe"], "#<");
                body
            }),
        );
        let (stream, mut client) = tokio::io::duplex(65536);
        let server = tokio::spawn(serve_connection(stream, routes, std::future::pending()));
        let body = b"\x00\xff#<\r\nGET /literal#< HTTP/1.1\r\n\x00";
        client.write_all(b"POST /api/").await.unwrap();
        tokio::task::yield_now().await;
        client.write_all(format!("fixture HTTP/1.1\r\nHost: fixture\r\nX-Probe: #<\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
        client.write_all(body).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response)).await.unwrap().unwrap();
        assert!(response.ends_with(body), "{}", String::from_utf8_lossy(&response));
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shared_server_drains_an_inflight_api_response_on_shutdown() {
        let (accepted, incoming) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let entered = Arc::new(Notify::new());
        let complete = Arc::new(Notify::new());
        let started = entered.clone();
        let release = complete.clone();
        let routes = Router::new().route(
            "/api/fixture",
            get(move || {
                let started = started.clone();
                let release = release.clone();
                async move {
                    started.notify_one();
                    release.notified().await;
                    "completed API response"
                }
            }),
        );
        let server = tokio::spawn(serve(MemoryListener(incoming), routes, async {
            let _ = stopped.await;
        }));
        let (stream, mut client) = tokio::io::duplex(65536);
        accepted.send(stream).unwrap();
        client.write_all(b"GET /api/fixture HTTP/1.1\r\nHost: fixture\r\n\r\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
        stop.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(!server.is_finished(), "shutdown must wait for the pending response");
        complete.notify_one();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response)).await.unwrap().unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.ends_with("completed API response"), "{response}");
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
        drop(accepted);
    }
}
