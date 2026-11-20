//! The HTTP GETs of the listeners: connect and read timeouts per socket operation, redirects followed, any
//! status code returned with its body, a fixed set of default headers on the wire in a fixed order, a
//! `gzip` / `deflate` body decoded, and `localhost` tried as `::1` then `127.0.0.1`.

use std::io::{self, Read};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};

/// The headers sent with every request, in wire order: user agent, accepted encodings (gzip and deflate
/// only), `*/*`, `keep-alive`.
pub const DEFAULT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "python-requests/2.33.0"),
    ("Accept-Encoding", "gzip, deflate"),
    ("Accept", "*/*"),
    ("Connection", "keep-alive"),
];

/// The headers of one request: the defaults in their order with the request's
/// values for the same names (case-insensitive), then the request's other headers.
pub fn merged_headers<'a>(request: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut headers: Vec<(&str, &str)> = DEFAULT_HEADERS
        .iter()
        .map(|&(name, value)| {
            request.iter().find(|(own, _)| own.eq_ignore_ascii_case(name)).map_or((name, value), |&(_, v)| (name, v))
        })
        .collect();
    for &(name, value) in request {
        if !DEFAULT_HEADERS.iter().any(|(default, _)| default.eq_ignore_ascii_case(name)) {
            headers.push((name, value));
        }
    }
    headers
}

/// Decodes a response body by its `Content-Encoding` (`gzip`, `x-gzip`, `deflate`, comma-separated chains decoded
/// last-applied first); any other coding is left as it is.
fn decode_body(content_encoding: Option<&str>, body: Vec<u8>) -> io::Result<Vec<u8>> {
    let Some(codings) = content_encoding else { return Ok(body) };
    let codings: Vec<String> =
        codings.split(',').map(|coding| coding.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).collect();
    let mut body = body;
    for coding in codings.iter().rev() {
        let mut decoded = Vec::new();
        match coding.as_str() {
            "gzip" | "x-gzip" => {
                GzDecoder::new(body.as_slice()).read_to_end(&mut decoded)?;
            }
            // Deflate: zlib-wrapped first, raw deflate when that fails.
            "deflate" => {
                if ZlibDecoder::new(body.as_slice()).read_to_end(&mut decoded).is_err() {
                    decoded.clear();
                    DeflateDecoder::new(body.as_slice()).read_to_end(&mut decoded)?;
                }
            }
            _ => return Ok(body),
        }
        body = decoded;
    }
    Ok(body)
}

/// The address order for `localhost` (IPv6 first); any other host goes through the system resolver.
fn resolve(netloc: &str) -> io::Result<Vec<SocketAddr>> {
    if let Some(port) = netloc.strip_prefix("localhost:").and_then(|port| port.parse::<u16>().ok()) {
        return Ok(vec![SocketAddr::from((Ipv6Addr::LOCALHOST, port)), SocketAddr::from((Ipv4Addr::LOCALHOST, port))]);
    }
    Ok(netloc.to_socket_addrs()?.collect())
}

/// One HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    /// URL after redirects.
    pub url: String,
    pub body: Vec<u8>,
}

/// A transport failure (no response at all).
#[derive(Debug, thiserror::Error)]
#[error("http request failed: {0}")]
pub struct HttpError(pub String);

/// Issues GET requests; the production implementation is [`HttpClient`].
pub trait HttpGet: Send {
    fn get(&mut self, url: &str, headers: &[(&str, &str)]) -> Result<HttpReply, HttpError>;
}

/// A connection-reusing client.
pub struct HttpClient {
    agent: ureq::Agent,
}

impl HttpClient {
    /// `timeout` applies to the connect and to every read.
    pub fn new(timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .redirects(30)
            .resolver(resolve)
            .build();
        HttpClient { agent }
    }
}

impl HttpGet for HttpClient {
    fn get(&mut self, url: &str, headers: &[(&str, &str)]) -> Result<HttpReply, HttpError> {
        let request =
            merged_headers(headers).iter().fold(self.agent.get(url), |request, (name, value)| request.set(name, value));
        let response = match request.call() {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(error) => return Err(HttpError(error.to_string())),
        };
        let status = response.status();
        let url = response.get_url().to_owned();
        let encoding = response.header("Content-Encoding").map(str::to_owned);
        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body).map_err(|error| HttpError(error.to_string()))?;
        let body = decode_body(encoding.as_deref(), body).map_err(|error| HttpError(error.to_string()))?;
        Ok(HttpReply { status, url, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// The wire headers: Host, then the defaults in order; the ED updater's `User-Agent` replaces the
    /// default in place.
    #[test]
    fn headers_follow_requests() {
        assert_eq!(merged_headers(&[]), DEFAULT_HEADERS);
        assert_eq!(
            merged_headers(&[("user-agent", "Mozilla/5.0"), ("X-Extra", "1")]),
            [
                ("User-Agent", "Mozilla/5.0"),
                ("Accept-Encoding", "gzip, deflate"),
                ("Accept", "*/*"),
                ("Connection", "keep-alive"),
                ("X-Extra", "1"),
            ]
        );
    }

    /// A gzip body is decoded; the request carries the default headers in their order.
    #[test]
    fn request_on_the_wire_and_gzip_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                lines.push(line.trim_end().to_owned());
            }
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(b"{\"valid\": true}").unwrap();
            let body = gz.finish().unwrap();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            lines
        });
        let mut client = HttpClient::new(Duration::from_secs(5));
        let reply = client.get(&format!("http://127.0.0.1:{port}/state"), &[]).unwrap();
        assert_eq!(reply.body, b"{\"valid\": true}");
        let lines = server.join().unwrap();
        assert_eq!(
            lines,
            [
                "GET /state HTTP/1.1".to_owned(),
                format!("Host: 127.0.0.1:{port}"),
                "User-Agent: python-requests/2.33.0".to_owned(),
                "Accept-Encoding: gzip, deflate".to_owned(),
                "Accept: */*".to_owned(),
                "Connection: keep-alive".to_owned(),
            ]
        );
    }

    #[test]
    fn localhost_is_ipv6_first() {
        assert_eq!(
            resolve("localhost:8111").unwrap(),
            ["[::1]:8111".parse().unwrap(), "127.0.0.1:8111".parse().unwrap()]
        );
        let deflated = {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(b"abc").unwrap();
            z.finish().unwrap()
        };
        assert_eq!(decode_body(Some("deflate"), deflated).unwrap(), b"abc");
        assert_eq!(decode_body(Some("br"), b"raw".to_vec()).unwrap(), b"raw");
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::{HttpError, HttpGet, HttpReply};

    /// A URL and the reply (or transport failure) it gets.
    pub(crate) type ScriptedReply = (String, Result<HttpReply, String>);

    /// Scripted replies, shared so a test can keep feeding a client owned by a background thread.
    #[derive(Clone, Default)]
    pub(crate) struct ScriptedHttp {
        pub(crate) replies: Arc<Mutex<VecDeque<ScriptedReply>>>,
        pub(crate) requested: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedHttp {
        pub(crate) fn reply(&self, url: &str, status: u16, body: &str) {
            let reply = HttpReply { status, url: url.to_owned(), body: body.as_bytes().to_vec() };
            self.replies.lock().unwrap().push_back((url.to_owned(), Ok(reply)));
        }

        pub(crate) fn fail(&self, url: &str) {
            self.replies.lock().unwrap().push_back((url.to_owned(), Err("refused".to_owned())));
        }
    }

    impl HttpGet for ScriptedHttp {
        fn get(&mut self, url: &str, _headers: &[(&str, &str)]) -> Result<HttpReply, HttpError> {
            self.requested.lock().unwrap().push(url.to_owned());
            let mut replies = self.replies.lock().unwrap();
            match replies.iter().position(|(u, _)| u == url) {
                Some(index) => replies
                    .remove(index)
                    .map(|(_, r)| r.map_err(HttpError))
                    .unwrap_or_else(|| Err(HttpError("none".into()))),
                None => Err(HttpError("no scripted reply".into())),
            }
        }
    }
}
