//! Bounded, allowlisted publication observations. Never reads asset bodies.
use super::*;
const LIMIT: usize = 100;
const STAGES: &[&str] = &[
    "queue",
    "fetch",
    "export",
    "objects",
    "snapshot",
    "activation",
    "evidence",
    "total",
    "observed",
];
fn seconds(v: &Value) -> Option<f64> {
    numeric(v, 1e12)
}
fn clean(j: &Value) -> Result<Value> {
    let mut row = json!({"key":j["key"],"sha":j["sha"],"status":j["status"],"created":seconds(&j["created"]),"finished":seconds(&j["finished"]),"timings":{},"details":{}});
    for k in STAGES {
        let source = match *k {
            "queue" => &j["queue_seconds"],
            "observed" => &j["observed_seconds"],
            _ => &j["timings"][*k],
        };
        if let Some(n) = seconds(source) {
            row["timings"][*k] = json!(n);
        }
    }
    for k in [
        "planning",
        "export_validate",
        "source_check",
        "source_check_wait",
        "export_check_window",
        "remote_head",
        "switch",
    ] {
        if let Some(n) = seconds(&j["timing_details"][k]) {
            row["details"][k] = json!(n);
        }
    }
    Ok(row)
}
fn summarize(rows: &[Value]) -> Value {
    let mut result = json!({});
    for stage in STAGES {
        let mut values: Vec<f64> = rows
            .iter()
            .filter(|r| r["status"] == "published")
            .filter_map(|r| seconds(&r["timings"][*stage]))
            .collect();
        values.sort_by(f64::total_cmp);
        if !values.is_empty() {
            let percentile =
                |p: f64| values[((values.len() as f64 * p).ceil() as usize).saturating_sub(1)];
            result[*stage] =
                json!({"count":values.len(),"p50":percentile(0.5),"p95":percentile(0.95)});
        }
    }
    result
}
pub fn collect(data: &Path) -> Value {
    let at = now();
    let result = (|| -> Result<Value> {
        let root = data.join("publisher/jobs");
        ensure!(fs::symlink_metadata(&root)?.is_dir(), "unsafe job root");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut candidates = Vec::new();
        for (i, entry) in fs::read_dir(&root)?.enumerate() {
            ensure!(
                i < 5000 && Instant::now() < deadline,
                "publication observation budget"
            );
            let entry = entry?;
            let meta = entry.metadata()?;
            ensure!(meta.is_file(), "unsafe job entry");
            candidates.push((meta.modified()?, entry.path()));
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let mut rows = Vec::new();
        let mut rejected = 0;
        for (_, path) in candidates.iter().take(LIMIT) {
            ensure!(Instant::now() < deadline, "publication observation budget");
            match crate::publisher::read_job(path).and_then(|j| clean(&j)) {
                Ok(row) => rows.push(row),
                Err(_) => rejected += 1,
            }
        }
        rows.sort_by(|a, b| {
            seconds(&b["created"])
                .unwrap_or(0.)
                .total_cmp(&seconds(&a["created"]).unwrap_or(0.))
        });
        Ok(
            json!({"status":if rejected==0{"ok"}else{"partial"},"measured_at":at,"limit":LIMIT,"sample_count":rows.len(),"rejected":rejected,"scope":"latest-active-records","summary":summarize(&rows),"jobs":rows}),
        )
    })();
    result.unwrap_or(json!({"status":"unavailable","measured_at":at,"jobs":[],"summary":{}}))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timings_are_allowlisted_and_percentiles_exclude_failures() {
        let a=clean(&json!({"key":"assets","sha":"a".repeat(40),"status":"published","created":1,"timings":{"total":2,"fetch":-1,"secret":10},"queue_seconds":0.1,"delivery":"secret","timing_details":{"source_check":1,"path":"secret"}})).unwrap();
        assert!(a.get("delivery").is_none());
        assert!(a["timings"].get("fetch").is_none());
        assert!(a["timings"].get("secret").is_none());
        assert!(a["details"].get("path").is_none());
        let b = json!({"status":"failed","timings":{"total":999}});
        let s = summarize(&[a, b]);
        assert_eq!(s["total"]["count"], 1);
        assert_eq!(s["total"]["p95"], 2.0);
    }
    #[test]
    fn unsafe_job_tree_is_unavailable_without_leaking_path() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("publisher/jobs")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("publisher/jobs/secret.json"))
            .unwrap();
        let v = collect(d.path());
        assert_eq!(v["status"], "unavailable");
        assert!(!v.to_string().contains("passwd"));
    }
}
