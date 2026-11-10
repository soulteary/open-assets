use super::*;
fn inspect_claim(folder: &Path, key: &str, deadline: Instant) -> Result<Option<u64>> {
    let name = folder.file_name().and_then(|s| s.to_str()).unwrap_or("");
    ensure!(
        name.len() == 32
            && name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "unknown cleanup claim"
    );
    real_dir(folder)?;
    let entries = fs::read_dir(folder)?.collect::<std::io::Result<Vec<_>>>()?;
    if entries.is_empty() {
        return Ok(None);
    }
    let receipt = read_json(&folder.join("receipt.json"))?;
    ensure!(
        receipt["schema_version"] == 1
            && receipt["key"] == key
            && receipt["sha"].as_str().is_some_and(valid_sha),
        "invalid cleanup receipt"
    );
    let mut size = 0;
    for e in entries {
        let name = e.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("invalid claim entry"))?;
        ensure!(
            matches!(name, "receipt.json" | "full" | "images"),
            "unknown claimed content"
        );
        if name != "receipt.json" {
            size += tree_stats(&e.path(), deadline)?.0;
        }
    }
    Ok(Some(size))
}
pub fn run(c: &Value, key: Option<&str>, apply: bool) -> Result<Value> {
    let _retention = lock(c, "retention")?;
    let _queue = lock(c, "queue")?;
    let jobs = jobs(c)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let now = crate::common::now();
    let mut plans = Vec::new();
    for r in selected(c, key)? {
        let key = text(r, "key")?;
        let settings = policy(c, r, false)?;
        let base = image_base(c, key)?;
        let releases = base.join("releases");
        let mut row = json!({"key":key,"enabled":settings["enabled"],"protected":{},"candidates":[],"archive_jobs":[],"local_bytes":0,"scope":"local-only"});
        if !present(&base)? {
            plans.push(row);
            continue;
        }
        real_dir(&base)?;
        let trash = base.join(".retention-trash");
        if present(&trash)? {
            real_dir(&trash)?;
            for e in fs::read_dir(&trash)? {
                inspect_claim(&e?.path(), key, deadline)?;
            }
        }
        let (current, mut protected) = protect(c, key, &jobs, &pins(&settings))?;
        let mut versions = BTreeMap::<String, Value>::new();
        if present(&releases)? {
            real_dir(&releases)?;
            let mut trees = Vec::new();
            for e in fs::read_dir(&releases)? {
                ensure!(trees.len() < 20000, "release directory budget");
                let p = e?.path();
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                let sha = name.strip_prefix("images-").unwrap_or(name);
                if valid_sha(sha) {
                    trees.push((sha.to_owned(), p));
                }
            }
            // Two readers overlap metadata latency; every entry is still checked.
            // Both readers must finish before planning can authorize any deletion.
            let scanned = std::thread::scope(|scope| -> Result<Vec<(String, u64, f64)>> {
                let handles: Vec<_> = (0..2)
                    .map(|worker| {
                        let trees = &trees;
                        scope.spawn(move || -> Result<Vec<(String, u64, f64)>> {
                            trees
                                .iter()
                                .skip(worker)
                                .step_by(2)
                                .map(|(sha, path)| {
                                    let (size, newest) = tree_stats(path, deadline)?;
                                    Ok((sha.clone(), size, newest))
                                })
                                .collect()
                        })
                    })
                    .collect();
                let mut out = Vec::new();
                let mut failure = None;
                for handle in handles {
                    match handle.join() {
                        Ok(Ok(rows)) => out.extend(rows),
                        Ok(Err(e)) => {
                            failure.get_or_insert(e);
                        }
                        Err(_) => {
                            failure.get_or_insert(anyhow!("retention scanner failed"));
                        }
                    }
                }
                if let Some(e) = failure {
                    return Err(e);
                }
                Ok(out)
            })?;
            for (sha, size, newest) in scanned {
                let v = versions
                    .entry(sha.clone())
                    .or_insert_with(|| json!({"sha":sha,"bytes":0,"newest":0.}));
                v["bytes"] = json!(v["bytes"]
                    .as_u64()
                    .unwrap()
                    .checked_add(size)
                    .ok_or_else(|| anyhow!("tree size overflow"))?);
                v["newest"] = json!(v["newest"].as_f64().unwrap().max(newest));
            }
        }
        for (sha, v) in &mut versions {
            if current.is_none() {
                protected
                    .entry(sha.clone())
                    .or_default()
                    .push("no-current-reference".into());
            }
            let manifest = state(c)?
                .join("manifests")
                .join(key)
                .join(format!("{sha}.json"));
            if !present(&manifest)? {
                protected
                    .entry(sha.clone())
                    .or_default()
                    .push("missing-manifest".into());
                continue;
            }
            let m = read_json(&manifest)?;
            ensure!(
                m["key"] == key && m["sha"] == *sha,
                "manifest identity mismatch"
            );
            validate_manifest(&m)?;
            v["newest"] = json!(v["newest"].as_f64().unwrap().max(mtime(&manifest)?));
        }
        for (_, j) in &jobs {
            if j["key"] == key {
                if let Some(v) = versions.get_mut(text(j, "sha")?) {
                    let stamp = j["created"]
                        .as_f64()
                        .unwrap()
                        .max(j["finished"].as_f64().unwrap_or(0.));
                    ensure!(stamp <= now + 86400., "invalid job timestamp");
                    v["newest"] = json!(v["newest"].as_f64().unwrap().max(stamp));
                }
            }
        }
        let mut ordered = versions.values().cloned().collect::<Vec<_>>();
        ordered.sort_by(|a, b| {
            b["newest"]
                .as_f64()
                .unwrap()
                .total_cmp(&a["newest"].as_f64().unwrap())
                .then_with(|| b["sha"].as_str().cmp(&a["sha"].as_str()))
        });
        for v in ordered
            .iter()
            .take(settings["keep_last"].as_u64().unwrap() as usize)
        {
            protected
                .entry(text(v, "sha")?.to_owned())
                .or_default()
                .push("keep-last".into());
        }
        let threshold = (settings["keep_days"].as_u64().unwrap() * 86400)
            .max(settings["min_age_hours"].as_u64().unwrap() * 3600) as f64;
        for v in &ordered {
            if now - v["newest"].as_f64().unwrap() < threshold {
                protected
                    .entry(text(v, "sha")?.to_owned())
                    .or_default()
                    .push("recent".into());
            }
        }
        let mut candidates = Vec::new();
        let mut bytes = 0;
        for v in ordered.iter().rev() {
            if protected.contains_key(text(v, "sha")?) {
                continue;
            }
            let size = v["bytes"].as_u64().unwrap();
            if candidates.len() >= settings["max_delete_versions"].as_u64().unwrap() as usize
                || bytes + size > settings["max_delete_bytes"].as_u64().unwrap()
            {
                continue;
            }
            bytes += size;
            candidates.push(v.clone());
        }
        let mut archive = Vec::new();
        for (p, j) in &jobs {
            if j["key"] == key
                && matches!(j["status"].as_str(), Some("published" | "superseded"))
                && !protected.contains_key(text(j, "sha")?)
                && now
                    - j["created"]
                        .as_f64()
                        .unwrap()
                        .max(j["finished"].as_f64().unwrap_or(0.))
                        .max(mtime(p)?)
                    >= settings["archive_jobs_after_days"].as_u64().unwrap() as f64 * 86400.
            {
                let id = text(j, "id")?;
                let target = state(c)?
                    .join("job-archive")
                    .join(&id[..2])
                    .join(format!("{id}.json"));
                ensure!(!present(&target)?, "duplicate archived task");
                archive.push(id.to_owned());
            }
        }
        archive.sort();
        archive.truncate(settings["max_archive_jobs"].as_u64().unwrap() as usize);
        row["protected"] = serde_json::to_value(&protected)?;
        row["candidates"] = json!(candidates);
        row["archive_jobs"] = json!(archive);
        row["local_bytes"] = json!(ordered
            .iter()
            .map(|v| v["bytes"].as_u64().unwrap())
            .sum::<u64>());
        plans.push(row);
    }
    // Complete all repository plans before claiming any live directory.
    if apply {
        for row in &mut plans {
            if row["enabled"] != true {
                continue;
            }
            ensure!(Instant::now() < deadline, "cleanup planning timeout");
            let key_owned = text(row, "key")?.to_owned();
            let key = key_owned.as_str();
            let base = image_base(c, key)?;
            let trash = base.join(".retention-trash");
            let releases = base.join("releases");
            for entry in row["candidates"].as_array().unwrap() {
                let sha = text(entry, "sha")?;
                fs::create_dir_all(&trash)?;
                real_dir(&trash)?;
                let claim = trash.join(uuid::Uuid::new_v4().simple().to_string());
                fs::create_dir(&claim)?;
                crate::common::atomic(
                    &claim.join("receipt.json"),
                    &json!({"schema_version":1,"key":key,"sha":sha}),
                )?;
                sync_dir(&trash)?;
                for (prefix, target) in [("", "full"), ("images-", "images")] {
                    let source = releases.join(format!("{prefix}{sha}"));
                    if present(&source)? {
                        real_dir(&source)?;
                        fs::rename(source, claim.join(target))?;
                    }
                }
                sync_dir(&claim)?;
                sync_dir(&releases)?;
            }
            for id in row["archive_jobs"].as_array().unwrap() {
                let id = id.as_str().unwrap();
                let source = state(c)?.join("jobs").join(format!("{id}.json"));
                let target = state(c)?
                    .join("job-archive")
                    .join(&id[..2])
                    .join(format!("{id}.json"));
                fs::create_dir_all(target.parent().unwrap())?;
                real_dir(target.parent().unwrap())?;
                ensure!(!present(&target)?, "duplicate archived task");
                fs::rename(&source, &target)?;
                sync_dir(target.parent().unwrap())?;
                sync_dir(source.parent().unwrap())?;
            }
            let r = c["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["key"] == key)
                .unwrap();
            let settings = policy(c, r, false)?;
            let mut removed = 0u64;
            let mut bytes = 0u64;
            if present(&trash)? {
                for e in fs::read_dir(&trash)? {
                    let folder = e?.path();
                    let size =
                        inspect_claim(&folder, key, Instant::now() + Duration::from_secs(30))?;
                    if let Some(size) = size {
                        if removed >= settings["max_delete_versions"].as_u64().unwrap()
                            || bytes + size > settings["max_delete_bytes"].as_u64().unwrap()
                        {
                            continue;
                        }
                        for name in ["full", "images"] {
                            let p = folder.join(name);
                            if present(&p)? {
                                real_dir(&p)?;
                                fs::remove_dir_all(&p)?;
                            }
                        }
                        fs::remove_file(folder.join("receipt.json"))?;
                        bytes += size;
                        removed += 1;
                    }
                    fs::remove_dir(&folder)?;
                    sync_dir(&trash)?;
                }
            }
            row["trash_removed"] = json!(removed);
            let record = json!({"schema_version":1,"key":key,"status":"complete","apply":true,"completed_at":crate::common::now(),"claimed_versions":row["candidates"].as_array().unwrap().len(),"removed_directories":removed,"archived_jobs":row["archive_jobs"].as_array().unwrap().len()});
            crate::common::atomic(
                &state(c)?
                    .join("retention-results")
                    .join(format!("{key}.json")),
                &record,
            )?;
        }
    }
    let report = json!({"status":"complete","apply":apply,"repositories":plans,"completed_at":crate::common::now()});
    if apply {
        crate::common::atomic(&state(c)?.join("retention-last.json"), &report)?;
    }
    Ok(report)
}
