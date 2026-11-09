//! Credential-separated, bounded S3 transport and immutable release maintenance.
use anyhow::{anyhow, bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::blocking::{Body, Client as HttpClient, Response};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Url,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};
pub mod maintenance;
pub mod reader;
pub const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
pub const CSP: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
pub const MAX_BODY: u64 = 512 * 1024 * 1024;
pub const MAX_METADATA: u64 = 8 * 1024 * 1024;

pub fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .ok_or_else(|| anyhow!("missing string field {k}"))
}
pub fn valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
pub fn valid_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|x| x.is_ascii_digit() || (b'a'..=b'f').contains(&x))
        && s.bytes().any(|x| x != b'0')
}
pub fn relative(s: &str) -> Result<&str> {
    ensure!(
        !s.is_empty()
            && !s.starts_with('/')
            && !s.ends_with('/')
            && !s.contains('\0')
            && s.split('/').all(|p| !p.is_empty() && !p.starts_with('.')),
        "invalid object path"
    );
    Ok(s)
}
pub fn parts<'a>(key: &str, s: &'a str) -> Result<(&'a str, &'a str)> {
    ensure!(valid_key(key), "invalid repository key");
    let tail = s
        .strip_prefix(&format!("repositories/{key}/releases/"))
        .ok_or_else(|| anyhow!("cross-repository object reference"))?;
    let (sha, p) = tail
        .split_once('/')
        .ok_or_else(|| anyhow!("invalid object reference"))?;
    ensure!(valid_sha(sha), "invalid source version");
    relative(p)?;
    Ok((sha, p))
}
pub fn read_json(path: &Path) -> Result<Value> {
    let mut f = safe_file(path)?;
    ensure!(f.metadata()?.len() <= MAX_METADATA, "oversized manifest");
    let mut raw = Vec::new();
    std::io::Read::by_ref(&mut f)
        .take(MAX_METADATA + 1)
        .read_to_end(&mut raw)?;
    ensure!(raw.len() as u64 <= MAX_METADATA, "oversized manifest");
    Ok(serde_json::from_slice(&raw)?)
}
pub fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub fn safe_file(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = o.open(path)?;
    ensure!(f.metadata()?.is_file(), "unsafe file");
    Ok(f)
}
pub fn real_dir(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "unsafe directory"
    );
    Ok(())
}
pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub fn file_digest(path: &Path, max: u64) -> Result<(u64, String, String)> {
    let mut f = safe_file(path)?;
    let mut hash = Sha256::new();
    let mut md = md5::Context::new();
    let mut size = 0u64;
    let mut buf = [0; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        ensure!(size <= max, "file exceeds limit");
        hash.update(&buf[..n]);
        md.consume(&buf[..n]);
    }
    Ok((
        size,
        hex::encode(hash.finalize()),
        format!("\"{:x}\"", md.finalize()),
    ))
}
pub fn verify_file(path: &Path, item: &Value) -> Result<()> {
    let size = item["size"]
        .as_u64()
        .ok_or_else(|| anyhow!("invalid size"))?;
    let (n, sha, _) = file_digest(path, size)?;
    ensure!(
        n == size && sha == text(item, "sha256")?,
        "immutable bytes mismatch"
    );
    Ok(())
}
pub fn source(manifest: &Value, item: &Value) -> Result<Value> {
    let key = text(manifest, "key")?;
    let sha = text(manifest, "sha")?;
    ensure!(
        valid_key(key) && valid_sha(sha),
        "invalid manifest identity"
    );
    let name = relative(text(item, "path")?)?;
    let refs = manifest
        .get("object_sources")
        .cloned()
        .unwrap_or_else(|| json!({}));
    ensure!(refs.is_object(), "invalid source map");
    if !refs.as_object().unwrap().is_empty() {
        ensure!(
            manifest["object_layout"] == "references-v1",
            "unsupported references"
        );
    }
    let value=refs.get(name).cloned().unwrap_or_else(||json!({"key":format!("repositories/{key}/releases/{sha}/{name}"),"content_type":item["content_type"]}));
    ensure!(
        value.as_object().is_some_and(|m| m.len() == 2
            && m.contains_key("key")
            && m.contains_key("content_type")),
        "invalid source record"
    );
    parts(key, text(&value, "key")?)?;
    valid_mime(text(&value, "content_type")?)?;
    Ok(value)
}
pub fn valid_mime(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| (32..=126).contains(&b)),
        "invalid content type"
    );
    Ok(())
}
pub fn validate_manifest(m: &Value) -> Result<BTreeMap<String, Value>> {
    let files = m["files"]
        .as_array()
        .ok_or_else(|| anyhow!("invalid manifest files"))?;
    ensure!(
        !files.is_empty() && files.len() <= 10000,
        "manifest file limit"
    );
    let mut out = BTreeMap::new();
    for item in files {
        let path = relative(text(item, "path")?)?;
        let sha = text(item, "sha256")?;
        ensure!(
            sha.len() == 64
                && sha
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && item["size"].as_u64().is_some(),
            "invalid manifest metadata"
        );
        valid_mime(text(item, "content_type")?)?;
        ensure!(
            out.insert(path.to_owned(), source(m, item)?).is_none(),
            "duplicate manifest path"
        );
    }
    if let Some(refs) = m.get("object_sources") {
        let refs = refs
            .as_object()
            .ok_or_else(|| anyhow!("invalid source map"))?;
        ensure!(
            refs.keys().all(|k| out.contains_key(k)),
            "unknown reference entry"
        );
    }
    Ok(out)
}
pub fn expected_objects(m: &Value) -> Result<BTreeMap<String, Value>> {
    let sources = validate_manifest(m)?;
    let mut out = BTreeMap::new();
    for item in m["files"].as_array().unwrap() {
        let name = text(item, "path")?;
        let src = &sources[name];
        let mut value = item.clone();
        value["content_type"] = src["content_type"].clone();
        if let Some(old) = out.insert(text(src, "key")?.to_owned(), value.clone()) {
            ensure!(
                old["size"] == value["size"]
                    && old["sha256"] == value["sha256"]
                    && old["content_type"] == value["content_type"],
                "conflicting object ownership"
            );
        }
    }
    Ok(out)
}

fn encode(s: &str, slash: bool) -> String {
    let mut r = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || (slash && b == b'/') {
            r.push(b as char)
        } else {
            r.push_str(&format!("%{b:02X}"));
        }
    }
    r
}
fn hmac(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}
fn hash(v: &[u8]) -> String {
    hex::encode(Sha256::digest(v))
}
pub fn signing_headers(
    method: &str,
    url: &Url,
    headers: &HeaderMap,
    payload: &str,
    access: &str,
    secret: &str,
    stamp: &str,
) -> Result<HeaderMap> {
    ensure!(stamp.len() == 16, "invalid signing timestamp");
    let date = &stamp[..8];
    let scope = format!("{date}/us-east-1/s3/aws4_request");
    let mut h = headers.clone();
    let host = match url.port() {
        Some(p) => format!(
            "{}:{p}",
            url.host_str().ok_or_else(|| anyhow!("invalid host"))?
        ),
        None => url
            .host_str()
            .ok_or_else(|| anyhow!("invalid host"))?
            .to_owned(),
    };
    h.insert("host", HeaderValue::from_str(&host)?);
    h.insert("x-amz-date", HeaderValue::from_str(stamp)?);
    h.insert("x-amz-content-sha256", HeaderValue::from_str(payload)?);
    let mut canonical = BTreeMap::new();
    for (k, v) in &h {
        canonical.insert(
            k.as_str(),
            v.to_str()?.split_whitespace().collect::<Vec<_>>().join(" "),
        );
    }
    let signed = canonical.keys().copied().collect::<Vec<_>>().join(";");
    let header_text = canonical
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let mut query = url
        .query_pairs()
        .map(|(k, v)| (encode(&k, false), encode(&v, false)))
        .collect::<Vec<_>>();
    query.sort();
    let query = query
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_request = format!(
        "{method}\n{}\n{query}\n{header_text}\n{signed}\n{payload}",
        url.path()
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hash(canonical_request.as_bytes())
    );
    let kd = hmac(format!("AWS4{secret}").as_bytes(), date);
    let kr = hmac(&kd, "us-east-1");
    let ks = hmac(&kr, "s3");
    let signing = hmac(&ks, "aws4_request");
    let signature = hex::encode(hmac(&signing, &string_to_sign));
    h.insert("authorization",HeaderValue::from_str(&format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={signature}"))?);
    Ok(h)
}

#[derive(Clone)]
pub struct Client {
    http: HttpClient,
    endpoint: Url,
    bucket: String,
    access_path: PathBuf,
    secret_path: PathBuf,
}
impl Client {
    pub fn from_config(c: &Value) -> Result<Self> {
        let endpoint = Url::parse(text(c, "endpoint")?)?;
        ensure!(
            matches!(endpoint.scheme(), "http" | "https")
                && endpoint.host_str().is_some()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none()
                && matches!(endpoint.path(), "" | "/"),
            "invalid S3 endpoint"
        );
        let bucket = text(c, "bucket")?.to_owned();
        ensure!(
            !bucket.is_empty()
                && bucket.len() <= 63
                && bucket
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b)),
            "invalid bucket"
        );
        let http = http_client(env::var("S3_CA_FILE").ok().as_deref(), 30)?;
        Ok(Self {
            http,
            endpoint,
            bucket,
            access_path: PathBuf::from(
                env::var("S3_ACCESS_KEY_FILE").context("S3 access file missing")?,
            ),
            secret_path: PathBuf::from(
                env::var("S3_SECRET_KEY_FILE").context("S3 secret file missing")?,
            ),
        })
    }
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
    fn url(&self, key: &str, query: &[(String, String)]) -> Result<Url> {
        ensure!(
            !key.starts_with('/') && !key.contains('\0'),
            "invalid S3 key"
        );
        let mut u = self.endpoint.clone();
        u.set_path(&format!(
            "/{}/{}",
            encode(&self.bucket, false),
            encode(key, true)
        ));
        if !query.is_empty() {
            let q = query
                .iter()
                .map(|(k, v)| format!("{}={}", encode(k, false), encode(v, false)))
                .collect::<Vec<_>>()
                .join("&");
            u.set_query(Some(&q));
        }
        Ok(u)
    }
    fn request(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        headers: HeaderMap,
        body: Option<Body>,
        payload: &str,
    ) -> Result<Response> {
        let url = self.url(key, query)?;
        let access = fs::read_to_string(&self.access_path)?;
        let secret = fs::read_to_string(&self.secret_path)?;
        ensure!(
            !access.trim().is_empty() && !secret.trim().is_empty(),
            "empty S3 identity"
        );
        let headers = signing_headers(
            method.as_str(),
            &url,
            &headers,
            payload,
            access.trim(),
            secret.trim(),
            &Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        )?;
        let mut r = self.http.request(method, url).headers(headers);
        if let Some(body) = body {
            r = r.body(body);
        }
        r.send().map_err(|_| anyhow!("S3 transport failed"))
    }
    fn checked(response: Response) -> Result<Response> {
        ensure!(
            response.status().is_success(),
            "S3 request failed with status {}",
            response.status().as_u16()
        );
        Ok(response)
    }
    pub fn head(&self, key: &str) -> Result<Option<Value>> {
        let r = self.request(Method::HEAD, key, &[], HeaderMap::new(), None, &hash(b""))?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        let r = Self::checked(r)?;
        Ok(Some(response_metadata(&r)?))
    }
    pub fn get(&self, key: &str) -> Result<Vec<u8>> {
        let mut r = Self::checked(self.request(
            Method::GET,
            key,
            &[],
            HeaderMap::new(),
            None,
            &hash(b""),
        )?)?;
        let mut raw = Vec::new();
        r.by_ref().take(MAX_METADATA + 1).read_to_end(&mut raw)?;
        ensure!(
            raw.len() as u64 <= MAX_METADATA,
            "S3 metadata response limit"
        );
        Ok(raw)
    }
    pub fn verify_object(&self, key: &str, item: &Value) -> Result<Value> {
        let expected = item["size"]
            .as_u64()
            .ok_or_else(|| anyhow!("invalid object size"))?;
        ensure!(expected <= MAX_BODY, "object size budget");
        let mut response = Self::checked(self.request(
            Method::GET,
            key,
            &[],
            HeaderMap::new(),
            None,
            &hash(b""),
        )?)?;
        let mut metadata = response_metadata(&response)?;
        ensure!(
            metadata["size"] == expected,
            "object response size mismatch"
        );
        let mut sha = Sha256::new();
        let mut md = md5::Context::new();
        let mut size = 0u64;
        let mut buf = [0u8; 65536];
        loop {
            let cap = (expected - size + 1).min(buf.len() as u64) as usize;
            let n = response.read(&mut buf[..cap])?;
            if n == 0 {
                break;
            }
            size += n as u64;
            ensure!(size <= expected, "object exceeds expected size");
            sha.update(&buf[..n]);
            md.consume(&buf[..n]);
        }
        let digest = hex::encode(sha.finalize());
        ensure!(
            size == expected && item["sha256"] == digest,
            "immutable object body mismatch"
        );
        metadata["sha256"] = json!(digest);
        metadata["body_etag"] = json!(format!("\"{:x}\"", md.finalize()));
        Ok(metadata)
    }
    pub fn get_to_file(&self, key: &str, path: &Path, max: u64) -> Result<Value> {
        ensure!(max <= MAX_BODY, "object size budget");
        let mut r = Self::checked(self.request(
            Method::GET,
            key,
            &[],
            HeaderMap::new(),
            None,
            &hash(b""),
        )?)?;
        let mut meta = response_metadata(&r)?;
        ensure!(
            meta["size"].as_u64().is_some_and(|n| n <= max),
            "remote object size limit"
        );
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let mut f = o.open(path)?;
        let mut size = 0;
        let mut sha = Sha256::new();
        let mut md = md5::Context::new();
        let result = (|| {
            let mut buf = [0; 65536];
            loop {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                ensure!(size <= max, "remote object exceeds expected size");
                sha.update(&buf[..n]);
                md.consume(&buf[..n]);
                f.write_all(&buf[..n])?;
            }
            ensure!(meta["size"] == size, "remote object length mismatch");
            f.sync_all()?;
            meta["sha256"] = json!(hex::encode(sha.finalize()));
            meta["body_etag"] = json!(format!("\"{:x}\"", md.finalize()));
            Ok(meta)
        })();
        if result.is_err() {
            drop(f);
            let _ = fs::remove_file(path);
        }
        result
    }
    pub fn put(&self, key: &str, body: &[u8], metadata: &Value) -> Result<()> {
        ensure!(body.len() as u64 <= MAX_BODY, "object size limit");
        let headers = metadata_headers(metadata)?;
        Self::checked(self.request(
            Method::PUT,
            key,
            &[],
            headers,
            Some(Body::from(body.to_vec())),
            &hash(body),
        )?)?;
        Ok(())
    }
    pub fn put_file(&self, key: &str, path: &Path, metadata: &Value) -> Result<()> {
        let mut file = safe_file(path)?;
        let before = file.metadata()?;
        let size = before.len();
        ensure!(size <= MAX_BODY, "object size limit");
        let mut sha = Sha256::new();
        let mut buf = [0; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
        }
        let digest = hex::encode(sha.finalize());
        if let Some(expected) = metadata.get("sha256") {
            ensure!(
                expected.as_str() == Some(&digest),
                "upload source digest mismatch"
            );
        }
        file.seek(SeekFrom::Start(0))?;
        let mut headers = metadata_headers(metadata)?;
        headers.insert("content-length", HeaderValue::from_str(&size.to_string())?);
        Self::checked(self.request(
            Method::PUT,
            key,
            &[],
            headers,
            Some(Body::sized(file, size)),
            &digest,
        )?)?;
        let after = fs::symlink_metadata(path)?;
        #[cfg(unix)]
        ensure!(
            after.is_file()
                && before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "upload source changed"
        );
        Ok(())
    }
    pub fn list(&self, prefix: &str, token: Option<&str>, limit: usize) -> Result<Value> {
        self.list_after(prefix, token, None, limit)
    }
    pub fn list_after(
        &self,
        prefix: &str,
        token: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        ensure!((1..=1000).contains(&limit), "listing page size");
        let mut query = vec![
            ("list-type".into(), "2".into()),
            ("prefix".into(), prefix.into()),
            ("max-keys".into(), limit.to_string()),
        ];
        if let Some(t) = token {
            ensure!(!t.is_empty() && t.len() <= 16384, "invalid token");
            query.push(("continuation-token".into(), t.into()));
        } else if let Some(a) = after {
            query.push(("start-after".into(), a.into()));
        }
        let response = Self::checked(self.request(
            Method::GET,
            "",
            &query,
            HeaderMap::new(),
            None,
            &hash(b""),
        )?)?;
        let bytes = bounded_response(response, MAX_METADATA)?;
        parse_listing(&bytes, prefix, limit)
    }
    pub fn versioning(&self) -> Result<Option<String>> {
        let r = Self::checked(self.request(
            Method::GET,
            "",
            &[("versioning".into(), "".into())],
            HeaderMap::new(),
            None,
            &hash(b""),
        )?)?;
        #[derive(Deserialize)]
        struct Version {
            #[serde(rename = "Status")]
            status: Option<String>,
        }
        let raw = bounded_response(r, 65536)?;
        xml_root(&raw, b"VersioningConfiguration")?;
        let v: Version = quick_xml::de::from_reader(raw.as_slice())?;
        Ok(v.status.filter(|s| !s.is_empty()))
    }
    pub fn delete(&self, keys: &[String]) -> Result<()> {
        ensure!(!keys.is_empty() && keys.len() <= 1000, "delete batch limit");
        ensure!(
            keys.iter().collect::<BTreeSet<_>>().len() == keys.len(),
            "duplicate delete key"
        );
        let mut body = String::from("<Delete><Quiet>false</Quiet>");
        for k in keys {
            body.push_str(&format!("<Object><Key>{}</Key></Object>", xml_escape(k)));
        }
        body.push_str("</Delete>");
        let mut h = HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/xml"));
        use base64::Engine;
        h.insert(
            "content-md5",
            HeaderValue::from_str(
                &base64::engine::general_purpose::STANDARD.encode(md5::compute(body.as_bytes()).0),
            )?,
        );
        let r = Self::checked(self.request(
            Method::POST,
            "",
            &[("delete".into(), "".into())],
            h,
            Some(Body::from(body.clone())),
            &hash(body.as_bytes()),
        )?)?;
        let raw = bounded_response(r, MAX_METADATA)?;
        verify_delete_result(&raw, keys)?;
        Ok(())
    }
    pub fn delete_one(&self, key: &str, etag: &str) -> Result<()> {
        let mut h = HeaderMap::new();
        h.insert("if-match", HeaderValue::from_str(etag)?);
        let r = Self::checked(self.request(Method::DELETE, key, &[], h, None, &hash(b""))?)?;
        ensure!(
            !r.headers().contains_key("x-amz-delete-marker")
                && r.headers()
                    .get("x-amz-version-id")
                    .is_none_or(|v| v == "null"),
            "unexpected versioned deletion"
        );
        Ok(())
    }
    pub fn copy(&self, source: &str, dest: &str, metadata: &Value) -> Result<()> {
        let mut h = metadata_headers(metadata)?;
        h.insert(
            "x-amz-copy-source",
            HeaderValue::from_str(&format!(
                "/{}/{}",
                encode(&self.bucket, false),
                encode(source, true)
            ))?,
        );
        h.insert(
            "x-amz-metadata-directive",
            HeaderValue::from_static("REPLACE"),
        );
        if let Some(etag) = metadata["etag"].as_str() {
            h.insert("x-amz-copy-source-if-match", HeaderValue::from_str(etag)?);
        }
        let r = Self::checked(self.request(Method::PUT, dest, &[], h, None, &hash(b""))?)?;
        let raw = bounded_response(r, 65536)?;
        ensure!(
            !std::str::from_utf8(&raw)?.contains("<Error>"),
            "S3 copy failed"
        );
        Ok(())
    }
}
pub fn read_guard(path: &Path, exclusive: bool, required: bool) -> Result<Option<File>> {
    use fs2::FileExt;
    let file = match safe_file(path) {
        Ok(file) => file,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && !required =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e.context("compaction guard unavailable")),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(if exclusive { 30 } else { 5 });
    loop {
        let result = if exclusive {
            file.try_lock_exclusive()
        } else {
            FileExt::try_lock_shared(&file)
        };
        match result {
            Ok(()) => return Ok(Some(file)),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn http_client(ca: Option<&str>, seconds: u64) -> Result<HttpClient> {
    let mut b = HttpClient::builder()
        .timeout(Duration::from_secs(seconds))
        .connect_timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(path) = ca {
        let raw = fs::read(path)?;
        b = b.add_root_certificate(reqwest::Certificate::from_pem(&raw)?);
    }
    Ok(b.build()?)
}
fn bounded_response(mut r: Response, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    r.by_ref().take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "S3 response budget");
    Ok(bytes)
}
fn metadata_headers(m: &Value) -> Result<HeaderMap> {
    let mut h = HeaderMap::new();
    for (field, header) in [
        ("content_type", "content-type"),
        ("cache_control", "cache-control"),
        ("sha256", "x-amz-meta-sha256"),
    ] {
        if let Some(value) = m[field].as_str() {
            h.insert(
                HeaderName::from_bytes(header.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
    }
    if let Some(extra) = m["metadata"].as_object() {
        for (k, v) in extra {
            if k == "sha256" && m["sha256"].is_string() {
                continue;
            }
            ensure!(
                k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid metadata key"
            );
            h.insert(
                HeaderName::from_bytes(format!("x-amz-meta-{k}").as_bytes())?,
                HeaderValue::from_str(
                    v.as_str()
                        .ok_or_else(|| anyhow!("invalid metadata value"))?,
                )?,
            );
        }
    }
    Ok(h)
}
fn response_metadata(r: &Response) -> Result<Value> {
    let h = r.headers();
    let get = |name| -> Result<&str> {
        Ok(h.get(name)
            .ok_or_else(|| anyhow!("missing object header"))?
            .to_str()?)
    };
    let modified = DateTime::parse_from_rfc2822(get("last-modified")?)?.timestamp();
    ensure!(modified >= 0, "invalid modified timestamp");
    let mut metadata = serde_json::Map::new();
    for (k, v) in h {
        if let Some(name) = k.as_str().strip_prefix("x-amz-meta-") {
            metadata.insert(name.to_owned(), json!(v.to_str()?));
        }
    }
    Ok(
        json!({"etag":get("etag")?,"modified":modified,"size":get("content-length")?.parse::<u64>()?,"content_type":h.get("content-type").and_then(|s|s.to_str().ok()).unwrap_or("application/octet-stream"),"cache_control":h.get("cache-control").and_then(|s|s.to_str().ok()),"metadata":metadata}),
    )
}
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[derive(Deserialize)]
struct ListResult {
    #[serde(rename = "IsTruncated")]
    truncated: bool,
    #[serde(rename = "NextContinuationToken")]
    next: Option<String>,
    #[serde(rename = "Contents", default)]
    contents: Vec<ListItem>,
}
#[derive(Deserialize)]
struct ListItem {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "LastModified")]
    modified: String,
}
fn xml_root(raw: &[u8], expected: &[u8]) -> Result<()> {
    let mut reader = quick_xml::Reader::from_reader(raw);
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) => {
                ensure!(
                    e.local_name().as_ref() == expected,
                    "unexpected S3 XML response"
                );
                return Ok(());
            }
            quick_xml::events::Event::Decl(_) | quick_xml::events::Event::Comment(_) => {}
            quick_xml::events::Event::Text(e) if e.as_ref().iter().all(u8::is_ascii_whitespace) => {
            }
            _ => bail!("invalid S3 XML document"),
        }
    }
}
fn verify_delete_result(raw: &[u8], keys: &[String]) -> Result<()> {
    xml_root(raw, b"DeleteResult")?;
    #[derive(Deserialize)]
    struct Deleted {
        #[serde(rename = "Key")]
        key: String,
        #[serde(rename = "DeleteMarker")]
        marker: Option<bool>,
        #[serde(rename = "VersionId")]
        version: Option<String>,
    }
    #[derive(Deserialize)]
    struct Failure {
        #[serde(rename = "Key")]
        _key: Option<String>,
    }
    #[derive(Deserialize)]
    struct ResultXml {
        #[serde(rename = "Deleted", default)]
        deleted: Vec<Deleted>,
        #[serde(rename = "Error", default)]
        errors: Vec<Failure>,
    }
    let result: ResultXml = quick_xml::de::from_reader(raw)?;
    ensure!(
        result.errors.is_empty() && result.deleted.len() == keys.len(),
        "partial S3 deletion failure"
    );
    let expected = keys.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut found = BTreeSet::new();
    for item in &result.deleted {
        ensure!(
            found.insert(item.key.as_str())
                && expected.contains(item.key.as_str())
                && item.marker != Some(true)
                && item.version.as_deref().is_none_or(|v| v == "null"),
            "unexpected deleted object"
        );
    }
    ensure!(found == expected, "incomplete S3 deletion result");
    Ok(())
}

pub fn parse_listing(raw: &[u8], prefix: &str, limit: usize) -> Result<Value> {
    ensure!(raw.len() as u64 <= MAX_METADATA, "listing byte limit");
    xml_root(raw, b"ListBucketResult")?;
    let p: ListResult = quick_xml::de::from_reader(raw)?;
    ensure!(p.contents.len() <= limit, "listing entry limit");
    let mut contents = Vec::new();
    let mut keys = BTreeSet::new();
    for o in p.contents {
        ensure!(
            o.key.starts_with(prefix)
                && keys.insert(o.key.clone())
                && o.etag.starts_with('"')
                && o.etag.ends_with('"')
                && !o.etag.contains(['\r', '\n']),
            "invalid listing entry"
        );
        let parsed = DateTime::parse_from_rfc3339(&o.modified)?;
        let modified = parsed.timestamp();
        let precise = modified as f64 + parsed.timestamp_subsec_nanos() as f64 / 1_000_000_000.;
        ensure!(
            modified >= 0 && modified as f64 <= crate::common::now() + 300.,
            "invalid object time"
        );
        contents.push(json!({"Key":o.key,"Size":o.size,"ETag":o.etag,"LastModified":modified,"LastModifiedPrecise":precise}));
    }
    if p.truncated {
        ensure!(
            p.next
                .as_ref()
                .is_some_and(|s| !s.is_empty() && s.len() <= 16384),
            "invalid pagination"
        );
    }
    Ok(json!({"Contents":contents,"IsTruncated":p.truncated,"NextContinuationToken":p.next}))
}

pub fn verify_sources(c: &Value, client: &Client, records: &[Value]) -> Result<Value> {
    let mut groups: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut repository_key: Option<String> = None;
    for rec in records {
        let name = text(rec, "key")?;
        let key = name
            .split('/')
            .nth(1)
            .ok_or_else(|| anyhow!("invalid source"))?;
        if let Some(expected) = &repository_key {
            ensure!(expected == key, "cross-repository verification plan");
        } else {
            repository_key = Some(key.to_owned());
        }
        let (sha, _) = parts(key, name)?;
        let prefix = format!("repositories/{key}/releases/{sha}/");
        if let Some(old) = groups
            .entry(prefix)
            .or_default()
            .insert(name.to_owned(), rec.clone())
        {
            ensure!(old == *rec, "conflicting source records");
        }
    }
    let mut tasks = Vec::new();
    for (prefix, group) in groups {
        let names = group.keys().cloned().collect::<Vec<_>>();
        for (n, chunk) in names.chunks(1000).enumerate() {
            tasks.push((
                prefix.clone(),
                if n == 0 {
                    None
                } else {
                    Some(names[n * 1000 - 1].clone())
                },
                chunk.last().unwrap().clone(),
                chunk
                    .iter()
                    .map(|k| (k.clone(), group[k].clone()))
                    .collect::<BTreeMap<_, _>>(),
            ));
        }
    }
    ensure!(tasks.len() <= 128, "reference listing budget");
    let pages = std::sync::atomic::AtomicUsize::new(0);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = c["publication"]["object_workers"].as_u64().unwrap_or(4);
    ensure!((1..=8).contains(&workers), "invalid workers");
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for _ in 0..workers {
            handles.push(scope.spawn(|| -> Result<()> {
                loop {
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((prefix, lower, upper, expected)) = tasks.get(n) else {
                        break;
                    };
                    let mut token = None;
                    let mut seen = BTreeSet::new();
                    let mut found = BTreeSet::new();
                    loop {
                        ensure!(
                            pages.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 128,
                            "reference page budget"
                        );
                        let page =
                            client.list_after(prefix, token.as_deref(), lower.as_deref(), 1000)?;
                        let mut upper_reached = false;
                        for item in page["Contents"].as_array().unwrap() {
                            let key = text(item, "Key")?;
                            upper_reached |= key >= upper.as_str();
                            if let Some(record) = expected.get(key) {
                                ensure!(
                                    found.insert(key.to_owned())
                                        && item["ETag"] == record["etag"]
                                        && item["Size"] == record["item"]["size"]
                                        && item["LastModified"] == record["modified"],
                                    "referenced source changed"
                                );
                            }
                        }
                        if upper_reached || page["IsTruncated"] == false {
                            break;
                        }
                        let t = text(&page, "NextContinuationToken")?.to_owned();
                        ensure!(seen.insert(t.clone()), "repeated continuation");
                        token = Some(t);
                    }
                    ensure!(found.len() == expected.len(), "referenced source missing");
                }
                Ok(())
            }));
        }
        let mut error = None;
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error.get_or_insert(e);
                }
                Err(_) => {
                    error.get_or_insert(anyhow!("source verifier panicked"));
                }
            }
        }
        if let Some(e) = error {
            Err(e)
        } else {
            Ok(())
        }
    })?;
    Ok(
        json!({"source_list_requests":pages.load(std::sync::atomic::Ordering::Relaxed),"source_ranges":tasks.len()}),
    )
}

pub fn run(command: &str, args: &[String], c: &Value) -> Result<()> {
    if command == "object-reader" {
        return reader::serve();
    }
    let result = maintenance::run(command, args, c)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    let code = if result["status"] == "busy" {
        2
    } else if (command == "check-restore"
        && (result["status"] != "complete" || result["diagnosis"] != "verified"))
        || (command == "close-restore" && result["status"] != "closed")
        || (command.starts_with("restore-audit-") && result["status"] != "complete")
    {
        1
    } else {
        0
    };
    if code != 0 {
        return Err(anyhow::Error::new(crate::common::ExitCode(code)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_delete_requires_exact_success_set_and_no_error_or_version_marker() {
        let keys = vec!["a".into(), "b".into()];
        assert!(verify_delete_result(b"<DeleteResult><Deleted><Key>a</Key></Deleted><Deleted><Key>b</Key></Deleted></DeleteResult>",&keys).is_ok());
        for body in ["<DeleteResult><Deleted><Key>a</Key></Deleted></DeleteResult>","<DeleteResult><Deleted><Key>a</Key></Deleted><Error><Key>b</Key></Error></DeleteResult>","<DeleteResult><Deleted><Key>a</Key></Deleted><Deleted><Key>a</Key></Deleted></DeleteResult>","<Error><Code>AccessDenied</Code></Error>","<DeleteResult><Deleted><Key>a</Key><DeleteMarker>true</DeleteMarker></Deleted><Deleted><Key>b</Key></Deleted></DeleteResult>"] {assert!(verify_delete_result(body.as_bytes(),&keys).is_err(),"{body}");}
    }
    #[test]
    fn traversal_and_reference_rejected() {
        for s in ["", "../a", "a//b", "/a", "a/.secret", "a/../b", "a/"] {
            assert!(relative(s).is_err(), "{s}");
        }
        assert!(relative("images/a+b%25.svg").is_ok());
        assert!(parts(
            "images",
            &format!("repositories/other/releases/{}/x", "a".repeat(40))
        )
        .is_err());
    }
    #[test]
    fn canonical_encoding() {
        assert_eq!(encode("a b+%/中", true), "a%20b%2B%25/%E4%B8%AD");
    }
    #[test]
    fn duplicate_manifest_refused() {
        let item = json!({"path":"images/a.png","size":3,"sha256":"a".repeat(64),"content_type":"image/png"});
        let m = json!({"key":"images","sha":"b".repeat(40),"files":[item.clone(),item]});
        assert!(validate_manifest(&m).is_err());
    }
    #[test]
    fn listing_requires_flag_and_prefix() {
        let raw=b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>other</Key><Size>1</Size><ETag>&quot;a&quot;</ETag><LastModified>2026-01-01T00:00:00Z</LastModified></Contents></ListBucketResult>";
        assert!(parse_listing(raw, "repositories/", 1000).is_err());
        assert!(parse_listing(b"<ListBucketResult/>", "", 1000).is_err());
    }
    #[test]
    fn signing_is_deterministic_and_scope_bound() {
        let u =
            Url::parse("https://s3.example.test/bucket/a%20b?list-type=2&prefix=x%2By").unwrap();
        let h = signing_headers(
            "GET",
            &u,
            &HeaderMap::new(),
            &hash(b""),
            "AKID",
            "secret",
            "20260101T010203Z",
        )
        .unwrap();
        let auth = h["authorization"].to_str().unwrap();
        assert!(auth.contains("Credential=AKID/20260101/us-east-1/s3/aws4_request"));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert!(auth.ends_with(
            "Signature=a723a1d98edc87f894a162cda91d6fd71c0bb4357a543b14c3f102ea67f6f0d1"
        ));
        assert_eq!(h["x-amz-date"], "20260101T010203Z");
    }
}

#[cfg(test)]
mod integration_tests;
