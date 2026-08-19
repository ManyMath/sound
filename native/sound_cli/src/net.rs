//! Minimal blocking HTTP/1.1 GET client (plain or TLS), used to stream a
//! track's bytes for playback without buffering the whole response first.
//!
//! Deliberately not a general HTTP client: one method (GET), one concern
//! (read a response, optionally with custom headers, as a byte stream). Pulls
//! in `rustls`/`webpki-roots` for TLS instead of a system TLS library, per the
//! project's "no dependency beyond Rust itself" constraint.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// A parsed HTTP response: status code, headers (lowercased names), and a
/// reader positioned at the start of the body.
pub struct Response {
    pub status: u16,
    /// Lowercased header name -> value, in response order.
    pub headers: Vec<(String, String)>,
    body: Body,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.body.read(buf)
    }
}

/// Either a plain TCP connection or a TLS one, behind one `Read` impl.
enum Conn {
    Plain(BufReader<TcpStream>),
    Tls(Box<BufReader<StreamOwned<ClientConnection, TcpStream>>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(r) => r.read(buf),
            Conn::Tls(r) => r.read(buf),
        }
    }
}

impl BufRead for Conn {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self {
            Conn::Plain(r) => r.fill_buf(),
            Conn::Tls(r) => r.fill_buf(),
        }
    }
    fn consume(&mut self, amt: usize) {
        match self {
            Conn::Plain(r) => r.consume(amt),
            Conn::Tls(r) => r.consume(amt),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(r) => r.get_mut().write(buf),
            Conn::Tls(r) => r.get_mut().write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(r) => r.get_mut().flush(),
            Conn::Tls(r) => r.get_mut().flush(),
        }
    }
}

/// A response body, handling both `Content-Length` and
/// `Transfer-Encoding: chunked` framing over the same connection.
enum Body {
    Fixed {
        conn: Conn,
        remaining: u64,
    },
    Chunked {
        conn: Conn,
        remaining: u64,
        done: bool,
    },
    /// No framing header present; read until the connection closes (only
    /// valid when the server also sends `Connection: close`).
    UntilClose {
        conn: Conn,
    },
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Fixed { conn, remaining } => {
                if *remaining == 0 || buf.is_empty() {
                    return Ok(0);
                }
                let cap = (*remaining).min(buf.len() as u64) as usize;
                let n = conn.read(&mut buf[..cap])?;
                if n == 0 {
                    // The connection closed before Content-Length bytes
                    // arrived; that is a truncated download, not end-of-body.
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("body truncated ({remaining} bytes missing)"),
                    ));
                }
                *remaining -= n as u64;
                Ok(n)
            }
            Body::Chunked {
                conn,
                remaining,
                done,
            } => {
                if *done || buf.is_empty() {
                    return Ok(0);
                }
                if *remaining == 0 {
                    // Start of a new chunk: read its size line.
                    let size_line = read_line(conn)?;
                    let size_str = size_line.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size_str, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "bad chunk size")
                    })?;
                    if size == 0 {
                        // Terminal chunk: consume the trailing CRLF (and any
                        // trailer headers, which this client ignores) up to
                        // the empty line that ends the response.
                        loop {
                            let line = read_line(conn)?;
                            if line.is_empty() {
                                break;
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                    *remaining = size;
                }
                let cap = (*remaining).min(buf.len() as u64) as usize;
                let n = conn.read(&mut buf[..cap])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed mid-chunk",
                    ));
                }
                *remaining -= n as u64;
                if *remaining == 0 {
                    // Consume the CRLF that follows every chunk's data.
                    let mut crlf = [0u8; 2];
                    conn.read_exact(&mut crlf)?;
                }
                Ok(n)
            }
            Body::UntilClose { conn } => conn.read(buf),
        }
    }
}

// Caps on what a server may feed us before the body starts, so a hostile or
// broken server cannot grow memory without bound.
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_HEADER_LINES: usize = 128;

/// Reads one CRLF-terminated line (without the CRLF) from a buffered reader.
fn read_line<R: BufRead>(r: &mut R) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let n = r.read(&mut byte)?;
        if n == 0 {
            if line.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "eof in headers",
                ));
            }
            break;
        }
        if byte[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            break;
        }
        if line.len() >= MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header line too long",
            ));
        }
        line.push(byte[0]);
    }
    String::from_utf8(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn root_store() -> &'static Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(store)
    })
}

/// Issues a GET request to `url`, sending `headers` (name, value pairs), and
/// returns the parsed response with a `Read` body positioned right after the
/// response headers. Follows up to 5 redirects (3xx with a `Location`
/// header).
///
/// Caller headers (which typically carry credentials, e.g. `Authorization`)
/// are only sent to the origin of the original URL: a redirect to a different
/// scheme, host, or port drops them for that hop and every hop after it, so a
/// bearer token can never leak to a third-party host or over a downgraded
/// plain-HTTP connection.
///
/// `url` must be `http://` or `https://`. A connect/read timeout of 15s
/// applies to each underlying socket operation.
pub fn get(url: &str, headers: &[(String, String)]) -> Result<Response, String> {
    let mut current = url.to_string();
    let mut send_headers = true;
    for _ in 0..5 {
        let response = get_once(&current, if send_headers { headers } else { &[] })?;
        if (300..400).contains(&response.status) {
            if let Some(location) = response.header("location") {
                let next = resolve_redirect(&current, location)?;
                if !same_origin(&current, &next)? {
                    send_headers = false;
                }
                current = next;
                continue;
            }
        }
        return Ok(response);
    }
    Err("too many redirects".to_string())
}

/// Whether two URLs share scheme, host (case-insensitive), and port.
fn same_origin(a: &str, b: &str) -> Result<bool, String> {
    let pa = parse_url(a)?;
    let pb = parse_url(b)?;
    Ok(pa.tls == pb.tls && pa.host.eq_ignore_ascii_case(&pb.host) && pa.port == pb.port)
}

fn resolve_redirect(base: &str, location: &str) -> Result<String, String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    let parsed = parse_url(base)?;
    let scheme = if parsed.tls { "https" } else { "http" };
    if location.starts_with('/') {
        Ok(format!("{scheme}://{}{location}", parsed.host_header()))
    } else {
        Err(format!("unsupported relative redirect: {location}"))
    }
}

struct ParsedUrl {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl ParsedUrl {
    fn host_header(&self) -> String {
        match (self.tls, self.port) {
            (true, 443) | (false, 80) => self.host.clone(),
            _ => format!("{}:{}", self.host, self.port),
        }
    }
}

fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!("unsupported URL scheme: {url}"));
    };
    // The authority ends at the first path/query/fragment delimiter; a URL
    // like `http://host?q=1` has an implicit "/" path.
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_string()),
    };
    // A fragment is client-side only; never send it.
    let path = path.split('#').next().unwrap_or("/").to_string();
    if path.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return Err("control characters or spaces in URL path".to_string());
    }
    // Reject the authority forms this minimal client does not speak, loudly,
    // instead of mangling them.
    if authority.contains('@') {
        return Err("userinfo (user@host) in URLs is not supported".to_string());
    }
    if authority.starts_with('[') {
        return Err("IPv6 literal hosts are not supported".to_string());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "bad port".to_string())?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    Ok(ParsedUrl {
        tls,
        host: host.to_string(),
        port,
        path,
    })
}

const IO_TIMEOUT: Duration = Duration::from_secs(15);

fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    // connect_timeout needs resolved addresses. (DNS resolution itself has no
    // std timeout; the OS resolver's applies.)
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?;
    let mut last_err = format!("no addresses for {host}:{port}");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, IO_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = format!("connect {addr}: {e}"),
        }
    }
    Err(last_err)
}

fn get_once(url: &str, headers: &[(String, String)]) -> Result<Response, String> {
    let parsed = parse_url(url)?;
    // Refuse header names/values that could inject request lines.
    for (name, value) in headers {
        let name_ok = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !name_ok {
            return Err(format!("invalid header name: {name:?}"));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(format!("header {name} value contains control characters"));
        }
    }
    let tcp = connect(parsed.host.as_str(), parsed.port)?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("set read timeout: {e}"))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("set write timeout: {e}"))?;

    let mut conn = if parsed.tls {
        let config = ClientConfig::builder()
            .with_root_certificates(root_store().clone())
            .with_no_client_auth();
        let server_name = ServerName::try_from(parsed.host.clone())
            .map_err(|e| format!("invalid server name {}: {e}", parsed.host))?;
        let client = ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| format!("tls setup: {e}"))?;
        Conn::Tls(Box::new(BufReader::new(StreamOwned::new(client, tcp))))
    } else {
        Conn::Plain(BufReader::new(tcp))
    };

    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: sound_cli/0.1\r\nAccept: */*\r\n",
        parsed.path,
        parsed.host_header(),
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    conn.write_all(request.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;
    conn.flush().map_err(|e| format!("flush request: {e}"))?;

    let status_line = read_line(&mut conn).map_err(|e| format!("read status line: {e}"))?;
    let status = parse_status(&status_line)?;

    let mut response_headers = Vec::new();
    loop {
        let line = read_line(&mut conn).map_err(|e| format!("read header line: {e}"))?;
        if line.is_empty() {
            break;
        }
        if response_headers.len() >= MAX_HEADER_LINES {
            return Err("too many response headers".to_string());
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        response_headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    let body = body_for(conn, &response_headers);
    Ok(Response {
        status,
        headers: response_headers,
        body,
    })
}

fn body_for(conn: Conn, headers: &[(String, String)]) -> Body {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    if let Some(encoding) = get("transfer-encoding") {
        if encoding.eq_ignore_ascii_case("chunked") {
            return Body::Chunked {
                conn,
                remaining: 0,
                done: false,
            };
        }
    }
    if let Some(len) = get("content-length").and_then(|v| v.parse::<u64>().ok()) {
        return Body::Fixed {
            conn,
            remaining: len,
        };
    }
    Body::UntilClose { conn }
}

fn parse_status(line: &str) -> Result<u16, String> {
    let mut parts = line.split_whitespace();
    let _version = parts
        .next()
        .ok_or_else(|| "empty status line".to_string())?;
    let code = parts
        .next()
        .ok_or_else(|| format!("no status code in: {line}"))?;
    code.parse::<u16>()
        .map_err(|_| format!("invalid status code: {code}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Spawns a one-shot HTTP server on localhost that writes `response` raw
    /// bytes to the first connection it accepts, then returns its URL prefix
    /// (`http://127.0.0.1:PORT`).
    fn serve_once(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard); // drain the request
            stream.write_all(response).unwrap();
        });
        format!("http://{addr}")
    }

    #[test]
    fn reads_content_length_body() {
        let base = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: audio/wav\r\n\r\nhello",
        );
        let mut response = get(&format!("{base}/track"), &[]).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header("content-type"), Some("audio/wav"));
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn reads_chunked_body() {
        let base = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        let mut response = get(&format!("{base}/track"), &[]).unwrap();
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn sends_custom_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let saw_auth = request.contains("Authorization: Bearer test-token");
            let body = if saw_auth {
                b"ok".as_slice()
            } else {
                b"no".as_slice()
            };
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                )
                .unwrap();
            stream.write_all(body).unwrap();
        });
        let base = format!("http://{addr}");
        let mut response = get(
            &format!("{base}/track"),
            &[("Authorization".to_string(), "Bearer test-token".to_string())],
        )
        .unwrap();
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"ok");
    }

    #[test]
    fn follows_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            // First connection: redirect.
            let (mut stream, _) = listener.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /real\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            drop(stream);
            // Second connection: the real resource.
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.read(&mut discard);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nreal")
                .unwrap();
        });
        let base = format!("http://{addr}");
        let mut response = get(&format!("{base}/track"), &[]).unwrap();
        assert_eq!(response.status, 200);
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"real");
    }

    #[test]
    fn rejects_unsupported_scheme() {
        assert!(get("ftp://example.com/x", &[]).is_err());
    }

    #[test]
    fn errors_on_truncated_content_length_body() {
        // The server promises 10 bytes but closes after 5; that must surface
        // as an error, not a clean end-of-body.
        let base = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello");
        let mut response = get(&format!("{base}/track"), &[]).unwrap();
        let mut body = Vec::new();
        let err = response.read_to_end(&mut body).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn drops_caller_headers_on_cross_origin_redirect() {
        // Target host: reports whether the request carried an Authorization
        // header. It is on a different port than the origin, so it is a
        // different origin and must not receive the credential.
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_addr = target.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let body = if request.to_ascii_lowercase().contains("authorization") {
                b"leaked".as_slice()
            } else {
                b"clean".as_slice()
            };
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
                )
                .unwrap();
            stream.write_all(body).unwrap();
        });
        // Origin host: redirects to the target.
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_addr = origin.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/real\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
        });
        let mut response = get(
            &format!("http://{origin_addr}/track"),
            &[("Authorization".to_string(), "Bearer secret".to_string())],
        )
        .unwrap();
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"clean");
    }

    #[test]
    fn rejects_header_injection() {
        assert!(get(
            "http://127.0.0.1:1/x",
            &[("X-Ok".to_string(), "v\r\nInjected: yes".to_string())],
        )
        .is_err());
        assert!(get(
            "http://127.0.0.1:1/x",
            &[("Bad Name".to_string(), "v".to_string())],
        )
        .is_err());
    }

    #[test]
    fn parses_query_without_path_and_strips_fragments() {
        let u = parse_url("http://example.com?q=1").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.path, "/?q=1");
        let u = parse_url("https://example.com/a#frag").unwrap();
        assert_eq!(u.path, "/a");
        assert!(parse_url("http://user:pass@example.com/").is_err());
        assert!(parse_url("http://[::1]:8080/").is_err());
    }

    /// Exercises the real TLS path against a live host. Ignored by default
    /// (no network access in most CI sandboxes); run explicitly with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn fetches_over_real_tls() {
        let mut response = get("https://example.com/", &[]).unwrap();
        assert_eq!(response.status, 200);
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert!(body.windows(5).any(|w| w == b"<html" || w == b"<!DOC"));
    }
}
