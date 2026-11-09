use super::*;
use std::time::SystemTime;
pub fn reference_fingerprint(c: &Value, key: &str) -> Result<String> {
    let base = image_base(c, key)?;
    let target = match fs::symlink_metadata(base.join("current")) {
        Ok(m) => {
            ensure!(m.file_type().is_symlink(), "current is not a link");
            Some(
                fs::read_link(base.join("current"))?
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let p = state(c)?.join("published").join(key);
    let published = if present(&p)? {
        read_json(&p)?
    } else {
        Value::Null
    };
    crate::common::digest(&json!([target, published]))
}
pub fn source_evidence(c: &Value, key: &str, sha: &str) -> Result<(Option<String>, String, Value)> {
    let p = state(c)?
        .join("object-restores")
        .join(key)
        .join(format!("{sha}.json"));
    if !present(&p)? {
        return Ok((None, "missing".into(), json!({"uncertain":false})));
    }
    let bytes = crate::common::read_regular(&p, 16384)?;
    let v: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        v["schema_version"] == 1
            && v["key"] == key
            && v["sha"] == sha
            && v["scope_digest"] == scoped_digest(c)?
            && matches!(
                v["status"].as_str(),
                Some("running" | "complete" | "failed")
            ),
        "invalid restoration report"
    );
    let cleaned = crate::info::reports::clean_restore(&v, key, sha, &scoped_digest(c)?)?;
    let uncertain = cleaned["uncertain"] == true;
    Ok((
        Some(hash(&bytes)),
        text(&v, "status")?.to_owned(),
        json!({"uncertain":uncertain}),
    ))
}
pub fn barrier(c: &Value, key: &str, sha: &str) -> Result<String> {
    let p = state(c)?
        .join("object-retention")
        .join(key)
        .join(format!("{sha}.json"));
    if !present(&p)? {
        return Ok("none".into());
    }
    let v = read_json(&p)?;
    ensure!(
        v["schema_version"] == 1
            && v["key"] == key
            && v["sha"] == sha
            && matches!(
                v["status"].as_str(),
                Some("deleting" | "complete" | "restored")
            ),
        "invalid restoration barrier"
    );
    Ok(text(&v, "status")?.to_owned())
}
pub fn mark_restored(c: &Value, key: &str, sha: &str) -> Result<()> {
    let marker = state(c)?
        .join("object-retention")
        .join(key)
        .join(format!("{sha}.json"));
    if present(&marker)? {
        barrier(c, key, sha)?;
        crate::common::atomic(
            &marker,
            &json!({"schema_version":1,"key":key,"sha":sha,"status":"restored","restored_at":crate::common::now()}),
        )?;
    }
    Ok(())
}
struct Budget {
    deadline: Instant,
    requests: usize,
    next: Instant,
}
impl Budget {
    fn new() -> Result<Self> {
        let seconds = env::var("WORKER_MAX_JOB_SECONDS")
            .unwrap_or_else(|_| "1800".into())
            .parse::<u64>()?;
        ensure!(
            (60..=86400).contains(&seconds),
            "invalid restoration time limit"
        );
        Ok(Self {
            deadline: Instant::now() + Duration::from_secs(seconds),
            requests: 0,
            next: Instant::now(),
        })
    }
    fn tick(&mut self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline && self.requests < 50000,
            "restoration budget exhausted"
        );
        if self.next > Instant::now() {
            std::thread::sleep(self.next - Instant::now());
        }
        ensure!(Instant::now() < self.deadline, "restoration timeout");
        self.requests += 1;
        self.next = Instant::now() + Duration::from_millis(50);
        Ok(())
    }
}
fn list_expected(
    client: &Client,
    m: &Value,
    expected: &BTreeMap<String, Value>,
    b: &mut Budget,
) -> Result<BTreeMap<String, Value>> {
    let own = format!(
        "repositories/{}/releases/{}/",
        text(m, "key")?,
        text(m, "sha")?
    );
    let mut prefixes = BTreeSet::new();
    for k in expected.keys() {
        prefixes.insert(k.split('/').take(4).collect::<Vec<_>>().join("/") + "/");
    }
    let mut out = BTreeMap::new();
    for prefix in prefixes {
        let mut token = None;
        let mut seen = BTreeSet::new();
        loop {
            b.tick()?;
            let page = client.list(&prefix, token.as_deref(), 1000)?;
            for obj in page["Contents"].as_array().unwrap() {
                let key = text(obj, "Key")?;
                if !expected.contains_key(key) && prefix != own {
                    continue;
                }
                let item = expected
                    .get(key)
                    .ok_or_else(|| anyhow!("unexpected restoration object"))?;
                ensure!(
                    obj["Size"] == item["size"]
                        && out.insert(key.to_owned(), obj.clone()).is_none(),
                    "restoration object size mismatch"
                );
            }
            if page["IsTruncated"] == false {
                break;
            }
            let t = text(&page, "NextContinuationToken")?.to_owned();
            ensure!(seen.insert(t.clone()), "restoration listing repeated token");
            token = Some(t);
        }
    }
    Ok(out)
}
fn verified(client: &Client, key: &str, item: &Value, b: &mut Budget) -> Result<Value> {
    b.tick()?;
    let meta = client.verify_object(key, item)?;
    ensure!(
        meta["size"] == item["size"] && meta["sha256"] == item["sha256"],
        "immutable restoration object conflict"
    );
    Ok(meta)
}
fn source_coverage(manifest: &Value) -> Result<BTreeMap<String, u64>> {
    let mut coverage = BTreeMap::new();
    for source in validate_manifest(manifest)?.values() {
        *coverage.entry(text(source, "key")?.to_owned()).or_default() += 1;
    }
    Ok(coverage)
}
fn metadata_matches(meta: &Value, item: &Value) -> bool {
    meta["metadata"]["sha256"] == item["sha256"]
        && meta["content_type"] == item["content_type"]
        && meta["cache_control"] == CACHE_CONTROL
}
fn images(files: &[Value]) -> Vec<Value> {
    files
        .iter()
        .filter_map(|item| {
            item["path"]
                .as_str()
                .and_then(|s| s.strip_prefix("images/"))
                .map(|path| {
                    let mut i = item.clone();
                    i["path"] = json!(path);
                    i
                })
        })
        .collect()
}
fn image_target(m: &Value) -> Result<String> {
    let sha = text(m, "sha")?;
    Ok(if m["image_layout"] == "snapshot-v1" {
        format!("{sha}/images")
    } else {
        format!("images-{sha}")
    })
}
fn copy_links(source: &Path, dest: &Path, files: &[Value]) -> Result<()> {
    fs::create_dir_all(dest)?;
    real_dir(dest)?;
    for item in files {
        let name = relative(text(item, "path")?)?;
        let from = source.join(name);
        let to = dest.join(name);
        verify_file(&from, item)?;
        fs::create_dir_all(to.parent().unwrap())?;
        fs::hard_link(&from, &to)?;
    }
    sync_tree(dest)?;
    Ok(())
}
fn sync_tree(root: &Path) -> Result<()> {
    let mut dirs = vec![root.to_owned()];
    let mut i = 0;
    while i < dirs.len() {
        for e in fs::read_dir(&dirs[i])? {
            let p = e?.path();
            let m = fs::symlink_metadata(&p)?;
            if m.is_dir() {
                dirs.push(p);
            } else {
                ensure!(m.is_file(), "unsafe local restore tree");
                safe_file(&p)?.sync_all()?;
            }
        }
        i += 1;
    }
    for p in dirs.iter().rev() {
        sync_dir(p)?;
    }
    Ok(())
}
fn persist(c: &Value, key: &str, sha: &str, v: &Value) -> Result<()> {
    crate::common::atomic(
        &state(c)?
            .join("object-restores")
            .join(key)
            .join(format!("{sha}.json")),
        v,
    )
}
pub fn run(c: &Value, key: &str, sha: &str, with_local: bool, rollback: bool) -> Result<Value> {
    ensure!(!rollback || with_local, "rollback requires --with-local");
    let repo = crate::common::repo(c, key)?;
    let mut report = json!({"schema_version":1,"key":key,"sha":sha,"status":"running","phase":"validate","started_at":crate::common::now(),"scope_digest":scoped_digest(c)?,"uploaded_objects":0,"repaired_objects":0,"verified_objects":0,"partial_possible":false,"with_local":with_local,"rollback_requested":rollback,"local_full_rebuilt":false,"local_images_rebuilt":false,"local_verified":false,"local_partial_possible":false,"activated":false,"activation_uncertain":false});
    persist(c, key, sha, &report)?;
    let result = (|| -> Result<()> {
        let mut budget = Budget::new()?;
        let manifest_path = state(c)?
            .join("manifests")
            .join(key)
            .join(format!("{sha}.json"));
        let original = read_json(&manifest_path)?;
        ensure!(
            original["key"] == key && original["sha"] == sha,
            "restoration manifest identity mismatch"
        );
        validate_manifest(&original)?;
        let files = original["files"].as_array().unwrap();
        let (_, _, digest) = super::object_cleanup::manifest_owned(c, key, sha)?;
        let marker = barrier(c, key, sha)?;
        if marker == "deleting" {
            let receipt = read_json(
                &state(c)?
                    .join("object-retention")
                    .join(key)
                    .join(format!("{sha}.json")),
            )?;
            ensure!(
                receipt["manifest_digest"] == digest
                    && receipt["bucket_scope"] == scoped_digest(c)?,
                "deletion receipt scope conflict"
            );
        }
        let g = state(c)?.join("git").join(key);
        fs::create_dir_all(&g)?;
        real_dir(&state(c)?)?;
        real_dir(g.parent().unwrap())?;
        real_dir(&g)?;
        if !present(&g.join("HEAD"))? {
            crate::publisher::git::git(c, &g, &["init", "--bare"])?;
        }
        report["phase"] = json!("git");
        crate::publisher::git::fetch(c, repo, &g)?;
        crate::publisher::git::git(
            c,
            &g,
            &["merge-base", "--is-ancestor", sha, "refs/heads/source"],
        )?;
        let base = image_base(c, key)?;
        fs::create_dir_all(&base)?;
        for directory in [
            PathBuf::from(text(c, "images_root")?),
            base.parent().unwrap().to_owned(),
            base.clone(),
        ] {
            real_dir(&directory)?;
        }
        let quota = repo["max_bytes"].as_u64().unwrap_or(1073741824);
        ensure!(
            quota > 0
                && crate::publisher::git::free_bytes(&base)?
                    >= quota
                        .checked_mul(if with_local { 3 } else { 2 })
                        .ok_or_else(|| anyhow!("restoration quota overflow"))?,
            "insufficient restoration staging capacity"
        );
        let staging = tempfile::Builder::new()
            .prefix(".restore-")
            .tempdir_in(&base)?;
        report["phase"] = json!("export");
        let rebuilt = crate::publisher::export(c, repo, &g, sha, staging.path())?;
        let identity = |a: &[Value]| -> BTreeMap<String, (u64, String)> {
            a.iter()
                .filter_map(|i| {
                    Some((
                        i["path"].as_str()?.to_owned(),
                        (i["size"].as_u64()?, i["sha256"].as_str()?.to_owned()),
                    ))
                })
                .collect()
        };
        ensure!(
            identity(&rebuilt) == identity(files),
            "Git export differs from restoration manifest"
        );
        crate::publisher::verify_tree(staging.path(), files)?;
        let releases = base.join("releases");
        fs::create_dir_all(&releases)?;
        real_dir(&releases)?;
        if with_local {
            report["phase"] = json!("local-preflight");
            for (root, items) in [
                (releases.join(sha), files.clone()),
                (releases.join(image_target(&original)?), images(files)),
            ] {
                if present(&root)? {
                    crate::publisher::verify_tree(&root, &items)?;
                }
            }
        }
        if rollback {
            protect(c, key, &[], &[])?;
        }
        let client = Client::from_config(c)?;
        let expected = expected_objects(&original)?;
        let coverage = source_coverage(&original)?;
        report["phase"] = json!("preflight");
        report["files"] = json!(files.len());
        report["logical_bytes"] = json!(files
            .iter()
            .map(|i| i["size"].as_u64().unwrap())
            .sum::<u64>());
        let found = list_expected(&client, &original, &expected, &mut budget)?;
        let mut repair = BTreeSet::new();
        for name in found.keys() {
            if !metadata_matches(
                &verified(&client, name, &expected[name], &mut budget)?,
                &expected[name],
            ) {
                repair.insert(name.clone());
            }
        }
        report["phase"] = json!("upload");
        for (name, item) in &expected {
            if found.contains_key(name) && !repair.contains(name) {
                continue;
            }
            budget.tick()?;
            let existing = client.head(name)?;
            if existing.is_some() {
                let meta = verified(&client, name, item, &mut budget)?;
                if metadata_matches(&meta, item) {
                    continue;
                }
            }
            report["partial_possible"] = json!(true);
            persist(c, key, sha, &report)?;
            budget.tick()?;
            client.put_file(name,&staging.path().join(text(item,"path")?),&json!({"sha256":item["sha256"],"content_type":item["content_type"],"cache_control":CACHE_CONTROL,"metadata":existing.as_ref().map(|v|v["metadata"].clone()).unwrap_or_else(||json!({}))}))?;
            let field = if existing.is_some() {
                "repaired_objects"
            } else {
                "uploaded_objects"
            };
            report[field] = json!(report[field].as_u64().unwrap() + 1);
        }
        report["phase"] = json!("verify");
        let listed = list_expected(&client, &original, &expected, &mut budget)?;
        ensure!(
            listed.keys().eq(expected.keys()),
            "incomplete restored object set"
        );
        for (name, item) in &expected {
            ensure!(
                metadata_matches(&verified(&client, name, item, &mut budget)?, item),
                "restored metadata mismatch"
            );
            report["verified_objects"] = json!(
                report["verified_objects"].as_u64().unwrap()
                    + coverage.get(name).copied().unwrap_or(0)
            );
        }
        ensure!(
            read_json(&manifest_path)? == original,
            "manifest changed during restoration"
        );
        if with_local {
            report["phase"] = json!("local-install");
            report["local_partial_possible"] = json!(true);
            persist(c, key, sha, &report)?;
            let full = releases.join(sha);
            if !present(&full)? {
                sync_tree(staging.path())?;
                fs::rename(staging.path(), &full)?;
                sync_dir(&releases)?;
                report["local_full_rebuilt"] = json!(true);
            }
            if original["image_layout"] != "snapshot-v1" {
                let target = releases.join(image_target(&original)?);
                if !present(&target)? {
                    let temp = tempfile::Builder::new()
                        .prefix(".restore-images-")
                        .tempdir_in(&base)?;
                    copy_links(&full.join("images"), temp.path(), &images(files))?;
                    fs::rename(temp.path(), &target)?;
                    sync_dir(&releases)?;
                    report["local_images_rebuilt"] = json!(true);
                }
            }
            crate::publisher::verify_tree(&full, files)?;
            crate::publisher::verify_tree(
                &releases.join(image_target(&original)?),
                &images(files),
            )?;
            report["local_verified"] = json!(true);
            report["local_partial_possible"] = json!(false);
        }
        File::open(&manifest_path)?
            .set_times(fs::FileTimes::new().set_modified(SystemTime::now()))?;
        mark_restored(c, key, sha)?;
        let seal = state(c)?.join("incremental").join(format!("{key}.json"));
        if present(&seal)? {
            crate::common::atomic(
                &seal,
                &json!({"version":0,"reason":"explicit-object-restoration"}),
            )?;
        }
        if rollback {
            report["phase"] = json!("activate");
            report["activation_uncertain"] = json!(true);
            persist(c, key, sha, &report)?;
            crate::publisher::rollback_images_locked(c, key, sha)?;
            report["activated"] = json!(true);
            report["activation_uncertain"] = json!(false);
        }
        report["phase"] = json!("complete");
        report["partial_possible"] = json!(false);
        report["manifest_digest"] = json!(digest);
        Ok(())
    })();
    report["finished_at"] = json!(crate::common::now());
    match result {
        Ok(()) => {
            report["status"] = json!("complete");
            persist(c, key, sha, &report)?;
            Ok(report)
        }
        Err(e) => {
            report["status"] = json!("failed");
            report["error"] = json!(error_class(&e));
            persist(c, key, sha, &report)?;
            Err(e)
        }
    }
}
pub fn error_class(e: &anyhow::Error) -> &'static str {
    if let Some(e) = e.downcast_ref::<std::io::Error>() {
        match e.kind() {
            std::io::ErrorKind::NotFound => "FileNotFoundError",
            std::io::ErrorKind::PermissionDenied => "PermissionError",
            std::io::ErrorKind::TimedOut => "TimeoutError",
            _ => "OSError",
        }
    } else {
        "ValueError"
    }
}
fn reason(e: &anyhow::Error) -> &'static str {
    match error_class(e) {
        "FileNotFoundError" => "missing",
        "PermissionError" => "permission",
        "TimeoutError" => "timeout",
        "OSError" => "io",
        _ => "mismatch",
    }
}
pub fn check(c: &Value, key: &str, sha: &str) -> Result<Value> {
    let path = state(c)?
        .join("restore-checks")
        .join(key)
        .join(format!("{sha}.json"));
    let mut report = json!({"schema_version":1,"key":key,"sha":sha,"status":"running","phase":"validate","started_at":crate::common::now(),"scope_digest":scoped_digest(c)?,"diagnosis":"unresolved","verified_objects":0,"checks":{"objects":{"status":"unavailable","reason":"not-checked"},"full":{"status":"unavailable","reason":"not-checked"},"images":{"status":"unavailable","reason":"not-checked"},"references":{"status":"unavailable","reason":"not-checked"},"barrier":{"status":"unavailable","reason":"not-checked"}}});
    crate::common::atomic(&path, &report)?;
    let result = (|| -> Result<()> {
        let original = read_json(
            &state(c)?
                .join("manifests")
                .join(key)
                .join(format!("{sha}.json")),
        )?;
        ensure!(
            original["key"] == key && original["sha"] == sha,
            "check manifest identity"
        );
        validate_manifest(&original)?;
        report["manifest_digest"] = json!(crate::common::digest(&original)?);
        let files = original["files"].as_array().unwrap();
        report["files"] = json!(files.len());
        match source_evidence(c, key, sha) {
            Ok((digest, status, evidence)) => {
                report["source_report_digest"] = json!(digest);
                report["source_status"] = json!(status);
                report["source_uncertain"] = evidence["uncertain"].clone();
            }
            Err(e) => {
                report["source_status"] = json!("unavailable");
                report["source_reason"] = json!(reason(&e));
            }
        }
        let mut budget = Budget::new()?;
        let client = Client::from_config(c)?;
        for phase in ["objects", "full", "images", "references", "barrier"] {
            report["phase"] = json!(phase);
            crate::common::atomic(&path, &report)?;
            let operation = (|| -> Result<String> {
                ensure!(Instant::now() < budget.deadline, "check timeout");
                match phase {
                    "objects" => {
                        let expected = expected_objects(&original)?;
                        let coverage = source_coverage(&original)?;
                        let found = list_expected(&client, &original, &expected, &mut budget)?;
                        ensure!(found.keys().eq(expected.keys()), "incomplete object set");
                        for (name, item) in &expected {
                            ensure!(
                                metadata_matches(
                                    &verified(&client, name, item, &mut budget)?,
                                    item
                                ),
                                "object metadata mismatch"
                            );
                            report["verified_objects"] = json!(
                                report["verified_objects"].as_u64().unwrap()
                                    + coverage.get(name).copied().unwrap_or(0)
                            );
                        }
                        Ok("verified".into())
                    }
                    "full" | "images" => {
                        let base = image_base(c, key)?;
                        let (root, items) = if phase == "full" {
                            (base.join("releases").join(sha), files.clone())
                        } else {
                            (
                                base.join("releases").join(image_target(&original)?),
                                images(files),
                            )
                        };
                        crate::publisher::verify_tree(&root, &items)?;
                        Ok("verified".into())
                    }
                    "references" => {
                        report["reference_digest"] = json!(reference_fingerprint(c, key)?);
                        let current = current_sha(&image_base(c, key)?)?;
                        let p = state(c)?.join("published").join(key);
                        let published = if present(&p)? {
                            Some(read_json(&p)?)
                        } else {
                            None
                        };
                        report["current_is_target"] = json!(current.as_deref() == Some(sha));
                        report["published_is_target"] =
                            json!(published.as_ref().is_some_and(|v| v["sha"] == sha));
                        let Some(current) = current else {
                            ensure!(published.is_none(), "published without current");
                            return Ok("not-published".into());
                        };
                        let m = read_json(
                            &state(c)?
                                .join("manifests")
                                .join(key)
                                .join(format!("{current}.json")),
                        )?;
                        validate_manifest(&m)?;
                        ensure!(
                            published == Some(m),
                            "current and published manifest disagree"
                        );
                        Ok(if current == sha {
                            "target-current"
                        } else {
                            "other-current"
                        }
                        .into())
                    }
                    _ => barrier(c, key, sha),
                }
            })();
            report["checks"][phase] = match operation {
                Ok(detail) => {
                    json!({"status":if phase=="barrier"&&matches!(detail.as_str(),"deleting"|"complete"){"blocked"}else{"ok"},"reason":detail})
                }
                Err(e) => json!({"status":"failed","reason":reason(&e)}),
            };
        }
        ensure!(
            read_json(
                &state(c)?
                    .join("manifests")
                    .join(key)
                    .join(format!("{sha}.json"))
            )? == original,
            "manifest changed during check"
        );
        report["diagnosis"] = json!(if report["checks"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v["status"] == "ok")
            && report["source_status"] != "unavailable"
        {
            "verified"
        } else {
            "issues"
        });
        Ok(())
    })();
    match result {
        Ok(()) => {
            report["status"] = json!("complete");
            report["phase"] = json!("complete");
        }
        Err(e) => {
            report["status"] = json!("failed");
            report["diagnosis"] = json!("unresolved");
            report["failure_reason"] = json!(reason(&e));
        }
    }
    report["finished_at"] = json!(crate::common::now());
    crate::common::atomic(&path, &report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_source_covers_all_logical_files_without_duplicate_gets() {
        let sha = "a".repeat(40);
        let key = format!("repositories/images/releases/{sha}/images/a.png");
        let item = |path: &str| json!({"path":path,"size":7,"sha256":"b".repeat(64),"content_type":"image/png"});
        let manifest = json!({"key":"images","sha":sha,"object_layout":"references-v1","files":[item("images/a.png"),item("images/b.png")],"object_sources":{"images/b.png":{"key":key,"content_type":"image/png"}}});
        assert_eq!(source_coverage(&manifest).unwrap()[&key], 2);
        assert_eq!(expected_objects(&manifest).unwrap().len(), 1);
    }
}
