use super::*;
use fs2::FileExt;
use std::time::Instant;
mod local;
mod object_cleanup;
mod restore;
pub use restore::{barrier, reference_fingerprint, source_evidence};
mod audit;
pub use audit::{plan as audit_plan, run as audit_run_unlocked, scan as audit_scan};
mod dedup;
pub fn state(c: &Value) -> Result<PathBuf> {
    Ok(PathBuf::from(text(c, "state_root")?))
}
pub fn image_base(c: &Value, key: &str) -> Result<PathBuf> {
    ensure!(valid_key(key), "invalid key");
    Ok(PathBuf::from(text(c, "images_root")?)
        .join("repos")
        .join(key))
}
pub fn lock(c: &Value, name: &str) -> Result<File> {
    ensure!(
        matches!(name, "worker" | "queue" | "retention" | "object-retention"),
        "invalid lock name"
    );
    let root = state(c)?;
    fs::create_dir_all(&root)?;
    real_dir(&root)?;
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let f = options.open(root.join(format!("{name}.lock")))?;
    ensure!(f.metadata()?.is_file(), "unsafe lock");
    f.try_lock_exclusive()?;
    Ok(f)
}
pub fn jobs(c: &Value) -> Result<Vec<(PathBuf, Value)>> {
    let root = state(c)?.join("jobs");
    if !present(&root)? {
        return Ok(Vec::new());
    }
    real_dir(&root)?;
    let mut out = Vec::new();
    for e in fs::read_dir(root)? {
        ensure!(out.len() < 10000, "active job limit");
        let p = e?.path();
        ensure!(
            p.extension().and_then(|s| s.to_str()) == Some("json"),
            "unknown active task entry"
        );
        let v = crate::publisher::read_job(&p)?;
        ensure!(
            v["created"]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0. && n <= crate::common::now() + 86400.),
            "invalid job time"
        );
        out.push((p, v));
    }
    Ok(out)
}
pub fn idle(c: &Value) -> Result<()> {
    ensure!(
        !jobs(c)?.iter().any(|(_, v)| matches!(
            v["status"].as_str(),
            Some("pending" | "running" | "images_prepared")
        )),
        "wait for pending publications"
    );
    Ok(())
}
pub fn activation_sha(target: &Path) -> Result<String> {
    let s = target
        .to_str()
        .ok_or_else(|| anyhow!("invalid activation target"))?;
    let t = s
        .strip_prefix("releases/")
        .ok_or_else(|| anyhow!("invalid activation target"))?;
    let sha = if let Some(s) = t.strip_prefix("images-") {
        s
    } else {
        t.strip_suffix("/images")
            .ok_or_else(|| anyhow!("invalid activation target"))?
    };
    ensure!(valid_sha(sha), "invalid activation SHA");
    Ok(sha.to_owned())
}
pub fn current_sha(base: &Path) -> Result<Option<String>> {
    let p = base.join("current");
    match fs::symlink_metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(m) => {
            ensure!(m.file_type().is_symlink(), "current must be a symlink");
            let t = fs::read_link(&p)?;
            let sha = activation_sha(&t)?;
            real_dir(&base.join(&t))?;
            Ok(Some(sha))
        }
    }
}
pub fn history(c: &Value, key: &str) -> Result<Vec<String>> {
    let p = state(c)?
        .join("rollback-protection")
        .join(format!("{key}.json"));
    if !present(&p)? {
        return Ok(Vec::new());
    }
    let v = read_json(&p)?;
    ensure!(v["key"] == key, "invalid rollback identity");
    let a = v["shas"]
        .as_array()
        .ok_or_else(|| anyhow!("invalid rollback history"))?;
    ensure!(a.len() <= 100, "history limit");
    a.iter()
        .map(|s| {
            let s = s.as_str().ok_or_else(|| anyhow!("invalid history SHA"))?;
            ensure!(valid_sha(s), "invalid history SHA");
            Ok(s.to_owned())
        })
        .collect()
}
pub type ProtectedReferences = (Option<String>, BTreeMap<String, Vec<String>>);
pub fn protect(
    c: &Value,
    key: &str,
    jobs: &[(PathBuf, Value)],
    pins: &[String],
) -> Result<ProtectedReferences> {
    let base = image_base(c, key)?;
    for p in [base.parent().unwrap(), &base] {
        if present(p)? {
            real_dir(p)?;
        }
    }
    let current = current_sha(&base)?;
    let p = state(c)?.join("published").join(key);
    if present(&p)? {
        let v = read_json(&p)?;
        ensure!(
            v["key"] == key && v["sha"].as_str() == current.as_deref(),
            "published and current disagree"
        );
    } else {
        ensure!(current.is_none(), "current has no published manifest");
    }
    let mut out = BTreeMap::<String, Vec<String>>::new();
    let mut add = |sha: String, reason: &str| {
        out.entry(sha).or_default().push(reason.to_owned());
    };
    if let Some(s) = &current {
        add(s.clone(), "current");
    }
    for s in history(c, key)? {
        add(s, "rollback");
    }
    for s in pins {
        ensure!(valid_sha(s), "invalid pin");
        add(s.clone(), "pinned");
    }
    for (_, j) in jobs {
        if j["key"] != key {
            continue;
        }
        let active = matches!(
            j["status"].as_str(),
            Some("pending" | "running" | "images_prepared" | "failed")
        );
        if active {
            add(
                text(j, "sha")?.to_owned(),
                &format!("job-{}", text(j, "status")?),
            );
        }
        if (active || j["sha"].as_str() == current.as_deref()) && j["previous"].is_string() {
            add(
                activation_sha(Path::new(text(j, "previous")?))?,
                "previous-job",
            );
        }
    }
    Ok((current, out))
}
pub fn policy(c: &Value, r: &Value, objects: bool) -> Result<Value> {
    let defaults = if objects {
        json!({"enabled":false,"exclusive_writer":false,"keep_last":10,"keep_days":90,"min_age_hours":24,"pinned_shas":[],"max_delete_versions":2,"max_delete_objects":100,"max_delete_bytes":268435456u64})
    } else {
        json!({"enabled":false,"keep_last":5,"keep_days":30,"min_age_hours":24,"rollback_versions":2,"pinned_shas":[],"archive_jobs_after_days":30,"max_delete_versions":10,"max_delete_bytes":268435456u64,"max_archive_jobs":100})
    };
    let mut out = defaults.clone();
    let name = if objects {
        "object_retention"
    } else {
        "retention"
    };
    for source in [c.get(name), r.get(name)].into_iter().flatten() {
        let m = source
            .as_object()
            .ok_or_else(|| anyhow!("invalid retention policy"))?;
        for (k, v) in m {
            ensure!(defaults.get(k).is_some(), "unknown retention option");
            out[k] = v.clone();
        }
    }
    for (k, v) in out.as_object().unwrap() {
        if k == "enabled" || k == "exclusive_writer" {
            ensure!(v.is_boolean(), "invalid retention flag");
        } else if k == "pinned_shas" {
            let pins = v.as_array().ok_or_else(|| anyhow!("invalid pins"))?;
            ensure!(
                pins.len() <= 1000 && pins.iter().all(|s| s.as_str().is_some_and(valid_sha)),
                "invalid pins"
            );
        } else {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("invalid retention limit"))?;
            let (low, high) = match k.as_str() {
                "keep_last" => (1, 10000),
                "keep_days" => (0, 36500),
                "min_age_hours" => (1, 876000),
                "rollback_versions" => (1, 100),
                "archive_jobs_after_days" => (1, 36500),
                "max_delete_versions" => (1, if objects { 10 } else { 100 }),
                "max_delete_objects" => (1, 1000),
                "max_delete_bytes" => (1, 10737418240),
                "max_archive_jobs" => (1, 1000),
                _ => unreachable!(),
            };
            ensure!((low..=high).contains(&n), "invalid retention limit");
        }
    }
    Ok(out)
}
pub fn pins(v: &Value) -> Vec<String> {
    v["pinned_shas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}
pub fn tree_stats(root: &Path, deadline: Instant) -> Result<(u64, f64)> {
    real_dir(root)?;
    let mut size = 0;
    let mut newest = mtime(root)?;
    let mut count = 0;
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        ensure!(Instant::now() < deadline, "maintenance timeout");
        for e in fs::read_dir(dir)? {
            count += 1;
            ensure!(count <= 1000000, "tree entry budget");
            let p = e?.path();
            let m = fs::symlink_metadata(&p)?;
            newest = newest.max(mtime(&p)?);
            if m.is_dir() {
                stack.push(p);
            } else {
                ensure!(m.is_file(), "unsafe maintenance tree entry");
                size += m.len();
            }
        }
    }
    Ok((size, newest))
}
pub fn mtime(path: &Path) -> Result<f64> {
    Ok(fs::symlink_metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64())
}
pub fn write_once(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("invalid output"))?;
    fs::create_dir_all(parent)?;
    real_dir(parent)?;
    let mut f = tempfile::NamedTempFile::new_in(parent)?;
    f.write_all(bytes)?;
    f.as_file().sync_all()?;
    fs::hard_link(f.path(), path)?;
    sync_dir(parent)?;
    Ok(())
}
pub fn scoped_digest(c: &Value) -> Result<String> {
    Ok(hash(&crate::common::python_json(
        &json!([c["endpoint"], c["bucket"]]),
        false,
    )?))
}
pub fn selected<'a>(c: &'a Value, key: Option<&str>) -> Result<Vec<&'a Value>> {
    let r = c["repositories"]
        .as_array()
        .ok_or_else(|| anyhow!("missing repositories"))?
        .iter()
        .filter(|r| key.is_none_or(|k| r["key"] == k))
        .collect::<Vec<_>>();
    ensure!(!r.is_empty(), "unknown repository");
    Ok(r)
}
pub fn argument<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|v| v == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}
pub fn run(command: &str, args: &[String], c: &Value) -> Result<Value> {
    let apply = args.iter().any(|s| s == "--apply");
    let key = argument(args, "--key");
    let lock_result = lock(c, "worker");
    let _worker = match lock_result {
        Ok(f) => f,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            if command == "object-retention" {
                object_cleanup::observe_busy(c, key, apply)?;
            }
            let mut result = json!({"status":"busy","apply":apply,"repositories":[]});
            if let Some(key) = args.first().filter(|s| !s.starts_with('-')) {
                result["key"] = json!(key);
            }
            if let Some(sha) = args.get(1).filter(|s| valid_sha(s)) {
                result["sha"] = json!(sha);
            }
            return Ok(result);
        }
        Err(e) => return Err(e),
    };
    let result = match command {
        "retention" => local::run(c, key, apply),
        "object-retention" => object_cleanup::run(c, key, apply),
        "compact-objects" => dedup::compact(c, apply),
        "deduplicate-local" => dedup::local(c, apply),
        "materialize-image-views" => dedup::legacy(c, apply),
        "restore-objects" | "check-restore" | "close-restore" => {
            let key = args
                .first()
                .ok_or_else(|| anyhow!("repository key required"))?;
            let sha = args.get(1).ok_or_else(|| anyhow!("SHA required"))?;
            ensure!(
                valid_key(key) && valid_sha(sha),
                "invalid restoration identity"
            );
            crate::common::repo(c, key)?;
            match command {
                "restore-objects" => restore::run(
                    c,
                    key,
                    sha,
                    args.iter().any(|s| s == "--with-local"),
                    args.iter().any(|s| s == "--rollback"),
                ),
                "check-restore" => restore::check(c, key, sha),
                _ => audit::close(
                    c,
                    key,
                    sha,
                    argument(args, "--reason").ok_or_else(|| anyhow!("closure reason required"))?,
                ),
            }
        }
        s if s.starts_with("restore-audit-") => audit::run(c, command, args),
        _ => bail!("unknown storage command"),
    };
    match result {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            if command == "object-retention" {
                object_cleanup::observe_busy(c, key, apply)?;
            }
            let mut result = json!({"status":"busy","apply":apply,"repositories":[]});
            if let Some(key) = args.first().filter(|s| !s.starts_with('-')) {
                result["key"] = json!(key);
            }
            if let Some(sha) = args.get(1).filter(|s| valid_sha(s)) {
                result["sha"] = json!(sha);
            }
            Ok(result)
        }
        other => other,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_flag_types_and_budget() {
        let c = json!({});
        let r = json!({"retention":{"enabled":"false"}});
        assert!(policy(&c, &r, false).is_err());
        assert!(policy(
            &c,
            &json!({"object_retention":{"max_delete_objects":1001}}),
            true
        )
        .is_err());
    }
    #[test]
    fn activation_rejects_escape() {
        assert!(activation_sha(Path::new("releases/../images")).is_err());
        assert!(activation_sha(Path::new(&format!("releases/{}/images", "a".repeat(40)))).is_ok());
    }
}
