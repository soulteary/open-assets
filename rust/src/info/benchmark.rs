//! Rate- and duration-bounded loopback HTTPS acceptance benchmark.
use super::*;
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
const MAX_BODY: u64 = 16 * 1024 * 1024;
const URL_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
fn rounded(n: f64, precision: i32) -> f64 {
    let factor = 10f64.powi(precision);
    (n * factor).round_ties_even() / factor
}
pub fn percentile(values: &[f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    values[((values.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1)]
}
fn resources() -> Result<Value> {
    let output = telemetry_command(
        "docker",
        &["stats", "--no-stream", "--format", "{{json .}}"],
        None,
        Duration::from_secs(20),
    )?;
    let mut rows = Vec::new();
    for line in output.lines() {
        let raw: Value = serde_json::from_str(line)?;
        let name = raw["Name"].as_str().context("container logical name")?;
        if let Some(role) = [
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
        ]
        .iter()
        .find(|role| name.ends_with(&format!("-{role}-1")))
        {
            let cpu = raw["CPUPerc"]
                .as_str()
                .context("CPU sample")?
                .trim_end_matches('%')
                .parse::<f64>()?;
            ensure!(cpu.is_finite() && cpu >= 0.0, "invalid CPU metric");
            rows.push(json!({"service":role,"cpu_percent":cpu,"memory_bytes":super::telemetry::stats_size(raw["MemUsage"].as_str().context("memory sample")?.split('/').next().context("memory value")?)?}));
        }
    }
    let host = host_metrics()?;
    Ok(json!({"collected_at":now(),"components":rows,"memory":host["memory"],"load":host["load"]}))
}
pub fn summarize_resources(samples: &[Value]) -> Value {
    let roles: std::collections::BTreeSet<_> = samples
        .iter()
        .filter_map(|s| s["components"].as_array())
        .flatten()
        .filter_map(|r| r["service"].as_str())
        .collect();
    let mut result = Vec::new();
    for role in roles {
        let rows: Vec<_> = samples
            .iter()
            .filter_map(|s| s["components"].as_array())
            .flatten()
            .filter(|r| r["service"] == role)
            .collect();
        let cpu: Vec<_> = rows
            .iter()
            .filter_map(|r| r["cpu_percent"].as_f64())
            .collect();
        let memory: Vec<_> = rows
            .iter()
            .filter_map(|r| r["memory_bytes"].as_u64())
            .collect();
        if cpu.len() != rows.len() || memory.len() != rows.len() || rows.is_empty() {
            continue;
        }
        result.push(json!({"service":role,"samples":rows.len(),"cpu_mean":rounded(cpu.iter().sum::<f64>()/rows.len()as f64,2),"cpu_max":cpu.iter().copied().fold(0.0,f64::max),"memory_mean_bytes":(memory.iter().map(|n|*n as f64).sum::<f64>()/rows.len()as f64).round_ties_even()as u64,"memory_max_bytes":memory.into_iter().max().unwrap_or(0)}));
    }
    json!(result)
}
fn safe_resources(data: &Path) -> Result<()> {
    let host = host_metrics()?;
    ensure!(
        host["memory"]["available"]
            .as_u64()
            .is_some_and(|n| n >= 512 * 1024 * 1024),
        "aborted: available memory below safety threshold"
    );
    ensure!(
        host["load"][0]
            .as_f64()
            .is_some_and(|n| n <= host["cpu_count"].as_u64().unwrap_or(1) as f64 * 2.0),
        "aborted: host load above safety threshold"
    );
    let p = std::ffi::CString::new(data.as_os_str().as_encoded_bytes())?;
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::statvfs(p.as_ptr(), &mut v) } == 0,
        "disk safety sample unavailable"
    );
    ensure!(
        (v.f_bavail as u64).saturating_mul(v.f_frsize as u64) >= 2 * 1024 * 1024 * 1024,
        "aborted: free disk below safety threshold"
    );
    Ok(())
}
struct Measurements {
    times: Vec<f64>,
    errors: usize,
    bytes: u64,
    next: Instant,
}
struct ReadTarget {
    host: String,
    path: String,
    expected: String,
    label: String,
}
fn scenario(
    target: &ReadTarget,
    concurrency: usize,
    seconds: u64,
    max_rps: u64,
    data: &Path,
) -> Result<Value> {
    let ReadTarget {
        host,
        path,
        expected,
        label,
    } = target;
    let (host, path, expected, label) = (
        host.as_str(),
        path.as_str(),
        expected.as_str(),
        label.as_str(),
    );
    safe_resources(data)?;
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid fixture digest"
    );
    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let state = Mutex::new(Measurements {
        times: Vec::new(),
        errors: 0,
        bytes: 0,
        next: start,
    });
    let stop = AtomicBool::new(false);
    let encoded = path
        .split('/')
        .map(|p| percent_encoding::utf8_percent_encode(p, URL_ENCODE).to_string())
        .collect::<Vec<_>>()
        .join("/");
    let url = format!("https://{host}{encoded}");
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, "127.0.0.1:443".parse()?)
        .pool_max_idle_per_host(concurrency)
        .build()?;
    let monitoring = std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for _ in 0..concurrency {
            let client = client.clone();
            let state = &state;
            let stop = &stop;
            let url = &url;
            handles.push(scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let slot = {
                        let Ok(mut s) = state.lock() else {
                            return;
                        };
                        let slot = s.next.max(Instant::now());
                        s.next = slot + Duration::from_secs_f64(1.0 / max_rps as f64);
                        slot
                    };
                    if slot >= deadline {
                        break;
                    }
                    while Instant::now() < slot && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(
                            slot.saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(50)),
                        );
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let began = Instant::now();
                    let read = (|| -> Result<usize> {
                        let mut response = client.get(url.as_str()).send()?;
                        ensure!(response.status() == 200, "unexpected benchmark response");
                        let mut bytes = Vec::new();
                        response
                            .by_ref()
                            .take(MAX_BODY + 1)
                            .read_to_end(&mut bytes)?;
                        ensure!(
                            bytes.len() as u64 <= MAX_BODY
                                && hex::encode(Sha256::digest(&bytes)) == expected,
                            "benchmark byte mismatch"
                        );
                        Ok(bytes.len())
                    })();
                    let Ok(mut s) = state.lock() else {
                        return;
                    };
                    match read {
                        Ok(n) => {
                            s.times.push(began.elapsed().as_secs_f64() * 1000.0);
                            s.bytes += n as u64;
                        }
                        Err(_) => {
                            s.errors += 1;
                            if s.errors >= 5 {
                                stop.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }));
        }
        let mut failure = None;
        while Instant::now() < deadline && !handles.iter().all(|h| h.is_finished()) {
            if let Err(e) = safe_resources(data) {
                failure = Some(e);
                stop.store(true, Ordering::Relaxed);
                break;
            }
            for _ in 0..10 {
                if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("benchmark worker failed"))?;
        }
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(())
    });
    monitoring?;
    let elapsed = start.elapsed().as_secs_f64();
    let state = state
        .into_inner()
        .map_err(|_| anyhow::anyhow!("benchmark results unavailable"))?;
    ensure!(
        state.errors == 0,
        "benchmark encountered errors; do not continue load generation"
    );
    let requests = state.times.len() + state.errors;
    Ok(
        json!({"label":label,"concurrency":concurrency,"duration_seconds":rounded(elapsed,3),"max_rps":max_rps,"requests":requests,"errors":state.errors,"requests_per_second":rounded(requests as f64/elapsed,3),"mib_per_second":rounded(state.bytes as f64/1048576.0/elapsed,3),"p50_ms":percentile(&state.times,0.5),"p95_ms":percentile(&state.times,0.95),"p99_ms":percentile(&state.times,0.99)}),
    )
}
fn targets(root: &Path, key: &str) -> Result<(PathBuf, Vec<ReadTarget>)> {
    let env = environment(root)?;
    let data = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?);
    let c = crate::common::load(Some(&root.join("private/repositories.yaml")))?;
    let r = crate::common::repo(&c, key)?;
    let manifest = read_json_bounded(&data.join("publisher/published").join(key), 4 * 1024 * 1024)?;
    ensure!(
        manifest["key"] == key
            && manifest["sha"]
                .as_str()
                .is_some_and(crate::common::valid_sha),
        "invalid published fixture"
    );
    let files = manifest["files"].as_array().context("fixture files")?;
    let find = |image: bool| {
        files
            .iter()
            .find(|row| {
                row["path"].as_str().is_some_and(|p| {
                    p.starts_with("images/") == image && crate::common::relative(p)
                }) && row["size"].as_u64().is_some_and(|n| n <= MAX_BODY)
            })
            .context("published fixture needs both image and attachment")
    };
    let image = find(true)?;
    let attachment = find(false)?;
    let domains = read_json_bounded(&root.join("private/domains.json"), 16384)?;
    let sha = manifest["sha"].as_str().unwrap();
    let mut results = Vec::new();
    for (role, path, row, label) in [
        (
            "img",
            format!(
                "{}{}",
                r["mount_path"].as_str().context("mount")?,
                image["path"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("images/")
                    .unwrap()
            ),
            image,
            "public-image",
        ),
        (
            "oss",
            format!(
                "/attachments/repositories/{key}/releases/{sha}/{}",
                attachment["path"].as_str().unwrap()
            ),
            attachment,
            "public-attachment",
        ),
    ] {
        let host = domains[role].as_str().context("fixture host")?;
        ensure!(
            host.len() <= 253
                && host.bytes().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || c == b'.'
                    || c == b'-'),
            "invalid fixture hostname"
        );
        results.push(ReadTarget {
            host: host.into(),
            path,
            expected: row["sha256"].as_str().context("fixture SHA256")?.into(),
            label: label.into(),
        });
    }
    Ok((data, results))
}
pub fn run(args: &[String]) -> Result<()> {
    crate::ops::host_only()?;
    let root = crate::ops::protected_root(&crate::ops::root_arg(args))?;
    let parse = |key: &str, default: u64| -> Result<u64> {
        Ok(crate::ops::flag(args, key)
            .map(str::parse)
            .transpose()?
            .unwrap_or(default))
    };
    let seconds = parse("--seconds", 10)?;
    let max_rps = parse("--max-rps", 40)?;
    ensure!(
        (5..=30).contains(&seconds) && (1..=100).contains(&max_rps),
        "benchmark safety bounds"
    );
    let baseline = args.iter().any(|a| a == "--baseline");
    let concurrency = if let Some(i) = args.iter().position(|a| a == "--concurrency") {
        let mut values = Vec::new();
        for s in &args[i + 1..] {
            if s.starts_with('-') {
                break;
            }
            let n = s.parse::<usize>()?;
            ensure!([1, 2, 4, 8].contains(&n), "benchmark concurrency bound");
            values.push(n);
        }
        ensure!(
            !values.is_empty() && values.len() <= 4,
            "benchmark concurrency list"
        );
        values
    } else {
        vec![1, 4, 8]
    };
    let reads = if baseline {
        None
    } else {
        Some(targets(
            &root,
            crate::ops::flag(args, "--key").context("--key required")?,
        )?)
    };
    let output = root.join("private/performance");
    fs::create_dir_all(&output)?;
    ensure!(
        fs::symlink_metadata(&output)?.is_dir(),
        "unsafe performance directory"
    );
    fs::set_permissions(&output, fs::Permissions::from_mode(0o700))?;
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let failed = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        let samples = samples.clone();
        let failed = failed.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match resources() {
                    Ok(v) => {
                        if let Ok(mut samples) = samples.lock() {
                            samples.push(v);
                        }
                    }
                    Err(_) => {
                        failed.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                for _ in 0..30 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
    };
    let mut scenarios = Vec::new();
    let result = (|| -> Result<()> {
        if baseline {
            std::thread::sleep(Duration::from_secs(seconds));
        } else {
            let (data, reads) = reads.as_ref().context("benchmark fixtures")?;
            for n in concurrency {
                for target in reads {
                    scenarios.push(scenario(target, n, seconds, max_rps, data)?);
                }
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    sampler
        .join()
        .map_err(|_| anyhow::anyhow!("resource sampler failed"))?;
    result?;
    ensure!(!failed.load(Ordering::Relaxed), "resource sampling failed");
    let samples = samples
        .lock()
        .map_err(|_| anyhow::anyhow!("resource samples unavailable"))?
        .clone();
    let host = host_metrics()?;
    let mut result = json!({"collected_at":now(),"mode":if baseline{"baseline"}else{"bounded-loopback-https"},"cpu_count":host["cpu_count"],"memory_total_bytes":host["memory"]["total"],"scenarios":scenarios,"resources":summarize_resources(&samples),"samples":samples});
    crate::ops::private_write(
        &output.join(if baseline {
            "baseline.json"
        } else {
            "latest-summary.json"
        }),
        &serde_json::to_vec_pretty(&result)?,
        None,
    )?;
    result.as_object_mut().unwrap().remove("samples");
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentile_matches_ceil_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.95), 95.0);
        assert_eq!(percentile(&[], 0.95), 0.0);
    }
    #[test]
    fn resource_summary_retains_only_logical_metrics() {
        let v = summarize_resources(&[
            json!({"components":[{"service":"nginx","cpu_percent":2,"memory_bytes":10,"token":"hidden"}]}),
            json!({"components":[{"service":"nginx","cpu_percent":4,"memory_bytes":20}]}),
        ]);
        assert_eq!(v[0]["memory_mean_bytes"], 15);
        assert_eq!(v[0]["cpu_mean"], 3.0);
        assert!(v[0].get("token").is_none());
    }
}
