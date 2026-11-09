use super::*;
struct Budget {
    deadline: Instant,
    requests: usize,
    objects: usize,
    next: Instant,
}
impl Budget {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(45),
            requests: 0,
            objects: 0,
            next: Instant::now(),
        }
    }
    fn tick(&mut self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline && self.requests < 500,
            "object cleanup budget exhausted"
        );
        if self.next > Instant::now() {
            std::thread::sleep(self.next - Instant::now());
        }
        ensure!(Instant::now() < self.deadline, "object cleanup timeout");
        self.requests += 1;
        self.next = Instant::now() + Duration::from_millis(100);
        Ok(())
    }
}
fn listing(client: &Client, prefix: &str, b: &mut Budget) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut token = None;
    loop {
        b.tick()?;
        let page = client.list(prefix, token.as_deref(), 1000)?;
        for item in page["Contents"].as_array().unwrap() {
            b.objects += 1;
            ensure!(b.objects <= 50000, "object cleanup listing limit");
            let name = text(item, "Key")?;
            let object = json!({"size":item["Size"],"etag":item["ETag"],"modified":item.get("LastModifiedPrecise").unwrap_or(&item["LastModified"])});
            ensure!(
                out.insert(name.to_owned(), object).is_none(),
                "duplicate listed object"
            );
        }
        if page["IsTruncated"] == false {
            break;
        }
        let t = text(&page, "NextContinuationToken")?.to_owned();
        ensure!(seen.insert(t.clone()), "repeated cleanup token");
        token = Some(t);
    }
    Ok(out)
}
pub(super) fn manifest_owned(
    c: &Value,
    key: &str,
    sha: &str,
) -> Result<(Value, BTreeMap<String, Value>, String)> {
    let path = state(c)?
        .join("manifests")
        .join(key)
        .join(format!("{sha}.json"));
    real_dir(path.parent().unwrap())?;
    let m = read_json(&path)?;
    ensure!(
        m["key"] == key && m["sha"] == sha,
        "cleanup manifest identity mismatch"
    );
    let sources = validate_manifest(&m)?;
    let prefix = format!("repositories/{key}/releases/{sha}/");
    let mut files = BTreeMap::new();
    for item in m["files"].as_array().unwrap() {
        let src = &sources[text(item, "path")?];
        let name = text(src, "key")?;
        if name.starts_with(&prefix) {
            files.insert(
                name.to_owned(),
                json!({"size":item["size"],"sha256":item["sha256"]}),
            );
        }
    }
    let identity = if m["object_layout"] == "references-v1" {
        json!({"objects":files,"sources":sources})
    } else {
        json!(files)
    };
    let digest = crate::common::digest(&identity)?;
    Ok((m, files, digest))
}
fn receipt(c: &Value, key: &str, sha: &str, digest: &str) -> Result<Option<Value>> {
    let p = state(c)?
        .join("object-retention")
        .join(key)
        .join(format!("{sha}.json"));
    if !present(&p)? {
        return Ok(None);
    }
    let v = read_json(&p)?;
    if matches!(v["status"].as_str(), Some("complete" | "restored")) {
        return Ok(None);
    }
    ensure!(
        v["schema_version"] == 1
            && v["key"] == key
            && v["sha"] == sha
            && v["manifest_digest"] == digest
            && v["bucket_scope"] == scoped_digest(c)?
            && v["status"] == "deleting"
            && v["objects"].is_object(),
        "invalid cleanup receipt"
    );
    Ok(Some(v))
}
fn execute(c: &Value, key: Option<&str>, apply: bool, provided: Option<Client>) -> Result<Value> {
    let _cleanup = lock(c, "object-retention")?;
    let _queue = lock(c, "queue")?;
    let repos = selected(c, key)?;
    for r in &repos {
        let p = policy(c, r, true)?;
        ensure!(
            !(apply && p["enabled"] == true && p["exclusive_writer"] != true),
            "object cleanup requires explicit exclusive_writer=true"
        );
    }
    let client = match provided {
        Some(client) => client,
        None => Client::from_config(c)?,
    };
    let mut budget = Budget::new();
    budget.tick()?;
    ensure!(
        client.versioning()?.is_none(),
        "versioned or suspended bucket unsupported"
    );
    let jobs = jobs(c)?;
    let now = crate::common::now();
    let mut plans = Vec::new();
    for r in repos {
        let key = text(r, "key")?;
        let settings = policy(c, r, true)?;
        let mut pins = pins(&settings);
        pins.extend(super::pins(&policy(c, r, false)?));
        let (current, mut protected) = protect(c, key, &jobs, &pins)?;
        let rootprefix = format!("repositories/{key}/releases/");
        let objects = listing(&client, &rootprefix, &mut budget)?;
        let mut versions = BTreeMap::<String, Value>::new();
        let mut unknown = 0;
        for (name, obj) in objects {
            match parts(key, &name) {
                Ok((sha, _)) => {
                    let v = versions
                        .entry(sha.to_owned())
                        .or_insert_with(|| json!({"sha":sha,"objects":{},"newest":0.}));
                    v["objects"][&name] = obj.clone();
                    v["newest"] = json!(v["newest"]
                        .as_f64()
                        .unwrap()
                        .max(obj["modified"].as_f64().unwrap()));
                }
                Err(_) => unknown += 1,
            }
        }
        let folder = state(c)?.join("manifests").join(key);
        if present(&folder)? {
            real_dir(&folder)?;
            let paths = fs::read_dir(&folder)?.collect::<std::io::Result<Vec<_>>>()?;
            ensure!(paths.len() <= 10000, "manifest count budget");
            for e in paths {
                ensure!(Instant::now() < budget.deadline, "cleanup planning timeout");
                let p = e.path();
                let sha = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                ensure!(
                    p.extension().and_then(|s| s.to_str()) == Some("json") && valid_sha(sha),
                    "unknown reference manifest"
                );
                let m = read_json(&p)?;
                ensure!(
                    m["key"] == key && m["sha"] == sha,
                    "manifest identity mismatch"
                );
                let marker = state(c)?
                    .join("object-retention")
                    .join(key)
                    .join(format!("{sha}.json"));
                if present(&marker)? {
                    let rec = read_json(&marker)?;
                    ensure!(
                        rec["key"] == key && rec["sha"] == sha,
                        "receipt identity mismatch"
                    );
                    if rec["status"] == "complete" {
                        continue;
                    }
                }
                if m["object_layout"] == "references-v1" {
                    versions.entry(sha.to_owned()).or_insert_with(
                        || json!({"sha":sha,"objects":{},"newest":mtime(&p).unwrap_or(0.)}),
                    );
                    for src in validate_manifest(&m)?.values() {
                        let (origin, _) = parts(key, text(src, "key")?)?;
                        if origin != sha {
                            protected
                                .entry(origin.to_owned())
                                .or_default()
                                .push("object-reference".into());
                        }
                    }
                }
            }
        }
        let receipt_dir = state(c)?.join("object-retention").join(key);
        if present(&receipt_dir)? {
            real_dir(&receipt_dir)?;
            for (n, e) in fs::read_dir(&receipt_dir)?.enumerate() {
                ensure!(n < 10000, "receipt count budget");
                let p = e?.path();
                let sha = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                ensure!(
                    p.extension().and_then(|s| s.to_str()) == Some("json") && valid_sha(sha),
                    "unknown deletion receipt"
                );
                let v = read_json(&p)?;
                if v["status"] == "deleting" {
                    versions
                        .entry(sha.to_owned())
                        .or_insert_with(|| json!({"sha":sha,"objects":{},"newest":0.}));
                }
            }
        }
        for (sha, v) in &mut versions {
            match manifest_owned(c, key, sha) {
                Ok((_m, files, digest)) => {
                    v["files"] = json!(files);
                    v["manifest_digest"] = json!(digest);
                    let observed = v["objects"].as_object().unwrap();
                    let expected = files.keys().cloned().collect::<BTreeSet<_>>();
                    if let Some(previous) = receipt(c, key, sha, &digest)? {
                        let saved = previous["objects"].as_object().unwrap();
                        ensure!(
                            saved.keys().cloned().collect::<BTreeSet<_>>() == expected
                                && observed.iter().all(|(k, v)| saved.get(k) == Some(v)),
                            "receipt conflicts with remaining objects"
                        );
                    } else if observed.keys().cloned().collect::<BTreeSet<_>>() != expected {
                        protected
                            .entry(sha.clone())
                            .or_default()
                            .push("incomplete-or-extra-objects".into());
                    }
                    if observed
                        .iter()
                        .any(|(k, v)| files.get(k).is_none_or(|f| f["size"] != v["size"]))
                    {
                        protected
                            .entry(sha.clone())
                            .or_default()
                            .push("manifest-size-mismatch".into());
                    }
                    let manifest = folder.join(format!("{sha}.json"));
                    let mut newest = v["newest"].as_f64().unwrap().max(mtime(&manifest)?);
                    let base = image_base(c, key)?;
                    for prefix in ["", "images-"] {
                        let p = base.join("releases").join(format!("{prefix}{sha}"));
                        if present(&p)? {
                            real_dir(&p)?;
                            newest = newest.max(mtime(&p)?);
                        }
                    }
                    for (_, j) in &jobs {
                        if j["key"] == key && j["sha"] == *sha {
                            newest = newest
                                .max(j["created"].as_f64().unwrap())
                                .max(j["finished"].as_f64().unwrap_or(0.));
                        }
                    }
                    v["newest"] = json!(newest);
                }
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    protected
                        .entry(sha.clone())
                        .or_default()
                        .push("missing-manifest".into());
                }
                Err(e) => return Err(e),
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
            let sha = text(v, "sha")?;
            if current.is_none() {
                protected
                    .entry(sha.to_owned())
                    .or_default()
                    .push("no-current-reference".into());
            }
            if now - v["newest"].as_f64().unwrap() < threshold {
                protected
                    .entry(sha.to_owned())
                    .or_default()
                    .push("recent".into());
            }
        }
        let mut count = 0u64;
        let mut bytes = 0u64;
        let mut candidates = Vec::new();
        let mut eligible_versions = 0;
        let mut eligible_objects = 0;
        let mut eligible_bytes = 0;
        let mut skips = 0;
        let mut skip_counts = BTreeMap::<String, u64>::new();
        for v in ordered.iter().rev() {
            if protected.contains_key(text(v, "sha")?) {
                continue;
            }
            eligible_versions += 1;
            let objs = v["objects"].as_object().unwrap();
            eligible_objects += objs.len();
            eligible_bytes += objs
                .values()
                .map(|o| o["size"].as_u64().unwrap())
                .sum::<u64>();
            let mut batch = BTreeMap::new();
            let mut b = 0;
            for (name, obj) in objs {
                let size = obj["size"].as_u64().unwrap();
                if count + batch.len() as u64 >= settings["max_delete_objects"].as_u64().unwrap() {
                    break;
                }
                if bytes + b + size <= settings["max_delete_bytes"].as_u64().unwrap() {
                    batch.insert(name.clone(), obj.clone());
                    b += size;
                }
            }
            if candidates.len() >= settings["max_delete_versions"].as_u64().unwrap() as usize
                || (!objs.is_empty() && batch.is_empty())
            {
                skips += 1;
                let reason = if candidates.len()
                    >= settings["max_delete_versions"].as_u64().unwrap() as usize
                {
                    "versions-limit"
                } else if count + objs.len() as u64
                    > settings["max_delete_objects"].as_u64().unwrap()
                {
                    "objects-limit"
                } else {
                    "bytes-limit"
                };
                *skip_counts.entry(reason.to_owned()).or_default() += 1;
                continue;
            }
            let mut v = v.clone();
            v["batch"] = json!(batch);
            count += batch.len() as u64;
            bytes += b;
            candidates.push(v);
        }
        plans.push(json!({"key":key,"enabled":settings["enabled"],"exclusive_writer":settings["exclusive_writer"],"protected":protected,"unknown_objects":unknown,"candidates":candidates,"eligible_versions":eligible_versions,"eligible_objects":eligible_objects,"eligible_bytes":eligible_bytes,"skipped_versions":skips,"skip_counts":skip_counts}));
    }
    let mut rows = Vec::new();
    for mut plan in plans {
        let key = text(&plan, "key")?.to_owned();
        let candidates = plan["candidates"].as_array().unwrap().clone();
        plan["candidates"]=json!(candidates.iter().map(|v|json!({"sha":v["sha"],"objects":v["batch"].as_object().unwrap().len(),"bytes":v["batch"].as_object().unwrap().values().map(|o|o["size"].as_u64().unwrap()).sum::<u64>()})).collect::<Vec<_>>());
        plan["removed_objects"] = json!(0);
        plan["removed_bytes"] = json!(0);
        plan["removed_versions"] = json!(0);
        if apply && plan["enabled"] == true {
            for v in candidates {
                let result = remove(c, &client, &key, &v, &mut budget)?;
                plan["removed_objects"] = json!(
                    plan["removed_objects"].as_u64().unwrap() + result["objects"].as_u64().unwrap()
                );
                plan["removed_bytes"] = json!(
                    plan["removed_bytes"].as_u64().unwrap() + result["bytes"].as_u64().unwrap()
                );
                if result["status"] == "complete" {
                    plan["removed_versions"] =
                        json!(plan["removed_versions"].as_u64().unwrap() + 1);
                } else {
                    plan["skipped_versions"] =
                        json!(plan["skipped_versions"].as_u64().unwrap() + 1);
                    let reason = if result["status"] == "partial" {
                        "batch-remaining"
                    } else {
                        "request-budget"
                    };
                    plan["skip_counts"][reason] =
                        json!(plan["skip_counts"][reason].as_u64().unwrap_or(0) + 1);
                }
            }
        }
        rows.push(plan);
    }
    let report = json!({"status":"complete","apply":apply,"repositories":rows,"completed_at":crate::common::now()});
    crate::common::atomic(&state(c)?.join("object-retention-last.json"), &report)?;
    Ok(report)
}
fn remove(c: &Value, client: &Client, key: &str, v: &Value, b: &mut Budget) -> Result<Value> {
    let sha = text(v, "sha")?;
    let prefix = format!("repositories/{key}/releases/{sha}/");
    b.tick()?;
    ensure!(client.versioning()?.is_none(), "bucket versioning changed");
    let observed = listing(client, &prefix, b)?;
    ensure!(
        json!(observed) == v["objects"],
        "version changed before deletion"
    );
    let available = (500usize.saturating_sub(b.requests + 1) / 2).min(
        (b.deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            / 0.2)
            .max(0.) as usize,
    );
    let mut selected = BTreeMap::new();
    let mid = Instant::now() + b.deadline.saturating_duration_since(Instant::now()) / 2;
    for (name, obj) in v["batch"].as_object().unwrap().iter().take(available) {
        if !selected.is_empty() && Instant::now() >= mid {
            break;
        }
        b.tick()?;
        let head = client
            .head(name)?
            .ok_or_else(|| anyhow!("object disappeared"))?;
        ensure!(
            head["size"] == obj["size"]
                && head["etag"] == obj["etag"]
                && head["modified"].as_f64().unwrap_or(-1.)
                    == obj["modified"].as_f64().unwrap_or(-2.).floor()
                && head["metadata"]["sha256"] == v["files"][name]["sha256"],
            "delete object metadata mismatch"
        );
        selected.insert(name.clone(), obj.clone());
    }
    if !observed.is_empty() && selected.is_empty() {
        return Ok(json!({"status":"budget","objects":0,"bytes":0}));
    }
    let record=receipt(c,key,sha,text(v,"manifest_digest")?)?.unwrap_or_else(||json!({"schema_version":1,"key":key,"sha":sha,"status":"deleting","manifest_digest":v["manifest_digest"],"bucket_scope":scoped_digest(c).unwrap_or_default(),"objects":observed}));
    let path = state(c)?
        .join("object-retention")
        .join(key)
        .join(format!("{sha}.json"));
    crate::common::atomic(&path, &record)?;
    let mut bytes = 0;
    for (name, obj) in &selected {
        b.tick()?;
        client.delete_one(name, text(obj, "etag")?)?;
        bytes += obj["size"].as_u64().unwrap();
    }
    let remaining = listing(client, &prefix, b)?;
    let expected = observed
        .into_iter()
        .filter(|(k, _)| !selected.contains_key(k))
        .collect::<BTreeMap<_, _>>();
    ensure!(remaining == expected, "objects changed after deletion");
    let complete = remaining.is_empty();
    if complete {
        crate::common::atomic(
            &path,
            &json!({"schema_version":1,"key":key,"sha":sha,"status":"complete","completed_at":crate::common::now()}),
        )?;
    }
    Ok(
        json!({"status":if complete{"complete"}else{"partial"},"objects":selected.len(),"bytes":bytes}),
    )
}

fn observe(
    c: &Value,
    key: Option<&str>,
    apply: bool,
    started: f64,
    status: &str,
    report: Option<&Value>,
    failure: &str,
) -> Result<()> {
    for r in selected(c, key)? {
        let key = text(r, "key")?;
        let path = state(c)?
            .join("object-retention-results")
            .join(format!("{key}.json"));
        let mut previous = if present(&path)? {
            read_json(&path).unwrap_or_else(|_| json!({}))
        } else {
            json!({})
        };
        let mut output = json!({"schema_version":1,"key":key});
        for name in ["attempt", "last_plan", "last_preview", "last_apply"] {
            if previous[name].is_object() {
                output[name] = previous[name].take();
            }
        }
        let mode = if apply { "apply" } else { "preview" };
        let mut record = json!({"mode":mode,"started_at":started,"finished_at":crate::common::now(),"status":status,"reason":if status=="running"{"started"}else{failure},"partial_possible":apply&&matches!(status,"running"|"failed")});
        if status == "complete" {
            let report = report.ok_or_else(|| anyhow!("missing cleanup report"))?;
            let row = report["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["key"] == key)
                .ok_or_else(|| anyhow!("missing cleanup row"))?;
            let candidates = row["candidates"].as_array().unwrap();
            record["finished_at"] = report["completed_at"].clone();
            record["policy_digest"] = json!(crate::common::digest(&json!([
                c["endpoint"],
                c["bucket"],
                key,
                policy(c, r, true)?,
                policy(c, r, false)?
            ]))?);
            record["candidate_versions"] = json!(candidates.len());
            record["candidate_objects"] = json!(candidates
                .iter()
                .map(|v| v["objects"].as_u64().unwrap())
                .sum::<u64>());
            record["candidate_bytes"] = json!(candidates
                .iter()
                .map(|v| v["bytes"].as_u64().unwrap())
                .sum::<u64>());
            for field in [
                "eligible_versions",
                "eligible_objects",
                "eligible_bytes",
                "skipped_versions",
                "unknown_objects",
                "removed_versions",
                "removed_objects",
                "removed_bytes",
            ] {
                record[field] = row.get(field).cloned().unwrap_or(json!(0));
            }
            let mut counts = BTreeMap::<String, u64>::new();
            for reasons in row["protected"].as_object().unwrap().values() {
                let distinct = reasons
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<BTreeSet<_>>();
                for reason in distinct {
                    *counts.entry(reason.to_owned()).or_default() += 1;
                }
            }
            record["protection_counts"] = json!(counts);
            record["skip_counts"] = row["skip_counts"].clone();
            record["partial_possible"] = json!(false);
            record["reason"] = json!(if !apply {
                "preview-only"
            } else if row["enabled"] != true {
                "disabled"
            } else if row["skipped_versions"].as_u64().unwrap_or(0) > 0 {
                "budget-skipped"
            } else if row["removed_versions"].as_u64().unwrap_or(0) > 0 {
                "applied"
            } else {
                "no-candidates"
            });
            for field in [
                "last_plan",
                if apply { "last_apply" } else { "last_preview" },
            ] {
                output[field] = record.clone();
            }
        }
        output["attempt"] = record;
        crate::common::atomic(&path, &output)?;
    }
    Ok(())
}
pub fn observe_busy(c: &Value, key: Option<&str>, apply: bool) -> Result<()> {
    observe(c, key, apply, crate::common::now(), "busy", None, "busy")
}
pub fn run(c: &Value, key: Option<&str>, apply: bool) -> Result<Value> {
    let started = crate::common::now();
    observe(c, key, apply, started, "running", None, "started")?;
    match execute(c, key, apply, None) {
        Ok(report) => {
            observe(
                c,
                key,
                apply,
                started,
                "complete",
                Some(&report),
                "preview-only",
            )?;
            Ok(report)
        }
        Err(e) => {
            let message = e.to_string();
            let reason = if message.contains("exclusive_writer") {
                "exclusive-writer-required"
            } else if message.contains("version") && message.contains("bucket") {
                "versioned-bucket"
            } else if message.contains("budget") || message.contains("timeout") {
                "time-budget"
            } else if e.downcast_ref::<std::io::Error>().is_some() {
                "io-error"
            } else {
                "unsafe-state"
            };
            observe(c, key, apply, started, "failed", None, reason)?;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use tiny_http::{Header, Response as Reply, Server, StatusCode};
    fn execute_when_available(c: &Value, client: &Client) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match execute(c, None, true, Some(client.clone())) {
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => return result,
            }
        }
    }
    #[test]
    fn deletion_rechecks_metadata_and_resumes_one_object_batches_without_touching_pins() {
        let temp = tempfile::tempdir().unwrap();
        let state_root = temp.path().join("state");
        let images = temp.path().join("images");
        fs::create_dir_all(state_root.join("jobs")).unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr().to_ip().unwrap());
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let sha_c = "c".repeat(40);
        let c = json!({"state_root":state_root,"images_root":images,"endpoint":endpoint,"bucket":"attachments","repositories":[{"key":"images","enabled":true,"object_retention":{"enabled":true,"exclusive_writer":true,"keep_last":1,"keep_days":0,"min_age_hours":1,"pinned_shas":[sha_b],"max_delete_objects":1}}]});
        let objects = Arc::new(Mutex::new(BTreeSet::<String>::new()));
        let old = std::time::UNIX_EPOCH + Duration::from_secs(1700000000);
        let digest = hash(b"payload");
        for sha in [&sha_a, &sha_b, &sha_c] {
            let names = if sha == &sha_a {
                vec!["images/a.png", "images/b.png"]
            } else {
                vec!["images/a.png"]
            };
            let files = names
                .iter()
                .map(
                    |name| json!({"path":name,"size":7,"sha256":digest,"content_type":"image/png"}),
                )
                .collect::<Vec<_>>();
            for name in names {
                objects
                    .lock()
                    .unwrap()
                    .insert(format!("repositories/images/releases/{sha}/{name}"));
            }
            let m = json!({"key":"images","sha":sha,"files":files,"image_layout":"snapshot-v1"});
            let p = state_root
                .join("manifests/images")
                .join(format!("{sha}.json"));
            crate::common::atomic(&p, &m).unwrap();
            File::open(&p)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
            if sha == &sha_c {
                let base = image_base(&c, "images").unwrap();
                fs::create_dir_all(base.join("releases").join(sha).join("images")).unwrap();
                std::os::unix::fs::symlink(format!("releases/{sha}/images"), base.join("current"))
                    .unwrap();
                crate::common::atomic(&state_root.join("published/images"), &m).unwrap();
            }
        }
        let access = temp.path().join("access");
        let secret = temp.path().join("secret");
        fs::write(&access, "fixture").unwrap();
        fs::write(&secret, "secret").unwrap();
        let client = Client {
            http: http_client(None, 5).unwrap(),
            endpoint: Url::parse(&endpoint).unwrap(),
            bucket: "attachments".into(),
            access_path: access,
            secret_path: secret,
        };
        let bad_head = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let deletes = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_objects = Arc::clone(&objects);
        let server_bad = Arc::clone(&bad_head);
        let server_stop = Arc::clone(&stop);
        let server_deletes = Arc::clone(&deletes);
        let candidate = sha_a.clone();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && !server_stop.load(Ordering::SeqCst) {
                let Some(request) = server.recv_timeout(Duration::from_millis(50)).unwrap() else {
                    continue;
                };
                let url = Url::parse(&format!("http://localhost{}", request.url())).unwrap();
                let key = url
                    .path()
                    .strip_prefix("/attachments/")
                    .unwrap_or("")
                    .to_owned();
                let q = url.query_pairs().collect::<BTreeMap<_, _>>();
                let response = if q.contains_key("versioning") {
                    Reply::from_string("<VersioningConfiguration/>")
                } else if q.contains_key("list-type") {
                    let prefix = q.get("prefix").map(|v| v.as_ref()).unwrap_or("");
                    let mut body =
                        String::from("<ListBucketResult><IsTruncated>false</IsTruncated>");
                    for name in server_objects
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|n| n.starts_with(prefix))
                    {
                        body.push_str(&format!("<Contents><Key>{name}</Key><Size>7</Size><ETag>&quot;321c3cf486ed509164edec1e1981fec8&quot;</ETag><LastModified>2023-11-14T22:13:20Z</LastModified></Contents>"));
                    }
                    body.push_str("</ListBucketResult>");
                    Reply::from_string(body)
                } else if request.method() == &tiny_http::Method::Head {
                    let etag = if server_bad.load(Ordering::SeqCst) {
                        "\"changed\""
                    } else {
                        "\"321c3cf486ed509164edec1e1981fec8\""
                    };
                    let headers = vec![
                        Header::from_bytes("ETag", etag).unwrap(),
                        Header::from_bytes("Last-Modified", "Tue, 14 Nov 2023 22:13:20 GMT")
                            .unwrap(),
                        Header::from_bytes("Content-Type", "image/png").unwrap(),
                        Header::from_bytes("x-amz-meta-sha256", digest.as_str()).unwrap(),
                    ];
                    Reply::new(
                        StatusCode(200),
                        headers,
                        std::io::Cursor::new(Vec::<u8>::new()),
                        Some(7),
                        None,
                    )
                } else if request.method() == &tiny_http::Method::Delete {
                    assert!(key.starts_with(&format!("repositories/images/releases/{candidate}/")));
                    assert!(request.headers().iter().any(|h| h.field.equiv("If-Match")));
                    assert!(server_objects.lock().unwrap().remove(&key));
                    server_deletes.lock().unwrap().push(key);
                    Reply::from_string("").with_status_code(204)
                } else {
                    panic!("unexpected S3 operation")
                };
                request.respond(response).unwrap();
            }
        });
        assert!(execute_when_available(&c, &client)
            .unwrap_err()
            .to_string()
            .contains("metadata mismatch"));
        assert!(deletes.lock().unwrap().is_empty());
        assert_eq!(objects.lock().unwrap().len(), 4);
        bad_head.store(false, Ordering::SeqCst);
        let first = execute_when_available(&c, &client).unwrap();
        assert_eq!(first["repositories"][0]["removed_objects"], 1);
        assert_eq!(first["repositories"][0]["removed_versions"], 0);
        assert_eq!(
            first["repositories"][0]["skip_counts"]["batch-remaining"],
            1
        );
        let receipt = state_root
            .join("object-retention/images")
            .join(format!("{sha_a}.json"));
        assert_eq!(read_json(&receipt).unwrap()["status"], "deleting");
        let second = execute_when_available(&c, &client).unwrap();
        assert_eq!(second["repositories"][0]["removed_objects"], 1);
        assert_eq!(second["repositories"][0]["removed_versions"], 1);
        assert_eq!(read_json(&receipt).unwrap()["status"], "complete");
        assert_eq!(objects.lock().unwrap().len(), 2);
        assert_eq!(deletes.lock().unwrap().len(), 2);
        observe(
            &c,
            None,
            true,
            crate::common::now() - 1.,
            "complete",
            Some(&second),
            "applied",
        )
        .unwrap();
        let observed = read_json(&state_root.join("object-retention-results/images.json")).unwrap();
        assert_eq!(observed["last_apply"]["removed_versions"], 1);
        assert_eq!(
            observed["last_plan"]["policy_digest"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        stop.store(true, Ordering::SeqCst);
        worker.join().unwrap();
    }
}
