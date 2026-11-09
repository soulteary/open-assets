//! Read-only version resolver. No S3 credentials are loaded by this process.
use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tiny_http::{Header, Method as HttpMethod, Response as ServerResponse, Server, StatusCode};
#[derive(Clone)]
pub struct ReaderConfig {
    pub manifests: PathBuf,
    pub receipts: PathBuf,
    pub bucket: String,
    pub endpoint: Url,
    pub ca: Option<String>,
}
impl ReaderConfig {
    pub fn from_env() -> Result<Self> {
        let endpoint = Url::parse(
            &env::var("OBJECT_ENDPOINT").unwrap_or_else(|_| "https://storage.internal:9000".into()),
        )?;
        ensure!(
            endpoint.scheme() == "https"
                && endpoint.host_str().is_some()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && matches!(endpoint.path(), "" | "/")
                && endpoint.query().is_none()
                && endpoint.fragment().is_none(),
            "invalid fixed object endpoint"
        );
        Ok(Self {
            manifests: env::var_os("OBJECT_MANIFESTS")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/manifests".into()),
            receipts: env::var_os("OBJECT_RECEIPTS")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/receipts".into()),
            bucket: env::var("OBJECT_BUCKET").unwrap_or_else(|_| "attachments".into()),
            endpoint,
            ca: Some(env::var("OBJECT_CA_FILE").unwrap_or_else(|_| "/tls/ca.crt".into())),
        })
    }
}
fn decode_path(path: &str) -> Result<String> {
    let path = path.split('?').next().unwrap_or(path);
    let mut bytes = Vec::new();
    let mut chars = path.as_bytes().iter();
    while let Some(&b) = chars.next() {
        if b == b'%' {
            let a = *chars.next().ok_or_else(|| anyhow!("invalid URL escape"))?;
            let z = *chars.next().ok_or_else(|| anyhow!("invalid URL escape"))?;
            let h = |x: u8| -> Option<u8> {
                match x {
                    b'0'..=b'9' => Some(x - b'0'),
                    b'a'..=b'f' => Some(x - b'a' + 10),
                    b'A'..=b'F' => Some(x - b'A' + 10),
                    _ => None,
                }
            };
            bytes.push(
                h(a).ok_or_else(|| anyhow!("invalid URL escape"))? * 16
                    + h(z).ok_or_else(|| anyhow!("invalid URL escape"))?,
            );
        } else {
            bytes.push(b);
        }
    }
    Ok(String::from_utf8(bytes)?)
}
#[derive(Clone)]
struct Entry {
    identity: (u64, u64, i64, i64, i64, i64),
    manifest: Value,
    bytes: u64,
    age: u64,
}
#[derive(Default)]
struct Cache {
    entries: BTreeMap<(String, String), Entry>,
    tick: u64,
}
impl Cache {
    fn manifest(&mut self, path: &Path, key: &str, sha: &str) -> Result<Value> {
        let meta = fs::symlink_metadata(path)?;
        ensure!(meta.is_file(), "unsafe manifest");
        #[cfg(unix)]
        let identity = (
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        );
        #[cfg(not(unix))]
        let identity = (0, meta.len(), 0, 0, 0, 0);
        self.tick += 1;
        let id = (key.to_owned(), sha.to_owned());
        if let Some(old) = self.entries.get_mut(&id) {
            if old.identity == identity {
                old.age = self.tick;
                return Ok(old.manifest.clone());
            }
        }
        let manifest = read_json(path)?;
        ensure!(
            manifest["key"] == key && manifest["sha"] == sha,
            "manifest identity mismatch"
        );
        validate_manifest(&manifest)?;
        self.entries.insert(
            id.clone(),
            Entry {
                identity,
                manifest: manifest.clone(),
                bytes: meta.len(),
                age: self.tick,
            },
        );
        while self.entries.len() > 8
            || self.entries.values().map(|v| v.bytes).sum::<u64>() > MAX_METADATA
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.age)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                self.entries.remove(&k);
            } else {
                break;
            }
        }
        Ok(manifest)
    }
}
fn resolve_cached(
    config: &ReaderConfig,
    path: &str,
    cache: &Mutex<Cache>,
) -> Result<(String, Option<String>)> {
    let decoded = decode_path(path)?;
    let prefix = format!("/{}/repositories/", config.bucket);
    let name = decoded
        .strip_prefix(&prefix)
        .ok_or_else(|| anyhow!("invalid version object URL"))?;
    let mut parts_it = name.splitn(4, '/');
    let key = parts_it.next().unwrap_or("");
    let release = parts_it.next().unwrap_or("");
    let sha = parts_it.next().unwrap_or("");
    let relative_path = parts_it.next().unwrap_or("");
    ensure!(
        valid_key(key) && release == "releases" && valid_sha(sha),
        "invalid version object identity"
    );
    relative(relative_path)?;
    let original = format!("repositories/{key}/releases/{sha}/{relative_path}");
    let path = config.manifests.join(key).join(format!("{sha}.json"));
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((original, None)),
        Err(e) => return Err(e.into()),
    };
    ensure!(meta.is_file(), "unsafe manifest");
    real_dir(path.parent().unwrap())?;
    let m = cache
        .lock()
        .map_err(|_| anyhow!("manifest cache poisoned"))?
        .manifest(&path, key, sha)?;
    let referenced = m["object_layout"] == "references-v1";
    let marker = config.receipts.join(key).join(format!("{sha}.json"));
    if referenced && fs::symlink_metadata(&marker).is_ok() {
        real_dir(marker.parent().unwrap())?;
        let receipt = read_json(&marker)?;
        ensure!(
            receipt["key"] == key && receipt["sha"] == sha,
            "invalid object receipt"
        );
        if matches!(receipt["status"].as_str(), Some("complete" | "deleting")) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "retired object release",
            )
            .into());
        }
    }
    if let Some(item) = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["path"] == relative_path)
    {
        let source = source(&m, item)?;
        return Ok((
            text(&source, "key")?.to_owned(),
            Some(text(item, "content_type")?.to_owned()),
        ));
    }
    if !referenced || m["legacy_extra_objects"] == true {
        Ok((original, None))
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "object absent").into())
    }
}
pub fn resolve(config: &ReaderConfig, path: &str) -> Result<(String, Option<String>)> {
    resolve_cached(config, path, &Mutex::new(Cache::default()))
}
fn header(name: &str, value: &str) -> Result<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .map_err(|_| anyhow!("invalid response header"))
}
fn error(request: tiny_http::Request, status: u16) {
    let mut response = ServerResponse::empty(StatusCode(status));
    for (k, v) in [
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Content-Security-Policy", CSP),
    ] {
        if let Ok(h) = header(k, v) {
            response.add_header(h);
        }
    }
    if status == 200 {
        response.add_header(header("X-Open-Assets-Reader", "guard-v1").unwrap());
    }
    let _ = request.respond(response);
}
type ForwardResponse = (Response, Vec<Header>, Option<usize>, Option<File>);
fn handle(
    request: tiny_http::Request,
    config: &ReaderConfig,
    http: &HttpClient,
    cache: &Mutex<Cache>,
) {
    if request.url() == "/healthz" {
        error(request, 200);
        return;
    }
    if !matches!(request.method(), HttpMethod::Get | HttpMethod::Head) {
        error(request, 405);
        return;
    }
    let result = (|| -> Result<ForwardResponse> {
        let lock = read_guard(&config.receipts.join("compaction.lock"), false, false)?;
        let (key, mime) = resolve_cached(config, request.url(), cache)?;
        let mut endpoint = config.endpoint.clone();
        endpoint.set_path(&format!(
            "/{}/{}",
            encode(&config.bucket, false),
            encode(&key, true)
        ));
        let method = if request.method() == &HttpMethod::Head {
            Method::HEAD
        } else {
            Method::GET
        };
        let mut upstream = http.request(method, endpoint);
        for name in [
            "Range",
            "If-Range",
            "If-Match",
            "If-None-Match",
            "If-Modified-Since",
            "If-Unmodified-Since",
        ] {
            if let Some(h) = request.headers().iter().find(|h| h.field.equiv(name)) {
                upstream = upstream.header(name, h.value.as_str());
            }
        }
        let response = upstream.send().context("upstream request unavailable")?;
        let mut headers = Vec::new();
        for name in [
            "Content-Length",
            "Content-Range",
            "Content-Encoding",
            "ETag",
            "Last-Modified",
            "Accept-Ranges",
            "Cache-Control",
        ] {
            if let Some(value) = response.headers().get(name) {
                headers.push(header(name, value.to_str()?)?);
            }
        }
        let content_type = if matches!(response.status().as_u16(), 200 | 206 | 304) {
            mime.as_deref().unwrap_or(
                response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream"),
            )
        } else {
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
        };
        headers.push(header("Content-Type", content_type)?);
        headers.push(header("X-Content-Type-Options", "nosniff")?);
        headers.push(header("Content-Security-Policy", CSP)?);
        let length = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        Ok((response, headers, length, lock))
    })();
    match result {
        Ok((response, headers, length, _guard)) => {
            let status = StatusCode(response.status().as_u16());
            let out = ServerResponse::new(status, headers, response, length, None);
            let _ = request.respond(out);
        }
        Err(e) => {
            let code = if e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                404
            } else if e.downcast_ref::<reqwest::Error>().is_some()
                || e.downcast_ref::<std::io::Error>().is_some()
            {
                502
            } else {
                400
            };
            error(request, code);
        }
    }
}
pub fn serve() -> Result<()> {
    let config = Arc::new(ReaderConfig::from_env()?);
    let http = Arc::new(http_client(config.ca.as_deref(), 20)?);
    let cache = Arc::new(Mutex::new(Cache::default()));
    let active = Arc::new(AtomicUsize::new(0));
    let server = Server::http("0.0.0.0:8083").map_err(|_| anyhow!("object reader bind failed"))?;
    for request in server.incoming_requests() {
        if active.fetch_add(1, Ordering::SeqCst) >= 32 {
            active.fetch_sub(1, Ordering::SeqCst);
            error(request, 503);
            continue;
        }
        let c = Arc::clone(&config);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            struct Active(Arc<AtomicUsize>);
            impl Drop for Active {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _guard = Active(active);
            handle(request, &c, &http, &cache);
        });
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoding_preserves_plus_and_decodes_once() {
        assert_eq!(decode_path("/a+b/%252e.svg?q=x").unwrap(), "/a+b/%2e.svg");
        assert!(decode_path("/%ff").is_err());
        assert!(decode_path("/%2").is_err());
    }
    #[test]
    fn resolver_rejects_hidden_paths_and_foreign_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ReaderConfig {
            manifests: tmp.path().join("manifests"),
            receipts: tmp.path().join("receipts"),
            bucket: "attachments".into(),
            endpoint: Url::parse("https://s3.test").unwrap(),
            ca: None,
        };
        let sha = "a".repeat(40);
        assert!(resolve(
            &config,
            &format!("/attachments/repositories/images/releases/{sha}/a/%2e%2e/x")
        )
        .is_err());
        assert!(resolve(
            &config,
            &format!("/other/repositories/images/releases/{sha}/a.png")
        )
        .is_err());
        let (key, mime) = resolve(
            &config,
            &format!("/attachments/repositories/images/releases/{sha}/a+b.png"),
        )
        .unwrap();
        assert!(key.ends_with("/a+b.png"));
        assert!(mime.is_none());
    }
    #[test]
    fn reader_streams_head_range_and_conditional_responses_with_manifest_mime_and_csp() {
        let tmp = tempfile::tempdir().unwrap();
        let manifests = tmp.path().join("manifests");
        let receipts = tmp.path().join("receipts");
        fs::create_dir_all(&receipts).unwrap();
        fs::write(receipts.join("compaction.lock"), b"").unwrap();
        let sha = "a".repeat(40);
        let origin = "b".repeat(40);
        let body = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        let source_key = format!("repositories/images/releases/{origin}/images/a.svg");
        let manifest = json!({"key":"images","sha":sha,"object_layout":"references-v1","files":[{"path":"images/a.svg","size":body.len(),"sha256":hash(body),"content_type":"image/svg+xml"}],"object_sources":{"images/a.svg":{"key":source_key,"content_type":"application/octet-stream"}}});
        crate::common::atomic(
            &manifests.join("images").join(format!("{sha}.json")),
            &manifest,
        )
        .unwrap();
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}",
            upstream.server_addr().to_ip().unwrap()
        ))
        .unwrap();
        let owner = source_key.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let request = upstream
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.url(), format!("/attachments/{owner}"));
                let range = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Range"))
                    .map(|h| h.value.to_string());
                let conditional = request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("If-None-Match"));
                let (status, data) = if conditional {
                    (304, Vec::new())
                } else if range.is_some() {
                    assert_eq!(range.as_deref(), Some("bytes=0-7"));
                    (206, body[..8].to_vec())
                } else {
                    (200, body.to_vec())
                };
                let length = if request.method() == &HttpMethod::Head {
                    body.len()
                } else {
                    data.len()
                };
                let mut headers = vec![
                    header("Content-Type", "application/octet-stream").unwrap(),
                    header("ETag", "\"fixture-etag\"").unwrap(),
                    header("Cache-Control", CACHE_CONTROL).unwrap(),
                    header("Accept-Ranges", "bytes").unwrap(),
                ];
                if status == 206 {
                    headers.push(
                        header("Content-Range", &format!("bytes 0-7/{}", body.len())).unwrap(),
                    );
                }
                request
                    .respond(ServerResponse::new(
                        StatusCode(status),
                        headers,
                        std::io::Cursor::new(data),
                        Some(length),
                        None,
                    ))
                    .unwrap();
            }
        });
        let frontend = Server::http("127.0.0.1:0").unwrap();
        let address = frontend.server_addr().to_ip().unwrap();
        let config = ReaderConfig {
            manifests,
            receipts,
            bucket: "attachments".into(),
            endpoint,
            ca: None,
        };
        let worker = std::thread::spawn(move || {
            let http = http_client(None, 3).unwrap();
            let cache = Mutex::new(Cache::default());
            for _ in 0..4 {
                let request = frontend
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                handle(request, &config, &http, &cache);
            }
        });
        let http = http_client(None, 5).unwrap();
        let url =
            format!("http://{address}/attachments/repositories/images/releases/{sha}/images/a.svg");
        for (method, extra, status) in [
            (Method::GET, None, 200),
            (Method::HEAD, None, 200),
            (Method::GET, Some(("Range", "bytes=0-7")), 206),
            (
                Method::GET,
                Some(("If-None-Match", "\"fixture-etag\"")),
                304,
            ),
        ] {
            let mut request = http.request(method.clone(), &url);
            if let Some((name, value)) = extra {
                request = request.header(name, value);
            }
            let response = request.send().unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.headers()["Content-Type"], "image/svg+xml");
            assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
            assert_eq!(response.headers()["Content-Security-Policy"], CSP);
            let bytes = response.bytes().unwrap();
            if method == Method::HEAD || status == 304 {
                assert!(bytes.is_empty());
            } else if status == 206 {
                assert_eq!(&bytes[..], &body[..8]);
            } else {
                assert_eq!(&bytes[..], body);
            }
        }
        worker.join().unwrap();
        server.join().unwrap();
    }
}
