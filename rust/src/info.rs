//! Read-only viewer, fixed-operation host broker, and bounded numeric history.
use crate::common::{atomic, now};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tiny_http::{Header, Response, Server, StatusCode};
mod benchmark;
mod publication;
mod repair;
pub mod reports;
mod telemetry;
pub use telemetry::{
    cleanup_service as cleanup_status, collect, command as telemetry_command, environment,
    host_metrics, storage_summary,
};
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
pub fn read_json_bounded(path: &Path, limit: u64) -> Result<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file() && meta.len() <= limit, "unsafe JSON record");
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "oversized JSON record");
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn read_snapshot(path: &Path) -> Result<Value> {
    let mut data = read_json_bounded(path, 2 * 1024 * 1024)?;
    ensure!(
        data.is_object() && data["schema_version"] == 1,
        "unsupported snapshot"
    );
    let at = data["collected_at"]
        .as_f64()
        .context("invalid collection time")?;
    ensure!(
        at.is_finite() && at >= 0.0 && at <= now() + 300.0,
        "invalid collection time"
    );
    let age = (now() - at).round_ties_even().max(0.0);
    data["age_seconds"] = json!(age as u64);
    data["stale"] = json!(age > 120.0);
    Ok(data)
}
fn numeric(value: &Value, max: f64) -> Option<f64> {
    value
        .as_f64()
        .filter(|x| x.is_finite() && *x >= 0.0 && *x <= max)
}
pub fn history_point(host: &Value, at: f64) -> Result<Value> {
    ensure!(
        at.is_finite() && (0.0..=9007199254740992.0).contains(&at),
        "invalid history time"
    );
    let total = numeric(&host["memory"]["total"], 9007199254740992.0).filter(|x| *x > 0.0);
    let used = numeric(&host["memory"]["used"], 9007199254740992.0)
        .filter(|x| total.is_some_and(|t| *x <= t));
    let load: Vec<Value> = (0..3)
        .map(|i| numeric(&host["load"][i], 1e6).map_or(Value::Null, |x| json!(x)))
        .collect();
    Ok(
        json!({"at":at,"cpu_percent":numeric(&host["cpu_percent"],100.0),"memory_used":used,"memory_total":total,"load":load,"cpu_count":host["cpu_count"].as_u64().filter(|x|*x>=1&&*x<=65536)}),
    )
}
pub fn update_history(path: &Path, host: &Value, at: f64) -> Result<Value> {
    let mut buckets = BTreeMap::new();
    if let Ok(old) = read_json_bounded(path, 1024 * 1024) {
        if old["schema_version"] == 1 {
            if let Some(points) = old["points"].as_array() {
                for p in points.iter().rev().take(2881).rev() {
                    if let Some(t) = numeric(&p["at"], at).filter(|t| *t >= at - 86400.0) {
                        let h = json!({"cpu_percent":p["cpu_percent"],"cpu_count":p["cpu_count"],"memory":{"used":p["memory_used"],"total":p["memory_total"]},"load":p["load"]});
                        buckets.insert((t / 30.0) as u64, history_point(&h, t)?);
                    }
                }
            }
        }
    }
    buckets.insert((at / 30.0) as u64, history_point(host, at)?);
    let mut points: Vec<Value> = buckets.into_values().collect();
    if points.len() > 2881 {
        points.drain(..points.len() - 2881);
    }
    Ok(json!({"schema_version":1,"interval_seconds":30,"retention_seconds":86400,"points":points}))
}
pub fn write_snapshot(path: &Path, mut value: Value) -> Result<()> {
    let parent = path.parent().context("snapshot parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o750))?;
    if value["host"].is_object() {
        let history = update_history(
            &parent.join("host-history.json"),
            &value["host"],
            value["collected_at"].as_f64().context("collection time")?,
        )?;
        write_snapshot(&parent.join("host-history.json"), history.clone())?;
        value["host_history"] = history;
    }
    atomic(path, &value)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o640))?;
    if unsafe { libc::geteuid() } == 0 {
        for p in [parent, path] {
            let p = std::ffi::CString::new(p.as_os_str().as_encoded_bytes())?;
            ensure!(
                unsafe { libc::chown(p.as_ptr(), 0, 20010) } == 0,
                "snapshot ownership"
            );
        }
    }
    Ok(())
}
pub fn refresh_snapshot(socket: &Path) -> Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"REFRESH\n")?;
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(1025)
        .read_until(b'\n', &mut line)?;
    ensure!(line.len() <= 1024, "invalid broker response");
    let v: Value = serde_json::from_slice(&line)?;
    ensure!(
        matches!(
            v["status"].as_str(),
            Some("collected" | "busy" | "cooldown")
        ),
        "collection unavailable"
    );
    Ok(v)
}
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub mime: &'static str,
    pub body: Vec<u8>,
    pub location: Option<String>,
}
pub fn route(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    snapshot: &Path,
    auth: &str,
    socket: &Path,
) -> Reply {
    let mut reply = Reply {
        status: 200,
        mime: "text/plain",
        body: Vec::new(),
        location: None,
    };
    if method == "POST" {
        if url != "/api/refresh" {
            reply.status = 501;
            return reply;
        }
        if headers.get("x-open-assets-refresh").map(String::as_str) != Some("1")
            || headers.get("origin").is_some_and(|o| {
                o != &format!(
                    "https://{}",
                    headers.get("host").map(String::as_str).unwrap_or("")
                )
            })
            || headers
                .get("content-length")
                .map(String::as_str)
                .unwrap_or("0")
                != "0"
            || headers.contains_key("transfer-encoding")
        {
            reply.status = 403;
            return reply;
        }
        let v = refresh_snapshot(socket).unwrap_or_else(|_| json!({"status":"unavailable"}));
        reply.status = match v["status"].as_str() {
            Some("collected") => 200,
            Some("busy") => 409,
            Some("cooldown") => 429,
            _ => 503,
        };
        reply.mime = "application/json";
        reply.body = serde_json::to_vec(&v).unwrap_or_default();
        return reply;
    }
    if method != "GET" && method != "HEAD" {
        reply.status = 501;
        return reply;
    }
    match url.split('?').next().unwrap_or("") {
        "/session" => {
            let valid = url::Url::parse(auth).is_ok_and(|u| {
                u.scheme() == "https"
                    && u.host_str().is_some()
                    && matches!(u.path(), "" | "/")
                    && u.query().is_none()
                    && u.fragment().is_none()
                    && u.username().is_empty()
                    && u.password().is_none()
            }) && !auth.chars().any(|c| c < ' ');
            if valid {
                reply.status = 303;
                reply.location = Some(auth.into());
            } else {
                reply.status = 503;
                reply.body = b"Session entry unavailable".to_vec();
            }
        }
        "/healthz" => {
            reply.mime = "application/json";
            reply.body = b"{\"status\":\"running\"}".to_vec();
        }
        "/api/status" => {
            reply.mime = "application/json";
            match read_snapshot(snapshot) {
                Ok(v) => reply.body = serde_json::to_vec(&v).unwrap_or_default(),
                Err(_) => {
                    reply.status = 503;
                    reply.body = b"{\"error\":\"snapshot unavailable\"}".to_vec();
                }
            }
        }
        "/" => {
            reply.mime = "text/html";
            reply.body = include_bytes!("../../web/static/index.html").to_vec();
        }
        "/app.js" => {
            reply.mime = "application/javascript";
            reply.body = include_bytes!("../../web/static/app.js").to_vec();
        }
        "/style.css" => {
            reply.mime = "text/css";
            reply.body = include_bytes!("../../web/static/style.css").to_vec();
        }
        _ => {
            reply.status = 404;
            reply.body = b"Not found".to_vec();
        }
    }
    reply
}
pub fn serve() -> Result<()> {
    let bind = std::env::var("INFO_BIND").unwrap_or_else(|_| "0.0.0.0:8082".into());
    let server = Server::http(bind).map_err(|_| anyhow::anyhow!("Info bind failed"))?;
    let snapshot = PathBuf::from(
        std::env::var("INFO_SNAPSHOT").unwrap_or_else(|_| "/info/snapshot.json".into()),
    );
    let auth = std::env::var("INFO_AUTH_URL").unwrap_or_default();
    let socket = PathBuf::from(
        std::env::var("INFO_REFRESH_SOCKET")
            .unwrap_or_else(|_| "/run/info-refresh/control.sock".into()),
    );
    let (sender, receiver) = std::sync::mpsc::sync_channel::<tiny_http::Request>(32);
    let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
    for _ in 0..8 {
        let receiver = receiver.clone();
        let snapshot = snapshot.clone();
        let auth = auth.clone();
        let socket = socket.clone();
        std::thread::spawn(move || loop {
            let request = match receiver.lock() {
                Ok(r) => r.recv(),
                Err(_) => return,
            };
            let Ok(request) = request else {
                return;
            };
            let headers = request
                .headers()
                .iter()
                .map(|h| {
                    (
                        h.field.as_str().as_str().to_ascii_lowercase(),
                        h.value.to_string(),
                    )
                })
                .collect();
            let reply = route(
                request.method().as_str(),
                request.url(),
                &headers,
                &snapshot,
                &auth,
                &socket,
            );
            let mut response =
                Response::from_data(reply.body).with_status_code(StatusCode(reply.status));
            for (k, v) in [
                ("Content-Type", format!("{}; charset=utf-8", reply.mime)),
                ("Cache-Control", "no-store".into()),
                ("X-Content-Type-Options", "nosniff".into()),
                ("Referrer-Policy", "no-referrer".into()),
                ("Content-Security-Policy", CSP.into()),
            ] {
                if let Ok(h) = Header::from_bytes(k, v) {
                    response.add_header(h);
                }
            }
            if let Some(target) = reply.location {
                if let Ok(h) = Header::from_bytes("Location", target) {
                    response.add_header(h);
                }
            }
            let _ = request.respond(response);
        });
    }
    for request in server.incoming_requests() {
        if sender.send(request).is_err() {
            break;
        }
    }
    Ok(())
}
fn root_arg(args: &[String]) -> PathBuf {
    args.windows(2)
        .find(|w| w[0] == "--root")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from("/app/open-assets"))
}
pub fn lock_file(path: &Path, nonblocking: bool) -> Result<File> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(f.metadata()?.is_file(), "unsafe lock");
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    ensure!(
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), flags) } == 0,
        "operation busy"
    );
    Ok(f)
}
pub fn broker(root: &Path) -> Result<()> {
    ensure!(unsafe { libc::geteuid() } == 0, "run broker as root");
    let env = environment(root)?;
    let output =
        PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?).join("info/snapshot.json");
    let socket = output
        .parent()
        .context("info parent")?
        .parent()
        .context("data parent")?
        .join("info-refresh/control.sock");
    let parent = socket.parent().context("broker parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o750))?;
    if let Ok(meta) = fs::symlink_metadata(&socket) {
        use std::os::unix::fs::FileTypeExt;
        ensure!(
            meta.file_type().is_socket(),
            "refusing non-socket broker path"
        );
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    for p in [parent, &socket] {
        let p = std::ffi::CString::new(p.as_os_str().as_encoded_bytes())?;
        ensure!(
            unsafe { libc::chown(p.as_ptr(), 0, 20010) } == 0,
            "broker ownership"
        );
    }
    let mut last: Option<Instant> = None;
    for mut stream in listener.incoming().flatten() {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        #[cfg(target_os = "linux")]
        {
            let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ok = unsafe {
                libc::getsockopt(
                    std::os::fd::AsRawFd::as_raw_fd(&stream),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    &mut cred as *mut _ as *mut _,
                    &mut len,
                )
            };
            if ok != 0 || !matches!(cred.uid, 0 | 20010) {
                continue;
            }
        }
        let mut data = Vec::new();
        if BufReader::new(&stream)
            .take(32)
            .read_until(b'\n', &mut data)
            .is_err()
            || data != b"REFRESH\n"
        {
            continue;
        }
        let result = if last.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
            json!({"status":"cooldown"})
        } else {
            match lock_file(&output.with_file_name(".collector.lock"), true) {
                Err(_) => json!({"status":"busy"}),
                Ok(_guard) => match collect(root, true).and_then(|v| {
                    let at = v["collected_at"].clone();
                    write_snapshot(&output, v)?;
                    Ok(at)
                }) {
                    Ok(at) => {
                        last = Some(Instant::now());
                        json!({"status":"collected","collected_at":at})
                    }
                    Err(_) => json!({"status":"unavailable"}),
                },
            }
        };
        let _ = writeln!(stream, "{}", result);
    }
    Ok(())
}
pub fn worker_healthy(record: &Value, at: f64, max: f64) -> bool {
    let age = |key: &str| {
        record[key]
            .as_f64()
            .map(|n| at - n)
            .filter(|v| v.is_finite())
    };
    if !age("updated_at").is_some_and(|v| (-5.0..=60.0).contains(&v)) {
        return false;
    }
    if !record["job_started_at"].is_null() {
        age("job_started_at").is_some_and(|v| (0.0..=max).contains(&v))
    } else {
        age("progress_at").is_some_and(|v| (-5.0..=120.0).contains(&v))
    }
}
pub fn health(role: &str, c: &Value) -> Result<()> {
    if role == "publisher" {
        let loaded = if c.is_null() {
            crate::common::load(None)?
        } else {
            c.clone()
        };
        let c = &loaded;
        let path = Path::new(c["state_root"].as_str().context("state root")?)
            .join("worker-heartbeat.json");
        let record = read_json_bounded(&path, 4096)?;
        let max = std::env::var("WORKER_MAX_JOB_SECONDS")
            .unwrap_or_else(|_| "1800".into())
            .parse::<f64>()?;
        ensure!(
            max >= 60.0 && worker_healthy(&record, now(), max),
            "worker heartbeat invalid"
        );
    } else {
        let port = match role {
            "webhook" => 9000,
            "info" => 8082,
            "object-reader" => 8083,
            "public-limiter" => 8088,
            _ => bail!("unknown probe role"),
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()?;
        ensure!(
            client
                .get(format!("http://127.0.0.1:{port}/healthz"))
                .send()?
                .status()
                == 200,
            "local probe failed"
        );
    }
    Ok(())
}
pub fn run(command: &str, args: &[String], c: &Value) -> Result<()> {
    match command {
        "info" | "info-serve" => serve(),
        "benchmark" => benchmark::run(args),
        "info-refresh" => broker(&root_arg(args)),
        "telemetry" => {
            ensure!(unsafe { libc::geteuid() } == 0, "run collector as root");
            let root = root_arg(args);
            let env = environment(&root)?;
            let output = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?)
                .join("info/snapshot.json");
            let _guard = lock_file(&output.with_file_name(".collector.lock"), true)?;
            write_snapshot(
                &output,
                collect(&root, args.iter().any(|a| a == "--refresh-storage"))?,
            )
        }
        "health" => health(args.first().context("probe role")?, c),
        "storage-scan" => {
            crate::ops::host_only()?;
            let root = root_arg(args);
            let env = environment(&root)?;
            let data = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?);
            let _guard = lock_file(&data.join("info/.storage.lock"), true)?;
            println!("{}", storage_summary(&root, &data, true)?);
            Ok(())
        }
        "host-history" => {
            crate::ops::host_only()?;
            let root = root_arg(args);
            let env = environment(&root)?;
            let data = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?);
            let path = data.join("info/host-history.json");
            write_snapshot(&path, update_history(&path, &host_metrics()?, now())?)
        }
        "repair" => repair::run(args),
        "object-info" | "resource-info" | "audit-info" | "doctor-info" => {
            reports::run(command, args, c)
        }
        _ => bail!("unknown Info command"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_allowlist_and_bounds() {
        let h = json!({"cpu_percent":101,"cpu_count":true,"memory":{"used":5,"total":4},"load":[1,-1,"secret"],"token":"hidden"});
        let p = history_point(&h, 100.0).unwrap();
        assert!(p["cpu_percent"].is_null());
        assert!(p["memory_used"].is_null());
        assert!(p["cpu_count"].is_null());
        assert_eq!(p["load"], json!([1.0, null, null]));
        assert!(p.get("token").is_none());
    }
    #[test]
    fn history_rejects_invalid_time() {
        assert!(history_point(&json!({}), f64::NAN).is_err());
    }
    #[test]
    fn routes_fail_closed() {
        let p = Path::new("/not-present");
        assert_eq!(
            route("GET", "/../../private/.env", &BTreeMap::new(), p, "", p).status,
            404
        );
        assert_eq!(
            route("POST", "/api/refresh", &BTreeMap::new(), p, "", p).status,
            403
        );
        assert_eq!(
            route(
                "GET",
                "/session?next=evil",
                &BTreeMap::new(),
                p,
                "https://safe.example/",
                p
            )
            .location
            .as_deref(),
            Some("https://safe.example/")
        );
        for u in [
            "http://x/",
            "https://user@x/",
            "https://x/path",
            "https://x/?next=evil",
        ] {
            assert_eq!(
                route("GET", "/session", &BTreeMap::new(), p, u, p).status,
                503
            );
        }
    }
    #[test]
    fn heartbeat_limits() {
        let v = json!({"updated_at":100,"progress_at":0});
        assert!(worker_healthy(&v, 100.0, 1800.0));
        assert!(!worker_healthy(&v, 161.0, 1800.0));
    }
}
#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn snapshot_rejects_boolean_future_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("snapshot.json");
        for t in [json!(true), json!(-1), json!(now() + 301.0)] {
            fs::write(
                &p,
                serde_json::to_vec(&json!({"schema_version":1,"collected_at":t})).unwrap(),
            )
            .unwrap();
            assert!(read_snapshot(&p).is_err());
        }
        fs::write(
            &p,
            serde_json::to_vec(&json!({"schema_version":1,"collected_at":now()-180.0})).unwrap(),
        )
        .unwrap();
        assert_eq!(read_snapshot(&p).unwrap()["stale"], true);
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(read_snapshot(&link).is_err());
    }
    #[test]
    fn numeric_history_retains_only_one_point_per_bucket() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("history.json");
        let old = json!({"schema_version":1,"points":[{"at":30,"cpu_percent":12,"memory_used":1,"memory_total":2,"load":[0,1,2],"cpu_count":2,"secret":"hidden"},{"at":31,"cpu_percent":13},{"at":90000,"cpu_percent":99}]});
        fs::write(&p, serde_json::to_vec(&old).unwrap()).unwrap();
        let h = update_history(&p, &json!({"cpu_percent":22,"cpu_count":4}), 59.0).unwrap();
        let points = h["points"].as_array().unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0]["cpu_percent"], 22.0);
        assert!(points[0].get("secret").is_none());
    }
    #[test]
    fn refresh_rejects_origin_body_and_transfer_encoding() {
        let path = Path::new("/absent");
        let mut h = BTreeMap::from([
            ("x-open-assets-refresh".into(), "1".into()),
            ("host".into(), "info.example.com".into()),
            ("origin".into(), "https://evil.example.com".into()),
        ]);
        assert_eq!(
            route("POST", "/api/refresh", &h, path, "", path).status,
            403
        );
        h.insert("origin".into(), "https://info.example.com".into());
        h.insert("content-length".into(), "1".into());
        assert_eq!(
            route("POST", "/api/refresh", &h, path, "", path).status,
            403
        );
        h.insert("content-length".into(), "0".into());
        h.insert("transfer-encoding".into(), "chunked".into());
        assert_eq!(
            route("POST", "/api/refresh", &h, path, "", path).status,
            403
        );
    }
}
