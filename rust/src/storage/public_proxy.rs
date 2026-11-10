//! Public text response negotiation and fixed-origin image proxy. No credential forwarding.
use super::*;
use crate::public_access::Policy;
use flate2::{read::GzEncoder, Compression};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tiny_http::{Header, Method as HttpMethod, Request, Response as Out, Server, StatusCode};

pub(crate) fn field<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}
fn header(name: &str, value: &str) -> Result<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .map_err(|_| anyhow!("invalid public header"))
}
pub(crate) fn reject(request: Request, status: u16) {
    let mut out = Out::empty(StatusCode(status));
    for (key, value) in [
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Content-Security-Policy", CSP),
        ("Allow", "GET, HEAD"),
    ] {
        if let Ok(value) = header(key, value) {
            out.add_header(value);
        }
    }
    let _ = request.respond(out);
}
fn quality(s: &str) -> Option<u16> {
    if s == "0" {
        return Some(0);
    }
    if s == "1" {
        return Some(1000);
    }
    let (whole, fraction) = s.split_once('.')?;
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => Some(if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u16>().ok()? * 10u16.pow(3 - fraction.len() as u32)
        }),
        "1" if fraction.bytes().all(|b| b == b'0') => Some(1000),
        _ => None,
    }
}
pub(crate) fn accepts_gzip(value: &str) -> bool {
    let mut gzip = None;
    let mut wildcard = None;
    for coding in value.split(',') {
        let mut parts = coding.trim().split(';');
        let name = parts.next().unwrap_or("").trim();
        let mut q = Some(1000);
        let mut weight = false;
        for part in parts {
            let Some((key, value)) = part.trim().split_once('=') else {
                q = None;
                continue;
            };
            if !key.trim().eq_ignore_ascii_case("q") || weight {
                q = None;
                continue;
            }
            weight = true;
            q = q.and_then(|_| quality(value.trim()));
        }
        let slot = if name.eq_ignore_ascii_case("gzip") {
            &mut gzip
        } else if name == "*" {
            &mut wildcard
        } else {
            continue;
        };
        let q = q.unwrap_or(0);
        *slot = Some(slot.map_or(q, |old: u16| old.min(q)));
    }
    gzip.or(wildcard).unwrap_or(0) > 0
}
pub(crate) fn text_path(path: &str) -> bool {
    let Ok(path) = super::reader::decode_path(path) else {
        return false;
    };
    path.rsplit('/')
        .next()
        .and_then(|p| p.rsplit_once('.'))
        .is_some_and(|(_, extension)| {
            ["js", "html", "css", "xml", "json", "shtml", "htm"]
                .iter()
                .any(|ext| extension.eq_ignore_ascii_case(ext))
        })
}
fn weak(etag: &str) -> String {
    if etag.starts_with("W/") {
        etag.into()
    } else {
        format!("W/{etag}")
    }
}
// If-None-Match uses weak comparison for GET/HEAD. Strip weakness for upstream
// servers that only implement quoted-ETag matching; never alter If-Match/If-Range.
fn none_match(value: &str) -> String {
    let mut quoted = false;
    let mut start = 0;
    let mut tokens = Vec::new();
    for (i, byte) in value.bytes().enumerate() {
        if byte == b'"' {
            quoted = !quoted;
        } else if byte == b',' && !quoted {
            tokens.push(&value[start..i]);
            start = i + 1;
        }
    }
    if quoted {
        return value.into();
    }
    tokens.push(&value[start..]);
    tokens
        .iter()
        .map(|v| v.trim().strip_prefix("W/").unwrap_or(v.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}
pub(crate) fn conditional(
    mut out: reqwest::blocking::RequestBuilder,
    request: &Request,
) -> reqwest::blocking::RequestBuilder {
    for name in [
        "Range",
        "If-Range",
        "If-Match",
        "If-None-Match",
        "If-Modified-Since",
        "If-Unmodified-Since",
    ] {
        if let Some(value) = field(request, name) {
            out = out.header(
                name,
                if name == "If-None-Match" {
                    none_match(value)
                } else {
                    value.into()
                },
            );
        }
    }
    // We select the encoding ourselves; origin must return identity bytes.
    out.header("Accept-Encoding", "identity")
}
pub(crate) fn allowed(request: &Request, policy: &Policy) -> bool {
    matches!(request.method(), HttpMethod::Get | HttpMethod::Head)
        && policy.referer_allowed(field(request, "Referer"))
}
pub(crate) fn respond(
    request: Request,
    response: Response,
    mime: Option<&str>,
    policy: &Policy,
) -> Result<()> {
    let status = response.status().as_u16();
    let path_text = policy.gzip_enabled && text_path(request.url());
    let encoding = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Accept-Encoding"))
        .map(|h| h.value.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let length = response
        .headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let selected = path_text
        && accepts_gzip(&encoding)
        && field(&request, "Range").is_none()
        && !response.headers().contains_key("Content-Encoding")
        && !response.headers().contains_key("Content-Range")
        && (status == 304
            || status == 200
                && length.is_some_and(|n| {
                    n >= policy.gzip_min_bytes.max(256) && n <= policy.gzip_max_bytes.min(2_097_152)
                }));
    let mut headers = Vec::new();
    for name in [
        "Content-Range",
        "Content-Encoding",
        "Last-Modified",
        "Accept-Ranges",
        "Cache-Control",
        "Expires",
    ] {
        if let Some(value) = response.headers().get(name) {
            headers.push(header(name, value.to_str()?)?);
        }
    }
    if let Some(etag) = response.headers().get("ETag") {
        let etag = etag.to_str()?;
        headers.push(header(
            "ETag",
            &if selected { weak(etag) } else { etag.into() },
        )?);
    }
    let mut vary = Vec::<String>::new();
    for value in response.headers().get_all("Vary") {
        for token in value
            .to_str()?
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            if !vary.iter().any(|v| v.eq_ignore_ascii_case(token)) {
                vary.push(token.into());
            }
        }
    }
    if path_text
        && !vary
            .iter()
            .any(|v| v == "*" || v.eq_ignore_ascii_case("Accept-Encoding"))
    {
        vary.push("Accept-Encoding".into());
    }
    if !vary.is_empty() {
        headers.push(header("Vary", &vary.join(", "))?);
    }
    // 304 has no reliable original length; omit encoding metadata so a cached
    // identity response below/above the size bounds is never relabelled gzip.
    if selected && status != 304 {
        headers.push(header("Content-Encoding", "gzip")?);
    }
    let content_type = if matches!(status, 200 | 206 | 304) {
        mime
    } else {
        None
    }
    .unwrap_or(
        response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream"),
    );
    headers.push(header("Content-Type", content_type)?);
    headers.push(header("X-Content-Type-Options", "nosniff")?);
    headers.push(header("Content-Security-Policy", CSP)?);
    let body_len = if selected || status == 304 {
        None
    } else {
        length.and_then(|n| usize::try_from(n).ok())
    };
    // HEAD advertises the selected GET representation but never reads its body.
    let body: Box<dyn Read + Send> =
        if selected && request.method() == &HttpMethod::Get && status == 200 {
            Box::new(GzEncoder::new(response, Compression::default()))
        } else {
            Box::new(response)
        };
    request.respond(Out::new(StatusCode(status), headers, body, body_len, None))?;
    Ok(())
}
fn image_url(origin: &Url, target: &str) -> Result<Url> {
    ensure!(
        target.starts_with('/')
            && !target.starts_with("//")
            && !target.contains('#')
            && !target.contains(['\r', '\n', '\0']),
        "invalid image target"
    );
    let (path, query) = target
        .split_once('?')
        .map_or((target, None), |(p, q)| (p, Some(q)));
    // set_path never interprets client input as a hostname/authority.
    let mut url = origin.clone();
    url.set_path(path);
    url.set_query(query);
    Ok(url)
}
fn handle(request: Request, http: &HttpClient, origin: &Url, policy: &Policy) {
    if !matches!(request.method(), HttpMethod::Get | HttpMethod::Head) {
        reject(request, 405);
        return;
    }
    if request.url() == "/healthz" {
        reject(request, 200);
        return;
    }
    if !allowed(&request, policy) {
        reject(request, 403);
        return;
    }
    let result = (|| -> Result<Response> {
        let url = image_url(origin, request.url())?;
        let method = if request.method() == &HttpMethod::Head {
            Method::HEAD
        } else {
            Method::GET
        };
        conditional(http.request(method, url), &request)
            .send()
            .context("image origin unavailable")
    })();
    match result {
        Ok(response) => {
            let _ = respond(request, response, None, policy);
        }
        Err(_) => reject(request, 502),
    }
}
pub(crate) fn start(policy: Arc<Policy>) -> Result<std::thread::JoinHandle<()>> {
    let server =
        Server::http("0.0.0.0:8085").map_err(|_| anyhow!("image text proxy bind failed"))?;
    let http = http_client(None, 20)?;
    let origin = Url::parse("http://nginx:8086")?;
    Ok(std::thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        for request in server.incoming_requests() {
            if active.fetch_add(1, Ordering::SeqCst) >= 32 {
                active.fetch_sub(1, Ordering::SeqCst);
                reject(request, 503);
                continue;
            }
            let active = Arc::clone(&active);
            let http = http.clone();
            let origin = origin.clone();
            let policy = Arc::clone(&policy);
            std::thread::spawn(move || {
                struct Active(Arc<AtomicUsize>);
                impl Drop for Active {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                let _active = Active(active);
                handle(request, &http, &origin, &policy);
            });
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::{Cursor, Read, Write};
    use std::net::TcpStream;
    #[test]
    fn exact_suffix_and_negotiation_refuse_fake_suffixes_and_zero_or_invalid_quality() {
        for path in [
            "/a.js",
            "/a.HTML?q=.png",
            "/a.css",
            "/a.xml",
            "/a.json",
            "/a.shtml",
            "/a.htm",
            "/a%2EJs",
        ] {
            assert!(text_path(path), "{path}");
        }
        for path in [
            "/a.png?file=.js",
            "/a.js.png",
            "/a.js/",
            "/a.svg",
            "/a.js/path",
            "/%ff.js",
        ] {
            assert!(!text_path(path), "{path}");
        }
        for value in [
            "gzip",
            "br, GZip;q=0.5",
            "*;q=1",
            "gzip;q=0.001",
            "gzip;q=1.000",
            "identity;q=0,gzip",
        ] {
            assert!(accepts_gzip(value), "{value}");
        }
        for value in [
            "",
            "br",
            "gzip;q=0",
            "gzip;q=0,*;q=1",
            "gzip;q=0.000",
            "gzip;q=1.001",
            "gzip;q=-1",
            "gzip;q=nan",
            "gzip;q=0.0001",
            "gzip;q=1;q=0",
            "gzip;q=bad,*;q=1",
            "gzip,gzip;q=0",
        ] {
            assert!(!accepts_gzip(value), "{value}");
        }
    }
    #[test]
    fn fixed_origin_cannot_be_replaced_by_client_authority_or_encoded_target() {
        assert_eq!(
            none_match("W/\"one,two\", \"normal\""),
            "\"one,two\", \"normal\""
        );
        let origin = Url::parse("http://nginx:8086").unwrap();
        for target in [
            "http://evil.test/a.js",
            "//evil.test/a.js",
            "/a.js#fragment",
            "/a\r\nHost:evil.js",
        ] {
            assert!(image_url(&origin, target).is_err());
        }
        for target in [
            "/a.js",
            "/%2f%2fevil.test/a.js?host=evil.test",
            "/@evil.test/a.js",
            "/a%20b.js",
        ] {
            let url = image_url(&origin, target).unwrap();
            assert_eq!(url.host_str(), Some("nginx"));
            assert_eq!(url.port(), Some(8086));
        }
        assert_eq!(image_url(&origin, "/a%20b.js").unwrap().path(), "/a%20b.js");
    }
    #[test]
    fn real_http_gzip_boundaries_identity_range_head_and_conditional_variants() {
        // Every response traverses a real local upstream and a real tiny-http frontend.
        let cases = [
            (255, None, 200, false),
            (256, None, 200, true),
            (2_097_152, None, 200, true),
            (2_097_153, None, 200, false),
            (4096, Some(("Range", "bytes=0-4095")), 206, false),
            (4096, Some(("If-None-Match", "W/\"fixture\"")), 304, true),
            (255, Some(("If-None-Match", "W/\"fixture\"")), 304, true),
            (
                4096,
                Some(("Accept-Encoding", "gzip;q=0,*;q=1")),
                200,
                false,
            ),
            (4096, Some(("Origin-Encoding", "gzip")), 200, false),
            (65536, Some(("Origin-Chunked", "true")), 200, false),
            (4096, Some(("Accept-Encoding", "identity")), 200, false),
            (
                4096,
                Some(("Origin-Range", "bytes 0-4095/8192")),
                200,
                false,
            ),
        ];
        let origin = Server::http("127.0.0.1:0").unwrap();
        let upstream = origin.server_addr().to_ip().unwrap();
        let producer = std::thread::spawn(move || {
            for (size, extra, status, _) in cases {
                let request = origin
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                assert!(field(&request, "Cookie").is_none());
                assert!(field(&request, "Authorization").is_none());
                if status == 304 {
                    assert_eq!(field(&request, "If-None-Match"), Some("\"fixture\""));
                }
                let mut headers = vec![
                    header("Content-Type", "text/javascript").unwrap(),
                    header("ETag", "\"fixture\"").unwrap(),
                    header("Vary", "Origin").unwrap(),
                ];
                if extra.is_some_and(|x| x.0 == "Origin-Encoding") {
                    headers.push(header("Content-Encoding", "gzip").unwrap());
                }
                if status == 206 || extra.is_some_and(|x| x.0 == "Origin-Range") {
                    headers.push(header("Content-Range", "bytes 0-4095/8192").unwrap());
                }
                let body = if status == 304 {
                    vec![]
                } else {
                    vec![b'x'; size]
                };
                request
                    .respond(
                        Out::new(
                            StatusCode(status),
                            headers,
                            Cursor::new(body),
                            Some(if status == 304 { 0 } else { size }),
                            None,
                        )
                        .with_chunked_threshold(
                            if extra.is_some_and(|x| x.0 == "Origin-Chunked") {
                                0
                            } else {
                                usize::MAX
                            },
                        ),
                    )
                    .unwrap();
            }
            let request = origin
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            assert_eq!(request.method(), &HttpMethod::Head);
            request
                .respond(Out::new(
                    StatusCode(200),
                    vec![header("ETag", "\"fixture\"").unwrap()],
                    Cursor::new(Vec::new()),
                    Some(4096),
                    None,
                ))
                .unwrap();
        });
        let frontend = Server::http("127.0.0.1:0").unwrap();
        let address = frontend.server_addr().to_ip().unwrap();
        let consumer = std::thread::spawn(move || {
            let http = http_client(None, 10).unwrap();
            let origin = Url::parse(&format!("http://{upstream}")).unwrap();
            let policy = Policy::default();
            for _ in 0..cases.len() + 1 {
                handle(
                    frontend
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap(),
                    &http,
                    &origin,
                    &policy,
                );
            }
        });
        let http = http_client(None, 10).unwrap();
        let url = format!("http://{address}/a.js");
        for (size, extra, status, compressed) in cases {
            let mut request = http
                .get(&url)
                .header("Accept-Encoding", "gzip")
                .header("Cookie", "secret=never-forward")
                .header("Authorization", "Bearer not-forwarded");
            if let Some((name, value)) = extra {
                if name == "Accept-Encoding" {
                    request = http.get(&url).header(name, value);
                } else if !name.starts_with("Origin-") {
                    request = request.header(name, value);
                }
            }
            let response = request.send().unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
            assert_eq!(response.headers()["Content-Security-Policy"], CSP);
            assert!(response.headers()["Vary"]
                .to_str()
                .unwrap()
                .contains("Accept-Encoding"));
            if compressed && status != 304 {
                assert_eq!(
                    response
                        .headers()
                        .get("Content-Encoding")
                        .map(|v| v.to_str().unwrap()),
                    Some("gzip"),
                    "size={size} status={status} headers={:?}",
                    response.headers()
                );
                assert_eq!(response.headers()["ETag"], "W/\"fixture\"");
            }
            if !compressed {
                assert_eq!(response.headers()["ETag"], "\"fixture\"");
                if !extra.is_some_and(|x| x.0 == "Origin-Encoding") {
                    assert!(response.headers().get("Content-Encoding").is_none());
                }
            }
            if status == 206 {
                assert_eq!(response.headers()["Content-Range"], "bytes 0-4095/8192");
            }
            if status == 304 {
                assert!(response.headers().get("Content-Encoding").is_none());
                assert_eq!(response.headers()["ETag"], "W/\"fixture\"");
                assert!(response.bytes().unwrap().is_empty());
                continue;
            }
            let bytes = response.bytes().unwrap();
            let mut decoded = Vec::new();
            if compressed {
                GzDecoder::new(&bytes[..])
                    .read_to_end(&mut decoded)
                    .unwrap();
            } else {
                decoded = bytes.to_vec();
            }
            assert_eq!(decoded, vec![b'x'; size]);
        }
        let response = http
            .head(&url)
            .header("Accept-Encoding", "gzip")
            .send()
            .unwrap();
        assert_eq!(response.headers()["Content-Encoding"], "gzip");
        assert!(response.headers().get("Content-Length").is_none());
        assert!(response.bytes().unwrap().is_empty());
        consumer.join().unwrap();
        producer.join().unwrap();
    }
    #[test]
    fn referer_denial_precedes_upstream_and_client_host_auth_and_forwarded_headers_are_not_forwarded(
    ) {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let origin = upstream.server_addr().to_ip().unwrap();
        let frontend = Server::http("127.0.0.1:0").unwrap();
        let address = frontend.server_addr().to_ip().unwrap();
        let worker = std::thread::spawn(move || {
            let policy = Policy {
                hotlink_enabled: true,
                allow_empty_referer: false,
                allowed_referers: vec!["allowed.test".into()],
                ..Policy::default()
            };
            let http = http_client(None, 5).unwrap();
            let origin = Url::parse(&format!("http://{origin}")).unwrap();
            for _ in 0..2 {
                handle(
                    frontend
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap()
                        .unwrap(),
                    &http,
                    &origin,
                    &policy,
                );
            }
        });
        let http = http_client(None, 5).unwrap();
        let url = format!("http://{address}/a.json");
        let denied = http
            .get(&url)
            .header("Referer", "https://evil.test/")
            .send()
            .unwrap();
        assert_eq!(denied.status().as_u16(), 403);
        assert_eq!(denied.headers()["Cache-Control"], "no-store");
        assert!(upstream
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
        let owner = std::thread::spawn(move || {
            let request = upstream
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(field(&request, "Host"), Some(origin.to_string().as_str()));
            for name in [
                "Cookie",
                "Authorization",
                "X-Forwarded-Host",
                "X-Forwarded-For",
                "Forwarded",
            ] {
                assert!(field(&request, name).is_none());
            }
            request.respond(Out::from_string("{\"ok\":true}")).unwrap();
        });
        let allowed = http
            .get(url)
            .header("Host", "evil.test")
            .header("Referer", "https://allowed.test/blog")
            .header("Cookie", "secret=x")
            .header("Authorization", "Bearer secret")
            .header("X-Forwarded-Host", "evil.test")
            .header("X-Forwarded-For", "192.0.2.1")
            .header("Forwarded", "host=evil.test")
            .send()
            .unwrap();
        assert_eq!(allowed.status().as_u16(), 200);
        assert_eq!(allowed.text().unwrap(), "{\"ok\":true}");
        worker.join().unwrap();
        owner.join().unwrap();
    }
    #[test]
    fn internal_post_rejects_without_origin_request_or_continue_before_body() {
        let origin = Server::http("127.0.0.1:0").unwrap();
        let upstream = origin.server_addr().to_ip().unwrap();
        let frontend = Server::http("127.0.0.1:0").unwrap();
        let address = frontend.server_addr().to_ip().unwrap();
        let worker = std::thread::spawn(move || {
            let request = frontend
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            handle(
                request,
                &http_client(None, 2).unwrap(),
                &Url::parse(&format!("http://{upstream}")).unwrap(),
                &Policy::default(),
            );
        });
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket.write_all(b"POST /a.js HTTP/1.1\r\nHost: local\r\nContent-Length: 100000000\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").unwrap();
        let mut buffer = [0; 1024];
        let n = socket.read(&mut buffer).unwrap();
        let received = String::from_utf8_lossy(&buffer[..n]);
        assert!(received.starts_with("HTTP/1.1 405"));
        assert!(!received.contains("100 Continue"));
        assert!(origin
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
        // tiny-http drains on Drop; the PUBLIC Nginx gate must reject body-bearing
        // methods first. Close this internal socket so its bounded worker exits.
        drop(socket);
        worker.join().unwrap();
    }
}
