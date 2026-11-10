use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::CString,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};
const SERVICES: [&str; 11] = [
    "gitea",
    "nginx",
    "oc",
    "otterio",
    "publisher",
    "stargate",
    "webhook",
    "info",
    "object-reader",
    "public-limiter",
    "traefik",
];
pub fn environment(root: &Path) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(root.join(".env"))?;
    Ok(text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect())
}
pub fn command(
    program: &str,
    args: &[&str],
    root: Option<&Path>,
    timeout: Duration,
) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(root) = root {
        cmd.current_dir(root);
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("host command unavailable")?;
    let stdout = child.stdout.take().context("command output")?;
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        let result = stdout.take(2 * 1024 * 1024 + 1).read_to_end(&mut b);
        (result, b)
    });
    let start = Instant::now();
    let result = loop {
        if let Some(status) = child.try_wait()? {
            break status.success();
        }
        if start.elapsed() > timeout {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            };
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    };
    let (read, bytes) = reader
        .join()
        .map_err(|_| anyhow::anyhow!("command output failed"))?;
    read?;
    ensure!(
        result && bytes.len() <= 2 * 1024 * 1024,
        "host command failed or exceeded budget"
    );
    Ok(String::from_utf8(bytes)?)
}
fn cpu_sample() -> Result<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat")?;
    let v: Vec<u64> = text
        .lines()
        .next()
        .context("cpu sample")?
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(v.len() == 8, "cpu sample");
    Ok((v.iter().sum(), v[3] + v[4]))
}
fn online_cpu_count() -> usize {
    // /proc load and CPU samples describe the host, not the collector's CPUQuota.
    // Match os.cpu_count() from the previous implementation even in a cgroup.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1)
}
pub fn host_metrics() -> Result<Value> {
    let mut fields = BTreeMap::new();
    for line in fs::read_to_string("/proc/meminfo")?.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if let Some(n) = value.split_whitespace().next() {
                fields.insert(key.to_owned(), n.parse::<u64>()?.saturating_mul(1024));
            }
        }
    }
    let get = |k: &str| fields.get(k).copied().unwrap_or(0);
    let total = get("MemTotal");
    let available = get("MemAvailable");
    let swap = get("SwapTotal");
    let load: Vec<f64> = fs::read_to_string("/proc/loadavg")?
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    let uptime: f64 = fs::read_to_string("/proc/uptime")?
        .split_whitespace()
        .next()
        .context("uptime")?
        .parse()?;
    let os = fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("PRETTY_NAME=")
                .map(|s| s.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| "Linux".into());
    Ok(
        json!({"cpu_count":online_cpu_count(),"cpu_percent":null,"load":load,"uptime_seconds":uptime,"os":os,"kernel":fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),"memory":{"total":total,"available":available,"used":total.saturating_sub(available)},"swap":{"total":swap,"used":swap.saturating_sub(get("SwapFree"))}}),
    )
}
fn disk(path: &Path, label: &str) -> Result<Value> {
    let p = CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::statvfs(p.as_ptr(), &mut v) } == 0,
        "filesystem unavailable"
    );
    let total = (v.f_blocks as u64).saturating_mul(v.f_frsize as u64);
    let used = total.saturating_sub((v.f_bfree as u64).saturating_mul(v.f_frsize as u64));
    let available = (v.f_bavail as u64).saturating_mul(v.f_frsize as u64);
    Ok(
        json!({"label":label,"total":total,"available":available,"used":used,"used_percent":if total==0{0.0}else{100.0*used as f64/total as f64},"inode_used_percent":if v.f_files==0{None}else{Some(100.0*(v.f_files-v.f_ffree)as f64/v.f_files as f64)}}),
    )
}
pub fn filesystems(data: &Path) -> Result<(Vec<Value>, usize)> {
    let mut seen = BTreeMap::new();
    let mut disks = Vec::new();
    let mut image = 0;
    let same = fs::metadata("/")?.dev() == fs::metadata(data)?.dev();
    for (path, label) in [
        (
            PathBuf::from("/"),
            if same {
                "系统 / 数据（同一文件系统）"
            } else {
                "系统"
            },
        ),
        (data.to_path_buf(), "数据"),
        (data.join("images"), "图片存储"),
        (data.join("publisher"), "Git / 发布状态"),
        (data.join("otterio"), "对象存储"),
    ] {
        if let Ok(m) = fs::metadata(&path) {
            let index = *seen.entry(m.dev()).or_insert_with(|| {
                let i = disks.len();
                if let Ok(d) = disk(&path, label) {
                    disks.push(d);
                }
                i
            });
            if path == data.join("images") {
                image = index;
            }
        }
    }
    Ok((disks, image))
}
pub(super) fn stats_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let index = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .context("metric unit")?;
    let n: f64 = text[..index].parse()?;
    let factor = match text[index..].trim() {
        "B" => 1.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "KiB" => 1024.0,
        "MiB" => 1048576.0,
        "GiB" => 1073741824.0,
        "TiB" => 1099511627776.0,
        _ => bail!("metric unit"),
    };
    ensure!(n.is_finite() && n >= 0.0, "metric value");
    Ok((n * factor).round_ties_even() as u64)
}
fn probe(
    address: &str,
    port: u16,
    path: &str,
    hostname: Option<&str>,
    ca: Option<&Path>,
) -> Result<f64> {
    let started = Instant::now();
    let host = hostname.unwrap_or(address);
    let scheme = if hostname.is_some() { "https" } else { "http" };
    let mut builder = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = ca {
        for cert in reqwest::Certificate::from_pem_bundle(&fs::read(ca)?)? {
            builder = builder.add_root_certificate(cert);
        }
    }
    if hostname.is_some() {
        builder = builder.resolve(host, format!("{address}:{port}").parse()?);
    }
    let response = builder
        .build()?
        .get(format!("{scheme}://{host}:{port}{path}"))
        .send()?;
    ensure!(response.status() == 200, "probe failed");
    Ok(started.elapsed().as_secs_f64() * 1000.0)
}
pub fn cleanup_service(service: &str) -> Value {
    if !service.starts_with("open-assets-") || !service.ends_with(".service") {
        return json!({"status":"unavailable"});
    }
    let output = command(
        "systemctl",
        &[
            "show",
            service,
            "--property=LoadState,ActiveState,SubState,Result,ConditionResult,ExecMainStatus,ExecMainExitTimestampMonotonic",
        ],
        None,
        Duration::from_secs(2),
    );
    match output {
        Err(_) => json!({"status":"unavailable"}),
        Ok(s) => {
            let p: BTreeMap<&str, &str> = s.lines().filter_map(|l| l.split_once('=')).collect();
            let v = |k| p.get(k).copied().unwrap_or("");
            let status = if v("LoadState") == "not-found" {
                "not-installed"
            } else if v("ConditionResult") == "no" {
                "condition-skipped"
            } else if matches!(
                v("ActiveState"),
                "active" | "activating" | "reloading" | "deactivating"
            ) {
                "running"
            } else if v("Result") != "success" && !v("Result").is_empty()
                || v("ExecMainStatus") != "0" && !v("ExecMainStatus").is_empty()
            {
                "failed"
            } else if v("ExecMainExitTimestampMonotonic")
                .parse::<u64>()
                .unwrap_or(0)
                > 0
            {
                "success"
            } else {
                "never"
            };
            json!({"status":status})
        }
    }
}
const METRICS: [&str; 16] = [
    "versions",
    "full_copies",
    "image_copies",
    "shared_image_views",
    "partial_versions",
    "local_bytes",
    "logical_bytes",
    "version_bytes",
    "git_bytes",
    "manifest_bytes",
    "trash_bytes",
    "staging_bytes",
    "other_bytes",
    "unusual_entries",
    "reserved_bytes",
    "objects",
];
fn safe_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
pub fn storage_summary(root: &Path, data: &Path, measure: bool) -> Result<Value> {
    let raw = fs::read(root.join("private/repositories.yaml"))?;
    ensure!(raw.len() <= 1024 * 1024, "configuration budget");
    let c: Value = serde_yaml::from_slice(&raw)?;
    let repositories = c["repositories"].as_array().context("repositories")?;
    ensure!(
        repositories.len() <= 100 && c["state_root"] == "/state" && c["images_root"] == "/images",
        "unsupported storage mapping"
    );
    let cache_path = data.join("info/repositories-cache.json");
    let cache = read_json_bounded(&cache_path, 4 * 1024 * 1024).unwrap_or(json!({}));
    let digest = hex::encode(Sha256::digest(&raw));
    let valid = cache["config_digest"] == digest && cache["measurement_version"] == 2;
    let mut rows = Vec::new();
    let mut measurements = serde_json::Map::new();
    let deadline = Instant::now() + Duration::from_secs(80);
    let mut entries = 0;
    let mut used_keys = BTreeSet::new();
    let mut markers = serde_json::Map::new();
    let cleanup = cleanup_results(data, repositories.iter().filter_map(|r| r["key"].as_str()));
    let mut failures = Vec::new();
    for r in repositories {
        let key = r["key"].as_str().context("key")?;
        ensure!(
            safe_key(key) && used_keys.insert(key),
            "invalid repository key"
        );
        let mut row = json!({"key":key,"cleanup":{"status":"never"},"publication_required_bytes":r["max_bytes"].as_u64().unwrap_or(1073741824).saturating_mul(2)});
        row["cleanup"] = cleanup
            .get(key)
            .cloned()
            .unwrap_or(json!({"status":"unavailable"}));
        let marker = source_marker(data, key);
        let mut metric = if valid {
            clean_metrics(&cache["repositories"][key])
        } else {
            None
        };
        if measure
            && (!valid
                || metric
                    .as_ref()
                    .is_none_or(|m| now() - m["measured_at"].as_f64().unwrap_or(0.0) >= 300.0)
                || cache["markers"][key] != marker
                || cache["failures"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == key)))
        {
            match measure_storage(data, key, deadline, &mut entries) {
                Ok(m) => metric = Some(m),
                Err(_) => row["measurement_status"] = json!("unavailable"),
            }
        }
        if let Some(m) = metric {
            if let Some(o) = m.as_object() {
                for (k, v) in o {
                    row[k] = v.clone();
                }
            }
            if row.get("measurement_status").is_none() {
                row["measurement_status"] = json!("ok");
            }
            let age = (now() - m["measured_at"].as_f64().unwrap_or(0.0))
                .round_ties_even()
                .max(0.0);
            row["measurement_age_seconds"] = json!(age);
            row["measurement_stale"] = json!(age > 600.0 || row["measurement_status"] != "ok");
            measurements.insert(key.into(), m);
            markers.insert(key.to_owned(), marker);
        } else {
            row["measurement_status"] = json!("unavailable");
            row["measurement_stale"] = json!(true);
        }
        match retention_settings(&c, r, data, key) {
            Ok((policy, rollback, quota)) => {
                row["policy"] = policy;
                row["rollback_records"] = json!(rollback);
                row["publication_required_bytes"] = json!(quota * 2);
            }
            Err(_) => {
                row["policy_status"] = json!("unavailable");
                row.as_object_mut()
                    .unwrap()
                    .remove("publication_required_bytes");
            }
        }
        if row["measurement_status"] != "ok" {
            failures.push(key.to_owned());
        }
        rows.push(row);
    }
    if measure {
        atomic(
            &cache_path,
            &json!({"schema_version":1,"measurement_version":2,"config_digest":digest,"repositories":measurements,"markers":markers,"failures":failures}),
        )?;
        fs::set_permissions(&cache_path, fs::Permissions::from_mode(0o600))?;
    }
    let status = if rows
        .iter()
        .all(|r| r["measurement_status"] == "ok" && r["policy_status"] != "unavailable")
    {
        "ok"
    } else {
        "partial"
    };
    Ok(
        json!({"status":status,"repositories":rows,"cleanup_service":cleanup_service("open-assets-retention.service")}),
    )
}
fn source_marker(data: &Path, key: &str) -> Value {
    json!([
        data.join("images/repos").join(key).join("releases"),
        data.join("publisher/published").join(key),
        data.join("publisher/git").join(key),
        data.join("publisher/retention-last.json")
    ]
    .map(|p| fs::symlink_metadata(p)
        .ok()
        .map(|m| m
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(m.mtime_nsec()))
        .unwrap_or(0)))
}
fn finite_time(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|n| n.is_finite() && n >= 0.0 && n <= now() + 300.0)
}
fn cleanup_results<'a>(
    data: &Path,
    keys: impl Iterator<Item = &'a str>,
) -> BTreeMap<String, Value> {
    let keys: Vec<_> = keys.collect();
    let path = data.join("publisher/retention-last.json");
    let mut result: BTreeMap<_, _> = keys
        .iter()
        .map(|k| (k.to_string(), json!({"status":"never"})))
        .collect();
    if path.exists() || fs::symlink_metadata(&path).is_ok() {
        let parsed = (|| -> Result<BTreeMap<String, Value>> {
            let value = read_json_bounded(&path, 4 * 1024 * 1024)?;
            ensure!(
                value["status"] == "complete"
                    && value["apply"] == true
                    && finite_time(&value["completed_at"]),
                "invalid cleanup result"
            );
            let rows = value["repositories"]
                .as_array()
                .context("cleanup repositories")?;
            let mut out: BTreeMap<_, _> = keys
                .iter()
                .map(|k| (k.to_string(), json!({"status":"not-in-last-run"})))
                .collect();
            for row in rows {
                let Some(key) = row["key"].as_str().filter(|k| out.contains_key(*k)) else {
                    continue;
                };
                let enabled = row["enabled"].as_bool().context("cleanup flag")?;
                let candidates = row["candidates"].as_array().context("cleanup candidates")?;
                let archives = row["archive_jobs"].as_array().context("cleanup archives")?;
                let removed = row
                    .get("trash_removed")
                    .map(|v| v.as_u64().context("cleanup count"))
                    .transpose()?
                    .unwrap_or(0);
                ensure!(
                    candidates.len() <= 100
                        && archives.len() <= 1000
                        && removed <= 100
                        && (!enabled || row.get("trash_removed").is_some()),
                    "cleanup count limit"
                );
                out.insert(key.into(),json!({"status":if enabled{"complete"}else{"disabled"},"completed_at":value["completed_at"],"claimed_versions":if enabled{candidates.len()}else{0},"removed_directories":if enabled{removed}else{0},"archived_jobs":if enabled{archives.len()}else{0}}));
            }
            Ok(out)
        })();
        result = parsed.unwrap_or_else(|_| {
            keys.iter()
                .map(|k| (k.to_string(), json!({"status":"unavailable"})))
                .collect()
        });
    }
    for key in keys {
        let path = data
            .join("publisher/retention-results")
            .join(format!("{key}.json"));
        if !path.exists() && fs::symlink_metadata(&path).is_err() {
            continue;
        }
        let parsed = (|| -> Result<Value> {
            let source = read_json_bounded(&path, 16384)?;
            ensure!(
                source["schema_version"] == 1
                    && source["key"] == key
                    && source["status"] == "complete"
                    && source["apply"] == true
                    && finite_time(&source["completed_at"]),
                "invalid cleanup summary"
            );
            for (name, max) in [
                ("claimed_versions", 100),
                ("removed_directories", 100),
                ("archived_jobs", 1000),
            ] {
                ensure!(
                    source[name].as_u64().is_some_and(|n| n <= max),
                    "cleanup count limit"
                );
            }
            Ok(
                json!({"status":"complete","completed_at":source["completed_at"],"claimed_versions":source["claimed_versions"],"removed_directories":source["removed_directories"],"archived_jobs":source["archived_jobs"]}),
            )
        })();
        result.insert(
            key.into(),
            parsed.unwrap_or(json!({"status":"unavailable"})),
        );
    }
    result
}

fn retention_settings(c: &Value, r: &Value, data: &Path, key: &str) -> Result<(Value, usize, u64)> {
    let mut p = json!({"enabled":false,"keep_last":5,"keep_days":30,"min_age_hours":24,"rollback_versions":2,"pinned_shas":[],"archive_jobs_after_days":30,"max_delete_versions":10,"max_delete_bytes":268435456,"max_archive_jobs":100});
    for source in [c.get("retention"), r.get("retention")]
        .into_iter()
        .flatten()
    {
        let m = source.as_object().context("invalid retention settings")?;
        for (k, v) in m {
            ensure!(p.get(k).is_some(), "unknown retention setting");
            p[k] = v.clone();
        }
    }
    ensure!(p["enabled"].is_boolean(), "invalid retention flag");
    for (k, lo, hi) in [
        ("keep_last", 1, 10000),
        ("keep_days", 0, 36500),
        ("min_age_hours", 1, 876000),
        ("rollback_versions", 1, 100),
        ("archive_jobs_after_days", 1, 36500),
        ("max_delete_versions", 1, 100),
        ("max_delete_bytes", 1, 10737418240u64),
        ("max_archive_jobs", 1, 1000),
    ] {
        ensure!(
            p[k].as_u64().is_some_and(|n| (lo..=hi).contains(&n)),
            "invalid retention limit"
        );
    }
    let pins = p["pinned_shas"].as_array().context("retention pins")?;
    ensure!(
        pins.len() <= 1000
            && pins
                .iter()
                .all(|s| s.as_str().is_some_and(crate::ops::valid_sha)),
        "invalid retention pins"
    );
    let pin_count = pins
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>()
        .len();
    p.as_object_mut().unwrap().remove("pinned_shas");
    p["pinned_count"] = json!(pin_count);
    let history = data
        .join("publisher/rollback-protection")
        .join(format!("{key}.json"));
    let count = if history.exists() || fs::symlink_metadata(&history).is_ok() {
        let v = read_json_bounded(&history, 16384)?;
        ensure!(v["key"] == key, "rollback binding mismatch");
        let shas = v["shas"].as_array().context("rollback history")?;
        ensure!(
            shas.len() <= 100
                && shas
                    .iter()
                    .all(|v| v.as_str().is_some_and(crate::ops::valid_sha)),
            "invalid rollback history"
        );
        shas.iter()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>()
            .len()
    } else {
        0
    };
    let quota = r
        .get("max_bytes")
        .map(|v| v.as_u64().context("publication reserve"))
        .transpose()?
        .unwrap_or(1073741824);
    ensure!(
        (1..=1000000000000000).contains(&quota),
        "publication reserve"
    );
    Ok((p, count, quota))
}

fn clean_metrics(v: &Value) -> Option<Value> {
    let at = v["measured_at"].as_f64()?;
    if !at.is_finite() || at < 0.0 || at > now() + 300.0 {
        return None;
    }
    let mut result = json!({"measured_at":at});
    for key in METRICS.iter().take(14) {
        result[*key] = json!(v[*key].as_u64()?);
    }
    Some(result)
}
fn measure_storage(
    data: &Path,
    key: &str,
    deadline: Instant,
    counter: &mut usize,
) -> Result<Value> {
    let mut metrics = json!({});
    for k in METRICS.iter().take(14) {
        metrics[*k] = json!(0);
    }
    let mut seen = HashSet::new();
    let mut versions = BTreeMap::<String, BTreeSet<&str>>::new();
    let mut embedded = BTreeSet::new();
    let mut direct = BTreeSet::new();
    for (root, fixed) in [
        (data.join("images/repos").join(key), None),
        (data.join("publisher/git").join(key), Some("git_bytes")),
        (
            data.join("publisher/manifests").join(key),
            Some("manifest_bytes"),
        ),
    ] {
        for p in [root.parent(), root.parent().and_then(Path::parent)]
            .into_iter()
            .flatten()
        {
            if fs::symlink_metadata(p).is_ok() {
                ensure!(fs::symlink_metadata(p)?.is_dir(), "linked storage parent");
            }
        }
        if !root.exists() {
            if fs::symlink_metadata(&root).is_ok() {
                bail!("linked storage root");
            }
            continue;
        }
        ensure!(fs::symlink_metadata(&root)?.is_dir(), "linked storage root");
        let mut stack = vec![root.clone()];
        while let Some(p) = stack.pop() {
            *counter += 1;
            ensure!(
                *counter <= 1_000_000 && Instant::now() <= deadline,
                "storage budget"
            );
            let m = fs::symlink_metadata(&p)?;
            let rel = p.strip_prefix(&root)?;
            let parts: Vec<_> = rel.iter().filter_map(|s| s.to_str()).collect();
            let category = fixed.unwrap_or_else(|| {
                if parts.first() == Some(&"releases") {
                    "version_bytes"
                } else if parts.first() == Some(&".retention-trash") {
                    "trash_bytes"
                } else if parts
                    .first()
                    .is_some_and(|p| p.starts_with(".staging-") || p.starts_with(".images-"))
                {
                    "staging_bytes"
                } else {
                    "other_bytes"
                }
            });
            if seen.insert((m.dev(), m.ino())) {
                let bytes = m.blocks().saturating_mul(512);
                for k in [category, "local_bytes"] {
                    metrics[k] = json!(metrics[k].as_u64().unwrap_or(0).saturating_add(bytes));
                }
                if m.is_file() {
                    metrics["logical_bytes"] = json!(metrics["logical_bytes"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(m.len()));
                }
            }
            if m.is_dir() {
                if fixed.is_none() && parts.len() == 2 && parts[0] == "releases" {
                    let (name, kind) = parts[1]
                        .strip_prefix("images-")
                        .map_or((parts[1], "full"), |s| (s, "images"));
                    if crate::ops::valid_sha(name) {
                        versions.entry(name.into()).or_default().insert(kind);
                    }
                }
                if fixed.is_none()
                    && parts.len() == 3
                    && parts[0] == "releases"
                    && parts[2] == "images"
                    && crate::ops::valid_sha(parts[1])
                {
                    embedded.insert(parts[1].to_owned());
                }
                for e in fs::read_dir(&p)? {
                    ensure!(
                        *counter + stack.len() < 1_000_000 && Instant::now() <= deadline,
                        "storage budget"
                    );
                    stack.push(e?.path());
                }
            } else if m.is_file() && fixed == Some("manifest_bytes") && parts.len() == 1 {
                if let Some(sha) = parts[0]
                    .strip_suffix(".json")
                    .filter(|s| crate::ops::valid_sha(s))
                {
                    let v = read_json_bounded(&p, 4 * 1024 * 1024)?;
                    if v["image_layout"] == "snapshot-v1" {
                        ensure!(
                            v["key"] == key && v["sha"] == sha,
                            "invalid shared snapshot"
                        );
                        direct.insert(sha.to_owned());
                    }
                }
            } else if !m.is_file() && !(parts == ["current"] && m.file_type().is_symlink()) {
                metrics["unusual_entries"] =
                    json!(metrics["unusual_entries"].as_u64().unwrap_or(0) + 1);
            }
        }
    }
    metrics["versions"] = json!(versions.len());
    metrics["full_copies"] = json!(versions.values().filter(|x| x.contains("full")).count());
    metrics["image_copies"] = json!(versions.values().filter(|x| x.contains("images")).count());
    metrics["shared_image_views"] = json!(versions
        .iter()
        .filter(|(sha, kinds)| kinds.contains("full")
            && direct.contains(*sha)
            && embedded.contains(*sha))
        .count());
    metrics["partial_versions"] = json!(versions
        .iter()
        .filter(|(sha, kinds)| !(kinds.contains("full")
            && (kinds.contains("images") || (direct.contains(*sha) && embedded.contains(*sha)))))
        .count());
    metrics["measured_at"] = json!(now());
    Ok(metrics)
}
pub fn capacity(
    disks: &[Value],
    storage: &Value,
    env: &BTreeMap<String, String>,
    image: usize,
) -> Value {
    let defaults = [
        ("INFO_DISK_WARN_PERCENT", 85u64),
        ("INFO_DISK_CRITICAL_PERCENT", 95),
        ("INFO_DISK_WARN_FREE_BYTES", 5368709120),
        ("INFO_DISK_CRITICAL_FREE_BYTES", 2147483648),
        ("INFO_INODE_WARN_PERCENT", 85),
        ("INFO_INODE_CRITICAL_PERCENT", 95),
    ];
    let mut limits = BTreeMap::new();
    for (k, v) in defaults {
        match env.get(k).map(|s| s.parse::<u64>()).unwrap_or(Ok(v)) {
            Ok(n) => {
                limits.insert(k, n);
            }
            Err(_) => {
                return json!({"level":"unknown","alerts":[{"code":"invalid-thresholds","severity":"warning"}]})
            }
        }
    }
    if !(0 < limits["INFO_DISK_WARN_PERCENT"]
        && limits["INFO_DISK_WARN_PERCENT"] < limits["INFO_DISK_CRITICAL_PERCENT"]
        && limits["INFO_DISK_CRITICAL_PERCENT"] <= 100
        && 0 < limits["INFO_INODE_WARN_PERCENT"]
        && limits["INFO_INODE_WARN_PERCENT"] < limits["INFO_INODE_CRITICAL_PERCENT"]
        && limits["INFO_INODE_CRITICAL_PERCENT"] <= 100
        && limits["INFO_DISK_CRITICAL_FREE_BYTES"] < limits["INFO_DISK_WARN_FREE_BYTES"])
    {
        return json!({"level":"unknown","alerts":[{"code":"invalid-thresholds","severity":"warning"}]});
    }
    let mut alerts = Vec::new();
    for (i, d) in disks.iter().enumerate() {
        for (resource, field, prefix) in [
            ("space", "used_percent", "INFO_DISK"),
            ("inode", "inode_used_percent", "INFO_INODE"),
        ] {
            if let Some(percent) = d[field].as_f64() {
                let severity =
                    if percent >= limits[&format!("{prefix}_CRITICAL_PERCENT").as_str()] as f64 {
                        Some("critical")
                    } else if percent >= limits[&format!("{prefix}_WARN_PERCENT").as_str()] as f64 {
                        Some("warning")
                    } else {
                        None
                    };
                if let Some(severity) = severity {
                    alerts.push(json!({"code":format!("{resource}-{severity}"),"severity":severity,"disk":i}));
                }
            } else {
                alerts.push(json!({"code":"inode-unavailable","severity":"warning","disk":i}));
            }
        }
        if let Some(free) = d["available"].as_u64() {
            for (k, severity) in [
                ("INFO_DISK_CRITICAL_FREE_BYTES", "critical"),
                ("INFO_DISK_WARN_FREE_BYTES", "warning"),
            ] {
                if free < limits[k] {
                    alerts.push(
                        json!({"code":format!("free-{severity}"),"severity":severity,"disk":i}),
                    );
                    break;
                }
            }
        }
    }
    if let Some(rows) = storage["repositories"].as_array() {
        for row in rows {
            if let Some(required) = row["publication_required_bytes"]
                .as_u64()
                .filter(|n| *n > 0)
            {
                if disks
                    .get(image)
                    .and_then(|d| d["available"].as_u64())
                    .is_some_and(|free| free < required)
                {
                    alerts.push(json!({"code":"publication-reserve","severity":"critical","key":row["key"],"required_bytes":required}));
                }
            }
        }
    }
    if storage["status"] != "ok" {
        alerts.push(json!({"code":"storage-unavailable","severity":"warning"}));
    } else if storage["repositories"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|r| r["measurement_stale"] == true))
    {
        alerts.push(json!({"code":"storage-stale","severity":"warning"}));
    }
    let level = if alerts.iter().any(|a| a["severity"] == "critical") {
        "critical"
    } else if alerts.is_empty() {
        "ok"
    } else {
        "warning"
    };
    json!({"level":level,"alerts":alerts,"thresholds":limits})
}
pub fn collect(root: &Path, refresh_storage: bool) -> Result<Value> {
    let started = Instant::now();
    let env = environment(root)?;
    let domains = read_json_bounded(&root.join("private/domains.json"), 16384)?;
    let data = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?);
    let first = cpu_sample()?;
    let mut ids: Vec<String> = command(
        "docker",
        &[
            "compose",
            "--project-directory",
            root.to_str().context("root encoding")?,
            "ps",
            "-aq",
        ],
        None,
        Duration::from_secs(20),
    )?
    .split_whitespace()
    .map(str::to_owned)
    .collect();
    if let Some(project) = env.get("TRAEFIK_PROJECT").filter(|p| !p.is_empty()) {
        ids.extend(
            command(
                "docker",
                &[
                    "ps",
                    "-aq",
                    "--filter",
                    "label=com.docker.compose.service=traefik",
                    "--filter",
                    &format!("label=com.docker.compose.project={project}"),
                ],
                None,
                Duration::from_secs(20),
            )?
            .split_whitespace()
            .map(str::to_owned),
        );
    }
    ids.sort();
    ids.dedup();
    let inspect: Vec<Value> = if ids.is_empty() {
        Vec::new()
    } else {
        let mut a = vec!["inspect"];
        a.extend(ids.iter().map(String::as_str));
        serde_json::from_str(&command("docker", &a, None, Duration::from_secs(20))?)?
    };
    let mut rows = BTreeMap::new();
    for raw in &inspect {
        let role = raw["Config"]["Labels"]["com.docker.compose.service"]
            .as_str()
            .unwrap_or("");
        if !SERVICES.contains(&role) {
            continue;
        }
        let id = raw["Id"].as_str().context("container ID")?;
        rows.insert(id.to_owned(),json!({"service":role,"image":raw["Config"]["Image"],"state":raw["State"]["Status"],"health":raw["State"]["Health"]["Status"].as_str().unwrap_or("not-configured"),"probe":"not-probed","restarts":raw["RestartCount"].as_u64().unwrap_or(0),"cpu_percent":null,"memory_bytes":null}));
    }
    let active: Vec<&str> = inspect
        .iter()
        .filter(|r| r["State"]["Running"] == true)
        .filter_map(|r| r["Id"].as_str())
        .filter(|id| rows.contains_key(*id))
        .collect();
    if !active.is_empty() {
        let mut a = vec!["stats", "--no-stream", "--format", "{{json .}}"];
        a.extend(active);
        if let Ok(output) = command("docker", &a, None, Duration::from_secs(20)) {
            for line in output.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if let Some((_, row)) = rows
                        .iter_mut()
                        .find(|(id, _)| id.starts_with(v["ID"].as_str().unwrap_or("!")))
                    {
                        row["cpu_percent"] = v["CPUPerc"]
                            .as_str()
                            .and_then(|s| s.trim_end_matches('%').parse::<f64>().ok())
                            .filter(|n| n.is_finite() && *n >= 0.0)
                            .map_or(Value::Null, |n| json!(n));
                        row["memory_bytes"] = v["MemUsage"]
                            .as_str()
                            .and_then(|s| stats_size(s.split('/').next().unwrap_or("")).ok())
                            .map_or(Value::Null, |n| json!(n));
                    }
                }
            }
        }
    }
    for raw in &inspect {
        let Some(row) = raw["Id"].as_str().and_then(|id| rows.get_mut(id)) else {
            continue;
        };
        if raw["State"]["Running"] != true {
            continue;
        }
        let role = row["service"].as_str().unwrap_or("").to_owned();
        let nets = raw["NetworkSettings"]["Networks"].as_object();
        let ip = nets
            .and_then(|n| {
                n.values()
                    .find_map(|v| v["IPAddress"].as_str().filter(|s| !s.is_empty()))
            })
            .unwrap_or("");
        let edge = nets
            .and_then(|n| n.get("open-assets-edge"))
            .and_then(|n| n["IPAddress"].as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(ip);
        let ca = root.join("private/tls/ca.crt");
        let target = match role.as_str() {
            "gitea" => Some(("127.0.0.1", 13000, "/api/healthz", None, None)),
            "stargate" => Some(("127.0.0.1", 18080, "/healthz", None, None)),
            "webhook" => Some((ip, 9000, "/healthz", None, None)),
            "nginx" => Some((edge, 8081, "/__health", None, None)),
            "otterio" => Some((
                "127.0.0.1",
                19000,
                "/otterio/health/ready",
                Some("storage.internal"),
                Some(ca.as_path()),
            )),
            "oc" => Some((
                edge,
                9090,
                "/",
                domains["storage"].as_str(),
                Some(ca.as_path()),
            )),
            "info" => Some((edge, 8082, "/healthz", None, None)),
            _ => None,
        };
        if let Some((a, p, path, h, ca)) = target {
            match probe(a, p, path, h, ca) {
                Ok(ms) => {
                    row["probe"] = json!("ok");
                    row["probe_latency_ms"] = json!(ms);
                }
                Err(_) => row["probe"] = json!("failed"),
            }
        }
        if role == "publisher" {
            row["probe"] = json!(if read_json_bounded(
                &data.join("publisher/worker-heartbeat.json"),
                4096
            )
            .is_ok_and(|v| worker_healthy(
                &v,
                now(),
                env.get("WORKER_MAX_JOB_SECONDS")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1800.0)
            )) {
                "ok"
            } else {
                "failed"
            });
        }
    }
    let mut components: Vec<Value> = rows.into_values().collect();
    for role in SERVICES {
        if !components.iter().any(|r| r["service"] == role) {
            components.push(json!({"service":role,"state":"missing","health":"missing","probe":"unavailable","restarts":0}));
        }
    }
    components.sort_by(|a, b| a["service"].as_str().cmp(&b["service"].as_str()));
    let second = cpu_sample()?;
    let mut host = host_metrics()?;
    host["cpu_percent"] = json!((100.0
        * (1.0
            - second.1.saturating_sub(first.1) as f64
                / second.0.saturating_sub(first.0).max(1) as f64))
        .clamp(0.0, 100.0));
    let (disks, image) = filesystems(&data)?;
    let storage = storage_summary(root, &data, false)
        .unwrap_or(json!({"status":"unavailable","repositories":[]}));
    let mut result = json!({"schema_version":1,"collected_at":now(),"host":host,"components":components,"disks":disks,"certificates":certificate_summary(root,&domains),"storage":storage,"collection_seconds":started.elapsed().as_secs_f64()});
    result["capacity"] = capacity(
        result["disks"].as_array().context("disks")?,
        &result["storage"],
        &env,
        image,
    );
    super::reports::augment(&mut result, root, &data)?;
    let repairs =
        read_json_bounded(&data.join("repair/state.json"), 1024 * 1024).unwrap_or(json!({}));
    let events=repairs["events"].as_array().map(|a|a.iter().rev().take(10).filter_map(|e|{let role=e["service"].as_str()?;if !SERVICES.contains(&role){return None;}Some(json!({"service":role,"time":e["time"].as_f64().filter(|t|t.is_finite()&&*t>=0.0),"reason":"persistent-unhealthy","result":match e["result"].as_str(){Some("ok"|"failed"|"skipped"|"attempt")=>e["result"].clone(),_=>json!("unavailable")}}))}).collect::<Vec<_>>()).unwrap_or_default();
    result["repairs"] = json!(events);
    let performance = root.join("private/performance/latest-summary.json");
    if performance.exists() {
        if let Ok(source) = read_json_bounded(&performance, 2 * 1024 * 1024) {
            if let Some(at) = source["collected_at"]
                .as_f64()
                .filter(|t| t.is_finite() && *t >= 0.0 && *t <= now() + 300.0)
            {
                if let Some(rows) = source["scenarios"].as_array().filter(|rs| rs.len() <= 100) {
                    let clean: Vec<_> = rows
                        .iter()
                        .filter_map(|row| {
                            let label = row["label"].as_str().filter(|s| {
                                s.chars().count() <= 256 && !s.chars().any(|c| c < ' ')
                            })?;
                            let mut clean = json!({"label":label});
                            for k in [
                                "concurrency",
                                "requests",
                                "errors",
                                "requests_per_second",
                                "p50_ms",
                                "p95_ms",
                                "p99_ms",
                            ] {
                                let n = row[k].as_f64().filter(|n| n.is_finite() && *n >= 0.0)?;
                                clean[k] = json!(n);
                            }
                            Some(clean)
                        })
                        .collect();
                    result["performance"] = json!({"collected_at":at,"scenarios":clean});
                }
            }
        }
    }
    if refresh_storage {
        let _ = command(
            "systemctl",
            &["start", "--no-block", "open-assets-storage-info.service"],
            None,
            Duration::from_secs(2),
        );
    }
    Ok(result)
}
fn certificate_dates(path: &Path, label: &str) -> Result<Value> {
    let output = command(
        "openssl",
        &[
            "x509",
            "-in",
            path.to_str().context("certificate path")?,
            "-noout",
            "-startdate",
            "-enddate",
        ],
        None,
        Duration::from_secs(5),
    )?;
    let dates: BTreeMap<&str, &str> = output.lines().filter_map(|l| l.split_once('=')).collect();
    let parse = |s: &str| -> Result<i64> {
        Ok(
            chrono::NaiveDateTime::parse_from_str(s, "%b %e %H:%M:%S %Y GMT")?
                .and_utc()
                .timestamp(),
        )
    };
    let start = parse(dates.get("notBefore").context("certificate start")?)?;
    let end = parse(dates.get("notAfter").context("certificate end")?)?;
    Ok(
        json!({"label":label,"not_before":start,"not_after":end,"days_remaining":(end as f64-now())/86400.0}),
    )
}
fn public_certificate(host: &str, label: &str) -> Value {
    let result = (|| -> Result<Value> {
        ensure!(
            !host.is_empty()
                && host.len() <= 253
                && host.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'-'),
            "invalid certificate hostname"
        );
        let output = command(
            "openssl",
            &[
                "s_client",
                "-connect",
                "127.0.0.1:443",
                "-servername",
                host,
                "-verify_hostname",
                host,
                "-verify_return_error",
                "-showcerts",
            ],
            None,
            Duration::from_secs(3),
        )?;
        let begin = output
            .find("-----BEGIN CERTIFICATE-----")
            .context("public certificate")?;
        let tail = &output[begin..];
        let end = tail
            .find("-----END CERTIFICATE-----")
            .context("public certificate")?
            + "-----END CERTIFICATE-----".len();
        let mut f = tempfile::NamedTempFile::new()?;
        f.write_all(&tail.as_bytes()[..end])?;
        certificate_dates(f.path(), label)
    })();
    result.unwrap_or(json!({"label":label,"error":"unavailable"}))
}
fn certificate_summary(root: &Path, domains: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    for (name, label) in [
        ("storage/public.crt", "内部 / OtterIO"),
        ("console/console.crt", "内部 / OC"),
    ] {
        rows.push(
            certificate_dates(&root.join("private/tls").join(name), label)
                .unwrap_or(json!({"label":label,"error":"unavailable"})),
        );
    }
    let issuers = root.join("private/tls/issuers");
    let mut found = false;
    if let Ok(entries) = fs::read_dir(&issuers) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries.into_iter().take(100) {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let label = format!("内部 CA / {}", entry.file_name().to_string_lossy());
                rows.push(
                    certificate_dates(&entry.path().join("root.crt"), &label)
                        .unwrap_or(json!({"label":label,"error":"unavailable"})),
                );
                found = true;
            }
        }
    }
    if !found {
        rows.push(
            certificate_dates(&root.join("private/tls/ca.crt"), "内部 CA")
                .unwrap_or(json!({"label":"内部 CA","error":"unavailable"})),
        );
    }
    if let Some(domains) = domains.as_object() {
        let entries: Vec<_> = domains
            .iter()
            .map(|(role, host)| {
                (
                    format!("公网 / {role}"),
                    host.as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        for chunk in entries.chunks(4) {
            std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|(label, host)| scope.spawn(move || public_certificate(host, label)))
                    .collect();
                for handle in handles {
                    if let Ok(row) = handle.join() {
                        rows.push(row);
                    }
                }
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stats_units() {
        assert_eq!(stats_size("1.5MiB").unwrap(), 1572864);
        assert!(stats_size("NaNMiB").is_err());
    }
    #[test]
    fn invalid_capacity_configuration() {
        let mut e = BTreeMap::new();
        e.insert("INFO_DISK_WARN_PERCENT".into(), "101".into());
        assert_eq!(capacity(&[], &json!({}), &e, 0)["level"], "unknown");
    }
}
#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn physical_measurement_deduplicates_shared_inodes() {
        let d = tempfile::tempdir().unwrap();
        let key = "assets";
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let base = d.path().join("images/repos/assets/releases");
        let manifests = d.path().join("publisher/manifests/assets");
        fs::create_dir_all(&manifests).unwrap();
        let mut manifest_bytes = 0;
        for sha in [&a, &b] {
            fs::create_dir_all(base.join(sha).join("images")).unwrap();
            let data =
                serde_json::to_vec(&json!({"image_layout":"snapshot-v1","key":key,"sha":sha}))
                    .unwrap();
            manifest_bytes += data.len() as u64;
            fs::write(manifests.join(format!("{sha}.json")), data).unwrap();
        }
        let source = base.join(&a).join("images/shared.png");
        fs::write(&source, vec![7u8; 4096]).unwrap();
        fs::hard_link(&source, base.join(&b).join("images/shared.png")).unwrap();
        let m = measure_storage(
            d.path(),
            key,
            Instant::now() + Duration::from_secs(2),
            &mut 0,
        )
        .unwrap();
        assert_eq!(m["logical_bytes"], 4096 + manifest_bytes);
        assert_eq!(m["versions"], 2);
        assert_eq!(m["shared_image_views"], 2);
        assert_eq!(m["partial_versions"], 0);
    }
    #[test]
    fn linked_storage_parent_refused() {
        let d = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("images")).unwrap();
        std::os::unix::fs::symlink(external.path(), d.path().join("images/repos")).unwrap();
        fs::create_dir(external.path().join("assets")).unwrap();
        assert!(measure_storage(
            d.path(),
            "assets",
            Instant::now() + Duration::from_secs(1),
            &mut 0
        )
        .is_err());
    }
    #[test]
    fn durable_cleanup_beats_legacy_and_bad_counts_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("publisher/retention-results");
        fs::create_dir_all(&folder).unwrap();
        let record = json!({"schema_version":1,"key":"assets","status":"complete","apply":true,"completed_at":now(),"claimed_versions":2,"removed_directories":1,"archived_jobs":3});
        fs::write(
            folder.join("assets.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let r = cleanup_results(d.path(), ["assets"].into_iter());
        assert_eq!(r["assets"]["claimed_versions"], 2);
        let mut bad = record;
        bad["claimed_versions"] = json!(true);
        fs::write(
            folder.join("assets.json"),
            serde_json::to_vec(&bad).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cleanup_results(d.path(), ["assets"].into_iter())["assets"]["status"],
            "unavailable"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod online_cpu_tests {
    use super::*;
    #[test]
    fn host_cpu_count_matches_proc_even_with_collector_cpu_quota() {
        let stat = fs::read_to_string("/proc/stat").unwrap();
        let online = stat
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| {
                name.strip_prefix("cpu").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
                })
            })
            .count();
        assert!(online > 0);
        assert_eq!(
            host_metrics().unwrap()["cpu_count"].as_u64(),
            Some(online as u64)
        );
    }
}
