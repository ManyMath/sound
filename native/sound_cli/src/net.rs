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
                        read_header_lines(conn)?;
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

// Caps on the header (and chunked trailer) lines a server may feed us, so a
// hostile or broken server cannot grow memory, or keep us reading, without
// bound.
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_HEADER_LINES: usize = 128;
const MAX_INTERIM_RESPONSES: usize = 8;

/// Reads one CRLF-terminated line (without the CRLF) from a buffered reader.
/// Bytes that are not UTF-8 (servers do send Latin-1 header values, e.g.
/// Icecast's `icy-name`) are replaced rather than failing the response.
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
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Reads header lines up to the empty line that ends them (a response's
/// headers, or a chunked body's trailers), returning lowercased names and
/// trimmed values. Every line counts against [MAX_HEADER_LINES], malformed
/// (colon-less) ones included, so a server cannot stream lines forever.
fn read_header_lines<R: BufRead>(r: &mut R) -> io::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for _ in 0..=MAX_HEADER_LINES {
        let line = read_line(r)?;
        if line.is_empty() {
            return Ok(headers);
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many header lines",
    ))
}

fn root_store() -> &'static Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(store)
    })
}

const MAX_REDIRECTS: usize = 5;

/// Issues a GET request to `url`, sending `headers` (name, value pairs), and
/// returns the parsed response with a `Read` body positioned right after the
/// response headers. Follows up to [MAX_REDIRECTS] redirects (3xx with a
/// `Location` header).
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
    // The original request plus one per followed redirect.
    for _ in 0..=MAX_REDIRECTS {
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

/// Resolves a `Location` value against the URL that returned it, per RFC
/// 3986 section 5.2: absolute (`https://host/x`), scheme-relative
/// (`//host/x`), absolute-path (`/x`), path-relative (`x`, `../x`), and
/// query-only (`?q`) references.
fn resolve_redirect(base: &str, location: &str) -> Result<String, String> {
    if has_scheme(location) {
        // Absolute; parse_url rejects any scheme other than http(s) later.
        return Ok(location.to_string());
    }
    let parsed = parse_url(base)?;
    let scheme = if parsed.tls { "https" } else { "http" };
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{rest}"));
    }
    // A fragment never reaches the server, so drop it before merging.
    let location = location.split('#').next().unwrap_or("");
    let (base_path, _) = split_query(&parsed.path);
    let target = if location.is_empty() {
        parsed.path.clone()
    } else if location.starts_with('?') {
        format!("{base_path}{location}")
    } else {
        let (path, query) = split_query(location);
        let merged = if path.starts_with('/') {
            path.to_string()
        } else {
            // Relative to the base path's "directory" (up to its last '/';
            // parse_url guarantees the path starts with one).
            let dir = &base_path[..=base_path.rfind('/').unwrap_or(0)];
            format!("{dir}{path}")
        };
        let mut target = remove_dot_segments(&merged);
        if let Some(query) = query {
            target.push('?');
            target.push_str(query);
        }
        target
    };
    Ok(format!("{scheme}://{}{target}", parsed.host_header()))
}

/// Whether `reference` starts with a URI scheme (`ALPHA *(ALPHA / DIGIT /
/// "+" / "-" / ".") ":"`), i.e. is an absolute URL rather than a relative one.
fn has_scheme(reference: &str) -> bool {
    let Some((scheme, _)) = reference.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// Splits `path?query` into the path and the query (without its `?`).
fn split_query(reference: &str) -> (&str, Option<&str>) {
    match reference.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (reference, None),
    }
}

/// Resolves `.` and `..` segments in an absolute path (RFC 3986 section
/// 5.2.4); `..` never climbs above the root.
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        match *segment {
            "." => {}
            ".." => {
                out.pop();
            }
            segment => {
                out.push(segment);
                continue;
            }
        }
        // A trailing dot segment still names a directory: keep the final '/'.
        if i + 1 == segments.len() {
            out.push("");
        }
    }
    format!("/{}", out.join("/"))
}

/// Strips `scheme` (e.g. `https://`) from the front of `url`, ignoring ASCII
/// case as RFC 3986 requires for schemes.
fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let head = url.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| &url[scheme.len()..])
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
    let (tls, rest) = if let Some(rest) = strip_scheme(url, "https://") {
        (true, rest)
    } else if let Some(rest) = strip_scheme(url, "http://") {
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

    // Interim 1xx responses (100 Continue, 103 Early Hints) may precede the
    // real one and have no body: skip them. 101 Switching Protocols is final
    // (and never asked for here), so it is returned like any other status.
    let mut interim = 0;
    let (status, response_headers) = loop {
        let status_line = read_line(&mut conn).map_err(|e| format!("read status line: {e}"))?;
        let status = parse_status(&status_line)?;
        let headers = read_header_lines(&mut conn).map_err(|e| format!("read headers: {e}"))?;
        if !(100..200).contains(&status) || status == 101 {
            break (status, headers);
        }
        interim += 1;
        if interim > MAX_INTERIM_RESPONSES {
            return Err("too many interim (1xx) responses".to_string());
        }
    };

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
    fn serve_once(response: impl AsRef<[u8]> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard); // drain the request
            stream.write_all(response.as_ref()).unwrap();
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

    /// Serves a chain of `redirects` same-origin 302s (`/r0` -> `/r1` -> ...)
    /// ending in a 200, returning the URL of the first hop.
    fn serve_redirect_chain(redirects: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for i in 0..=redirects {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut discard = [0u8; 4096];
                let _ = stream.read(&mut discard);
                let response = if i < redirects {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: /r{}\r\nContent-Length: 0\r\n\r\n",
                        i + 1
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string()
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/r0")
    }

    #[test]
    fn follows_exactly_max_redirects() {
        let response = get(&serve_redirect_chain(MAX_REDIRECTS), &[]).unwrap();
        assert_eq!(response.status, 200);
        assert!(get(&serve_redirect_chain(MAX_REDIRECTS + 1), &[]).is_err());
    }

    #[test]
    fn tolerates_non_utf8_header_values() {
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nicy-name: Caf".to_vec();
        response.push(0xE9); // Latin-1 e-acute
        response.extend_from_slice(b" Radio\r\n\r\nhello");
        let mut response = get(&format!("{}/track", serve_once(response)), &[]).unwrap();
        assert_eq!(response.header("icy-name"), Some("Caf\u{FFFD} Radio"));
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn caps_header_lines_including_malformed_ones() {
        // Colon-less lines used to be skipped without counting, so a server
        // could send them forever.
        let mut response = b"HTTP/1.1 200 OK\r\n".to_vec();
        for _ in 0..=MAX_HEADER_LINES {
            response.extend_from_slice(b"x\r\n");
        }
        response.extend_from_slice(b"Content-Length: 2\r\n\r\nok");
        assert!(get(&format!("{}/track", serve_once(response)), &[]).is_err());
    }

    #[test]
    fn caps_chunked_trailer_lines() {
        let mut response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n".to_vec();
        for _ in 0..=MAX_HEADER_LINES {
            response.extend_from_slice(b"X-Trailer: 1\r\n");
        }
        response.extend_from_slice(b"\r\n");
        let mut response = get(&format!("{}/track", serve_once(response)), &[]).unwrap();
        let mut body = Vec::new();
        let err = response.read_to_end(&mut body).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn skips_interim_1xx_responses() {
        let base = serve_once(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
        );
        let mut response = get(&format!("{base}/track"), &[]).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header("link"), None);
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"hello");

        let endless = b"HTTP/1.1 103 Early Hints\r\n\r\n".repeat(MAX_INTERIM_RESPONSES + 1);
        assert!(get(&format!("{}/track", serve_once(endless)), &[]).is_err());
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
    fn follows_scheme_relative_redirect_to_its_host() {
        // `Location: //host/x` names another host; it used to be resolved
        // as a path on the origin (`GET //host/x`).
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_addr = target.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            let mut discard = [0u8; 4096];
            let _ = stream.read(&mut discard);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nreal")
                .unwrap();
        });
        let base = serve_once(format!(
            "HTTP/1.1 302 Found\r\nLocation: //{target_addr}/real\r\nContent-Length: 0\r\n\r\n"
        ));
        let mut response = get(&format!("{base}/track"), &[]).unwrap();
        let mut body = Vec::new();
        response.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"real");
    }

    #[test]
    fn resolves_relative_redirect_references() {
        let base = "http://example.com:8080/a/b/track.mp3?x=1";
        for (location, want) in [
            ("https://cdn.example.com/x", "https://cdn.example.com/x"),
            ("HTTP://cdn.example.com/x", "HTTP://cdn.example.com/x"),
            ("//cdn.example.com/x?y", "http://cdn.example.com/x?y"),
            ("/root", "http://example.com:8080/root"),
            ("other.mp3", "http://example.com:8080/a/b/other.mp3"),
            (
                "./c/other.mp3?q",
                "http://example.com:8080/a/b/c/other.mp3?q",
            ),
            ("../other.mp3", "http://example.com:8080/a/other.mp3"),
            ("../../../../x", "http://example.com:8080/x"),
            ("..", "http://example.com:8080/a/"),
            ("/a/./b/../c", "http://example.com:8080/a/c"),
            ("?y=2", "http://example.com:8080/a/b/track.mp3?y=2"),
            ("other.mp3#frag", "http://example.com:8080/a/b/other.mp3"),
        ] {
            assert_eq!(
                resolve_redirect(base, location).unwrap(),
                want,
                "{location}"
            );
        }
        assert!(parse_url("HTTPS://Example.com/a").unwrap().tls);
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
