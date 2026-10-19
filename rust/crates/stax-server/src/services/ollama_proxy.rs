//! `routes/misc.py::ollama_proxy` — one httpx call, on a raw socket.
//!
//! ```python
//! ollama_url = f"http://localhost:11434/api/{path}"
//! async with httpx.AsyncClient(timeout=120.0) as client:
//!     response = await client.request(method=…, url=…, content=body, headers=…)
//! ```
//!
//! # Why this is a `TcpStream` and not one line of `reqwest`
//!
//! `rust/ARCHITECT-STATE.md` finding 12 already paid for this answer once, for
//! `parity/src/http.rs`: every mainstream client transparently decompresses,
//! follows redirects, retries, normalises header casing, or coalesces
//! `Transfer-Encoding`. Each of those turns a real divergence into a green tick.
//! The proxy's *whole job* is to look at `transfer-encoding` and `content-type`
//! on the raw response and branch, so a client that helpfully hides either one
//! would be deciding the branch for us.
//!
//! And the fence: `stax-server` has no HTTP-client dependency, the batch-E claim
//! forbids `Cargo.toml` edits, and adding one to proxy to a port that is closed
//! on this host would be buying a large dependency tree to reach an
//! `ECONNREFUSED`.
//!
//! # What this module does NOT reproduce
//!
//! httpx merges its own default headers *under* the caller's
//! (`accept: */*`, `accept-encoding: …`, `connection: keep-alive`,
//! `user-agent: python-httpx/…`) before sending. Those are not synthesised
//! here. They are invisible to the differ — they only ever reach *Ollama*, and
//! Ollama is the one participant the parity harness does not compare — but they
//! are a real difference in the bytes that would leave the machine, so they are
//! named rather than assumed away. See `parity/DIV-e-misc.md`.
//!
//! # The one thing measured before any of it was written
//!
//! Port 11434 is closed on this host (`ss -lnt` shows no listener; a TCP connect
//! to both `127.0.0.1:11434` and `[::1]:11434` is refused; `getent ahosts
//! localhost` resolves to `127.0.0.1` only). So the only branch either side can
//! reach is the bare `except Exception` — [`ProxyOutcome::Unavailable`] — and
//! that is what makes `M-ollama` a deterministic case row instead of a `!` one.
//! **If Ollama is ever running, that row stops being deterministic**: the body
//! becomes whatever the daemon answers, which depends on the models installed.
//! `parity/DIV-e-misc.md` records the consequence; the row does not self-guard.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `http://localhost:11434/…` — the authority, spelled the way Python spells it.
///
/// `localhost`, not `127.0.0.1`: the f-string says `localhost`, so the name is
/// resolved by the OS resolver on every call exactly as httpx resolves it. On a
/// host where `localhost` also carries `::1` that is two candidate addresses,
/// and hard-coding the v4 literal would quietly change which one is tried.
pub const UPSTREAM_HOST: &str = "localhost";
/// The port half of the authority.
pub const UPSTREAM_PORT: u16 = 11434;

/// `httpx.AsyncClient(timeout=120.0)`.
///
/// httpx's scalar timeout sets connect/read/write/pool to 120 s *each*, so it is
/// applied to the connect, the write and the read separately here rather than as
/// one deadline over the whole exchange.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// `if k.lower() not in ('host', 'content-length')` — the two dropped headers.
///
/// `host` because the URL supplies a new authority and `content-length` because
/// the entity is re-framed; both comparisons are on the LOWERCASED name, which
/// is why this list is lowercase and the filter lowercases its input.
const DROPPED_HEADERS: [&str; 2] = ["host", "content-length"];

/// One upstream response, kept as it arrived.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// The numeric status.
    pub status: u16,
    /// Header names lowercased, values verbatim, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The entity, de-chunked if it arrived chunked. Never decompressed —
    /// httpx would decompress, but `response.json()` is the only consumer and
    /// Ollama does not negotiate compression with a client that asks for none.
    pub body: Vec<u8>,
}

impl Upstream {
    /// The first value for `name`, which is already lowercase.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Which of `ollama_proxy`'s three exits the call reached.
#[derive(Debug, Clone)]
pub enum ProxyOutcome {
    /// `JSONResponse(content=body, status_code=response.status_code)`.
    ///
    /// `body` is `response.json()` when the upstream `content-type` starts with
    /// `application/json`, and `{}` otherwise.
    Json {
        /// The upstream status, forwarded verbatim.
        status: u16,
        /// The decoded entity, or an empty object.
        body: Value,
    },
    /// The `transfer-encoding: chunked` leg — starlette's `StreamingResponse`
    /// with `headers=dict(response.headers)`.
    ///
    /// Unreachable on this host and therefore unmeasured: it carries no case row
    /// (batch-E law 4 forbids rows for streams outright) and the exact header
    /// set starlette emits for it is a reading of the source, not a probe.
    Stream {
        /// The upstream status.
        status: u16,
        /// `dict(response.headers)`, forwarded as-is.
        headers: Vec<(String, String)>,
        /// The de-chunked entity.
        body: Vec<u8>,
    },
    /// `except Exception: JSONResponse({"error": "Ollama not available"}, 502)`.
    ///
    /// A *bare* catch-all in Python, so this is every failure there is: refused
    /// connection, DNS failure, timeout, malformed response, and — because
    /// `response.json()` is inside the `try` — an upstream that claims
    /// `application/json` and sends something that is not.
    Unavailable,
}

/// `{k: v for k, v in request.headers.items() if k.lower() not in (…)}`.
///
/// Starlette's `request.headers.items()` yields every header in arrival order
/// with the name already lowercased, duplicates included — it is a multidict,
/// not a map, so two `accept` headers forward as two headers. axum's
/// `HeaderMap::iter` has the same shape, so the port is the filter and nothing
/// else.
#[must_use]
pub fn forwarded_headers(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            if DROPPED_HEADERS.contains(&name.as_str()) {
                return None;
            }
            // A header whose bytes are not valid UTF-8 does survive
            // `request.headers.items()` — starlette decodes latin-1 — so this
            // filter is NOT identical there. A non-UTF-8 header never reaches
            // this route from the dashboard, and inventing a latin-1 transcoder
            // for it would be unmeasured code; recorded instead.
            let value = value.to_str().ok()?;
            Some((name, value.to_owned()))
        })
        .collect()
}

/// Issue the proxied request and classify the result.
///
/// Never returns an error: Python's `except Exception` swallows every failure
/// into one 502, so "failed" is the [`ProxyOutcome::Unavailable`] variant.
pub async fn proxy(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> ProxyOutcome {
    let Ok(upstream) = send(method, path, headers, body).await else {
        return ProxyOutcome::Unavailable;
    };

    // `response.headers.get("transfer-encoding") == "chunked"` — an EXACT
    // equality on the value, not a substring test. `chunked, gzip` does not
    // match it and neither does `Chunked`: httpx lowercases header NAMES, not
    // values, so the comparison stays byte-exact here too.
    if upstream.header("transfer-encoding") == Some("chunked") {
        return ProxyOutcome::Stream {
            status: upstream.status,
            headers: upstream.headers,
            body: upstream.body,
        };
    }

    // `ct = response.headers.get("content-type", "")` then `ct.startswith(…)`.
    let content_type = upstream.header("content-type").unwrap_or_default();
    let body = if content_type.starts_with("application/json") {
        // `response.json()` — a parse failure raises INSIDE the `try`, so it is
        // the 502, not an empty object.
        match serde_json::from_slice::<Value>(&upstream.body) {
            Ok(value) => value,
            Err(_) => return ProxyOutcome::Unavailable,
        }
    } else {
        Value::Object(serde_json::Map::new())
    };
    ProxyOutcome::Json {
        status: upstream.status,
        body,
    }
}

/// The socket half: one request, one response, no cleverness.
///
/// # Errors
/// Connect, write, read, timeout and framing failures — all of which the caller
/// folds into the single 502 the reference produces.
pub async fn send(
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<Upstream> {
    connect_and_send(UPSTREAM_HOST, UPSTREAM_PORT, method, path, headers, body).await
}

/// [`send`], with the authority injected so the tests can stand up a real
/// upstream on an ephemeral port instead of asserting against 11434.
async fn connect_and_send(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<Upstream> {
    let target = format!("/api/{path}");
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))??;

    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    // `httpx._content.encode_content` emits `Content-Length` only for a
    // NON-EMPTY body: `headers = {"Content-Length": str(len(body))} if body
    // else {}`. So a proxied `GET` with no entity carries no `content-length` at
    // all, and adding one "for correctness" would be a byte the reference does
    // not send.
    if !body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    tokio::time::timeout(TIMEOUT, async {
        stream.write_all(head.as_bytes()).await?;
        if !body.is_empty() {
            stream.write_all(body).await?;
        }
        stream.flush().await
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))??;

    let raw = tokio::time::timeout(TIMEOUT, read_message(&mut stream))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out"))??;
    parse_response(&raw)
}

/// Read until the framing says the message is complete, or until EOF.
///
/// Stopping on the framing rather than only on EOF is what keeps a keep-alive
/// upstream from holding the call open for the full 120 s. httpx does the same
/// thing; it just does it behind a connection pool this module does not have.
async fn read_message(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..read]);
        if framed_end(&buf) {
            return Ok(buf);
        }
    }
}

/// True once `buf` holds a complete HTTP message by its own framing headers.
fn framed_end(buf: &[u8]) -> bool {
    let Some(head_end) = find_subslice(buf, b"\r\n\r\n") else {
        return false;
    };
    let body = &buf[head_end + 4..];
    let Ok(head) = std::str::from_utf8(&buf[..head_end]) else {
        return false;
    };
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().ok(),
            "transfer-encoding" => chunked = value.trim().eq_ignore_ascii_case("chunked"),
            _ => {}
        }
    }
    if chunked {
        return find_subslice(body, b"0\r\n\r\n").is_some();
    }
    content_length.is_some_and(|len| body.len() >= len)
}

/// Split the raw bytes into a status, lowercased headers, and a decoded entity.
fn parse_response(raw: &[u8]) -> std::io::Result<Upstream> {
    let invalid = |what: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, what);
    let head_end =
        find_subslice(raw, b"\r\n\r\n").ok_or_else(|| invalid("no header terminator"))?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| invalid("non-utf8 headers"))?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1).map(str::to_owned))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid("no status code"))?;

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let raw_body = &raw[head_end + 4..];
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(raw_body)?
    } else if let Some(len) = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
    {
        raw_body[..len.min(raw_body.len())].to_vec()
    } else {
        raw_body.to_vec()
    };

    Ok(Upstream {
        status,
        headers,
        body,
    })
}

/// `Transfer-Encoding: chunked`, decoded. Extensions after the size are ignored,
/// which is what the size grammar allows and what every decoder does.
fn dechunk(mut body: &[u8]) -> std::io::Result<Vec<u8>> {
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed chunked body");
    let mut out = Vec::new();
    loop {
        let line_end = find_subslice(body, b"\r\n").ok_or_else(invalid)?;
        let header = std::str::from_utf8(&body[..line_end]).map_err(|_| invalid())?;
        let size_text = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid())?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err(invalid());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// `bytes.find` — no dependency, no allocation.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn headers_of(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                axum::http::HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    /// Serve one canned response on an ephemeral port and hand back the port
    /// plus the exact request bytes the client sent.
    async fn one_shot_upstream(canned: &'static [u8]) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut seen = Vec::new();
            let mut chunk = [0_u8; 4096];
            // TCP can split the head and body even on loopback. Read the
            // complete Content-Length-framed request before replying.
            loop {
                let read = socket.read(&mut chunk).await.expect("read");
                assert!(read > 0, "request ended before its complete frame");
                seen.extend_from_slice(&chunk[..read]);
                if let Some(head_end) = seen.windows(4).position(|v| v == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&seen[..head_end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                        .unwrap_or(0);
                    if seen.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            socket.write_all(canned).await.expect("write");
            socket.flush().await.expect("flush");
            seen
        });
        (port, handle)
    }

    #[test]
    fn host_and_content_length_are_dropped_case_insensitively() {
        let map = headers_of(&[
            ("Host", "127.0.0.1:8081"),
            ("Content-Length", "12"),
            ("Content-Type", "application/json"),
            ("X-Keep", "yes"),
        ]);
        assert_eq!(
            forwarded_headers(&map),
            vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-keep".to_owned(), "yes".to_owned()),
            ]
        );
    }

    #[test]
    fn a_repeated_header_forwards_twice_because_the_source_is_a_multidict() {
        let map = headers_of(&[("Accept", "text/plain"), ("Accept", "application/json")]);
        assert_eq!(forwarded_headers(&map).len(), 2);
    }

    #[test]
    fn a_content_length_framed_response_parses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"models\":[]}";
        let upstream = parse_response(raw).expect("parses");
        assert_eq!(upstream.status, 200);
        assert_eq!(upstream.header("content-type"), Some("application/json"));
        assert_eq!(upstream.body, b"{\"models\":[]}");
    }

    #[test]
    fn a_chunked_response_is_dechunked_and_flagged() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n2\r\n!!\r\n0\r\n\r\n";
        let upstream = parse_response(raw).expect("parses");
        assert_eq!(upstream.header("transfer-encoding"), Some("chunked"));
        assert_eq!(upstream.body, b"hello!!");
    }

    #[test]
    fn a_chunk_extension_after_the_size_is_ignored() {
        assert_eq!(
            dechunk(b"3;name=v\r\nabc\r\n0\r\n\r\n").expect("decodes"),
            b"abc"
        );
    }

    #[test]
    fn framing_completion_needs_the_terminal_chunk() {
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n";
        assert!(!framed_end(head));
        let mut whole = head.to_vec();
        whole.extend_from_slice(b"0\r\n\r\n");
        assert!(framed_end(&whole));
    }

    /// The reference answers 502 because the connection is refused. This proves
    /// the failure path itself WITHOUT depending on 11434 being closed: bind an
    /// ephemeral port, drop the listener, then connect.
    #[tokio::test]
    async fn a_refused_connection_is_a_transport_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let outcome = connect_and_send("127.0.0.1", port, "GET", "tags", &[], b"").await;
        assert!(outcome.is_err(), "port {port} should refuse");
    }

    #[tokio::test]
    async fn an_empty_body_carries_no_content_length_header() {
        let (port, handle) = one_shot_upstream(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        let sent_headers = vec![("accept".to_owned(), "*/*".to_owned())];
        let upstream = connect_and_send("127.0.0.1", port, "GET", "tags", &sent_headers, b"")
            .await
            .expect("round trip");
        assert_eq!(upstream.status, 200);
        let request = String::from_utf8(handle.await.expect("join")).expect("utf8");
        assert!(
            request.starts_with("GET /api/tags HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains("accept: */*\r\n"), "{request}");
        assert!(
            !request.to_ascii_lowercase().contains("content-length"),
            "an empty entity must not be framed: {request}"
        );
    }

    #[tokio::test]
    async fn a_non_empty_body_is_framed_and_forwarded() {
        let (port, handle) = one_shot_upstream(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"done\":true}",
        )
        .await;
        let body = br#"{"model":"x"}"#;
        let upstream = connect_and_send("127.0.0.1", port, "POST", "generate", &[], body)
            .await
            .expect("round trip");
        assert_eq!(upstream.status, 200);
        let request = String::from_utf8(handle.await.expect("join")).expect("utf8");
        assert!(
            request.contains(&format!("Content-Length: {}\r\n", body.len())),
            "{request}"
        );
        assert!(request.ends_with(r#"{"model":"x"}"#), "{request}");
    }

    #[test]
    fn an_absent_content_type_is_the_empty_string_not_a_failure() {
        let bare = Upstream {
            status: 500,
            headers: vec![],
            body: b"boom".to_vec(),
        };
        assert_eq!(bare.header("content-type").unwrap_or_default(), "");
    }
}
