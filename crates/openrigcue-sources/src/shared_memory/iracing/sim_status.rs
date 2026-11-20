//! The iRacing simulator status probe.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};

use super::Raised;

/// Host and port of the status page `http://127.0.0.1:32034/get_sim_status?object=simStatus`.
pub const SIM_STATUS_ADDRESS: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 32034);
/// Path of the status page.
pub const SIM_STATUS_PATH: &str = "/get_sim_status?object=simStatus";

/// Whether the iRacing simulator reports itself running.
pub trait SimStatus: Send {
    /// Whether the body of the status page contains `'running:1'`; `false` when the page cannot be
    /// fetched (no connection, or an error status); `Raised` for a broken response
    /// (one that breaks off or cannot be parsed, a body that is not UTF-8).
    fn is_running(&mut self) -> Result<bool, Raised>;
}

/// Fetches the status page over HTTP. Wine/Proton sockets are host sockets, so the simulator's local server is
/// reachable from Linux at the same address. There is no timeout:
/// connect, write and read block until the server answers
/// or closes.
pub struct HttpSimStatus {
    address: SocketAddr,
}

impl HttpSimStatus {
    /// Probe of [`SIM_STATUS_ADDRESS`].
    pub fn new() -> Self {
        HttpSimStatus { address: SIM_STATUS_ADDRESS }
    }
}

impl Default for HttpSimStatus {
    fn default() -> Self {
        HttpSimStatus::new()
    }
}

impl SimStatus for HttpSimStatus {
    fn is_running(&mut self) -> Result<bool, Raised> {
        match urlopen_read(self.address, SIM_STATUS_PATH)? {
            Some(body) => std::str::from_utf8(&body).map(|text| text.contains("running:1")).map_err(|_| Raised),
            None => Ok(false),
        }
    }
}

/// Most redirects followed for one request.
const MAX_REDIRECTIONS: usize = 10;
/// The same URL visited this often is a redirect loop.
const MAX_REPEATS: usize = 4;
/// Most header lines accepted in a response.
const MAX_HEADERS: usize = 100;
/// Longest line accepted in a response.
const MAX_LINE: usize = 65536;

/// Fetches an `http://` URL: `Ok(Some(body))` for
/// a 2xx response (after following 301/302/303/307/308 redirects), `Ok(None)` for a fetch failure (connect or send
/// failure, an error status, too many redirects or a redirect loop, a redirect to another scheme), `Err`
/// for a broken response (failures while reading the status, headers or body).
fn urlopen_read(address: SocketAddr, path: &str) -> Result<Option<Vec<u8>>, Raised> {
    let (mut host, mut port, mut path) = (address.ip().to_string(), address.port(), path.to_owned());
    let mut visited: Vec<String> = Vec::new();
    loop {
        let url = format!("http://{host}:{port}{path}");
        let Ok(mut stream) = TcpStream::connect((host.as_str(), port)) else { return Ok(None) };
        // Request headers: `Accept-Encoding: identity` first, then `Host`, `User-Agent` and
        // `Connection: close`, title-cased.
        let host_header = if port == 80 { host.clone() } else { format!("{host}:{port}") };
        let request = format!(
            "GET {path} HTTP/1.1\r\nAccept-Encoding: identity\r\nHost: {host_header}\r\nUser-Agent: Python-urllib/3.13\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return Ok(None);
        }
        let mut reader = BufReader::new(stream);
        let response = read_response(&mut reader)?;
        if (200..300).contains(&response.status) {
            return read_body(&mut reader, &response).map(Some);
        }
        if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(None);
        }
        // The redirect target is the `location` header, then `uri`; without either the redirect status
        // counts as an error status.
        let Some(location) = response.header("location").or_else(|| response.header("uri")) else { return Ok(None) };
        let Some((next_host, next_port, next_path)) = resolve_redirect(&host, port, &path, &location) else {
            return Ok(None);
        };
        visited.push(url);
        let next = format!("http://{next_host}:{next_port}{next_path}");
        if visited.len() >= MAX_REDIRECTIONS || visited.iter().filter(|seen| **seen == next).count() >= MAX_REPEATS {
            return Ok(None);
        }
        (host, port, path) = (next_host, next_port, next_path);
    }
}

/// The parts of a response the probe needs.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Response {
    /// The first header of that name (case-insensitive).
    fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone())
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<Vec<u8>, Raised> {
    let mut line = Vec::new();
    reader.take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).map_err(|_| Raised)?;
    if line.len() > MAX_LINE {
        return Err(Raised);
    }
    Ok(line)
}

/// Reads a response up to the body: the status line (an error for an empty one,
/// or for one that does not start with `HTTP/` or has no 3-digit status), `100 Continue` responses
/// skipped, and the headers (at most 100).
fn read_response(reader: &mut impl BufRead) -> Result<Response, Raised> {
    loop {
        let line = read_line(reader)?;
        let line = String::from_utf8_lossy(&line).into_owned();
        let mut parts = line.trim_end_matches(['\r', '\n']).splitn(3, ' ');
        let (version, status) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if !version.starts_with("HTTP/") || status.len() != 3 {
            return Err(Raised);
        }
        let status: u16 = status.parse().map_err(|_| Raised)?;
        if !(100..=999).contains(&status) {
            return Err(Raised);
        }
        let mut headers = Vec::new();
        loop {
            let line = read_line(reader)?;
            if line == b"\r\n" || line == b"\n" || line.is_empty() {
                break;
            }
            if headers.len() >= MAX_HEADERS {
                return Err(Raised);
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            if let Some((name, value)) = text.split_once(':') {
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
        }
        if status == 100 {
            continue;
        }
        return Ok(Response { status, headers });
    }
}

/// Reads the body: a chunked body, `Content-Length` bytes (an error when the connection ends
/// first), or everything until the server closes; nothing for 204/304 and 1xx.
fn read_body(reader: &mut impl BufRead, response: &Response) -> Result<Vec<u8>, Raised> {
    if response.status == 204 || response.status == 304 || response.status < 200 {
        return Ok(Vec::new());
    }
    let chunked = response.header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    if chunked {
        let mut body = Vec::new();
        loop {
            let line = read_line(reader)?;
            let text = String::from_utf8_lossy(&line).into_owned();
            let size = text.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size, 16).map_err(|_| Raised)?;
            if size == 0 {
                // The trailer, up to the blank line or the end of the stream.
                loop {
                    let line = read_line(reader)?;
                    if line.is_empty() || line == b"\r\n" || line == b"\n" {
                        return Ok(body);
                    }
                }
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..]).map_err(|_| Raised)?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).map_err(|_| Raised)?;
        }
    }
    let length = response.header("content-length").and_then(|value| value.parse::<usize>().ok());
    let mut body = Vec::new();
    match length {
        Some(length) => {
            body.resize(length, 0);
            reader.read_exact(&mut body).map_err(|_| Raised)?;
        }
        None => {
            reader.read_to_end(&mut body).map_err(|_| Raised)?;
        }
    }
    Ok(body)
}

/// Resolves a redirect target against the current URL: an absolute `http://host[:port]/path`, a
/// network-path `//host/path`, an absolute path, or a relative path. `None` for another scheme,
/// which is not allowed.
fn resolve_redirect(host: &str, port: u16, path: &str, location: &str) -> Option<(String, u16, String)> {
    let split_authority = |rest: &str| -> Option<(String, u16, String)> {
        let (authority, tail) = rest.find('/').map_or((rest, "/"), |at| (&rest[..at], &rest[at..]));
        let (name, port) = match authority.rsplit_once(':') {
            Some((name, port)) => (name.to_owned(), port.parse().ok()?),
            None => (authority.to_owned(), 80),
        };
        Some((name, port, tail.to_owned()))
    };
    if let Some(rest) = location.strip_prefix("http://") {
        return split_authority(rest);
    }
    if location.contains("://") {
        return None;
    }
    if let Some(rest) = location.strip_prefix("//") {
        return split_authority(rest);
    }
    if location.starts_with('/') {
        return Some((host.to_owned(), port, location.to_owned()));
    }
    let base = path.split('?').next().unwrap_or("/");
    let directory = &base[..base.rfind('/').map_or(0, |at| at + 1)];
    Some((host.to_owned(), port, format!("{directory}{location}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A one-shot HTTP server answering each connection with the next scripted response; returns its address and
    /// what the client sent.
    fn serve(responses: Vec<&'static [u8]>) -> (SocketAddr, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    request.push_str(&line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                requests.push(request);
                stream.write_all(response).unwrap();
            }
            requests
        });
        (address, handle)
    }

    fn probe(address: SocketAddr) -> Result<bool, Raised> {
        HttpSimStatus { address }.is_running()
    }

    /// The request: HTTP/1.1 with `Accept-Encoding: identity`, `Host`, `User-Agent` and
    /// `Connection: close`, in that order; `'running:1'` in a 2xx body.
    #[test]
    fn request_and_status_body() {
        let (address, server) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nrunning:1"]);
        assert_eq!(probe(address), Ok(true));
        let requests = server.join().unwrap();
        assert_eq!(
            requests[0],
            format!(
                "GET {SIM_STATUS_PATH} HTTP/1.1\r\nAccept-Encoding: identity\r\nHost: {address}\r\nUser-Agent: Python-urllib/3.13\r\nConnection: close\r\n\r\n"
            )
        );
        let (address, _) = serve(vec![b"HTTP/1.1 200 OK\r\n\r\nrunning:0"]);
        assert_eq!(probe(address), Ok(false));
    }

    /// A chunked body is decoded; a redirect is followed with a new request.
    #[test]
    fn chunked_body_and_redirect() {
        let (address, server) = serve(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /other\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nrunn\r\n5\r\ning:1\r\n0\r\n\r\n",
        ]);
        assert_eq!(probe(address), Ok(true));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /other HTTP/1.1\r\n"));
    }

    /// An error status (non-2xx) and connection failures read false. A broken-off body, a bad status line
    /// and a non-UTF-8 body are errors.
    #[test]
    fn errors() {
        let (address, _) = serve(vec![b"HTTP/1.1 404 Not Found\r\n\r\nrunning:1"]);
        assert_eq!(probe(address), Ok(false));
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert_eq!(probe(closed), Ok(false));
        let (address, _) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nrunning:1"]);
        assert_eq!(probe(address), Err(Raised));
        let (address, _) = serve(vec![b"garbage\r\n\r\n"]);
        assert_eq!(probe(address), Err(Raised));
        let (address, _) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n\xff\xfe"]);
        assert_eq!(probe(address), Err(Raised));
    }

    #[test]
    fn redirect_targets() {
        assert_eq!(resolve_redirect("h", 1, "/a/b?x", "c"), Some(("h".into(), 1, "/a/c".into())));
        assert_eq!(resolve_redirect("h", 1, "/a", "/z"), Some(("h".into(), 1, "/z".into())));
        assert_eq!(resolve_redirect("h", 1, "/a", "http://k:5/q"), Some(("k".into(), 5, "/q".into())));
        assert_eq!(resolve_redirect("h", 1, "/a", "ftp://k/q"), None);
    }
}
