//! Durable signed event admission and replay-safe single-host publication.
use crate::common::{atomic, load, now, read_json, repo};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
pub mod git;
pub mod mime;
pub use mime::mime;
pub mod prepared;
pub mod queue;
pub mod snapshot;
pub use queue::{enqueue, read_job};
pub fn hex_string(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn valid_sha(s: &str) -> bool {
    hex_string(s, 40) && s.bytes().any(|b| b != b'0')
}
pub fn valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
pub fn validate(c: &Value) -> Result<()> {
    if let Some(p) = c.get("publication") {
        let m = p.as_object().context("invalid publication settings")?;
        ensure!(
            m.keys()
                .all(|k| ["object_workers", "reference_releases"].contains(&k.as_str())),
            "invalid publication settings"
        );
        if let Some(v) = p.get("reference_releases") {
            ensure!(v.is_boolean(), "reference_releases must be boolean")
        }
    }
    let workers = c["publication"]["object_workers"].as_u64().unwrap_or(4);
    ensure!((1..=8).contains(&workers), "invalid object workers");
    Ok(())
}
fn state(c: &Value) -> Result<PathBuf> {
    Ok(PathBuf::from(
        c["state_root"].as_str().context("state_root")?,
    ))
}
fn base(c: &Value, key: &str) -> Result<PathBuf> {
    ensure!(valid_key(key), "invalid repository key");
    Ok(Path::new(c["images_root"].as_str().context("images_root")?)
        .join("repos")
        .join(key))
}
fn refs(c: &Value) -> bool {
    c["publication"]["reference_releases"]
        .as_bool()
        .unwrap_or(false)
}
pub fn verify_tree(root: &Path, files: &[Value]) -> Result<()> {
    snapshot::verify(root, files, None)
}

fn prepare_parent(dest: &Path, name: &str) -> Result<()> {
    snapshot::relative(name)?;
    let parent = dest
        .join(name)
        .parent()
        .context("snapshot parent")?
        .to_path_buf();
    let rel = parent.strip_prefix(dest)?;
    let mut path = dest.to_path_buf();
    ensure!(fs::symlink_metadata(dest)?.is_dir(), "unsafe staging root");
    for part in rel.components() {
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
                fs::symlink_metadata(&path)?.is_dir(),
                "unsafe snapshot directory"
            ),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[derive(Default)]
struct Exported {
    files: Vec<Value>,
    oids: BTreeMap<String, String>,
    proofs: BTreeMap<String, Value>,
    reused: BTreeMap<String, snapshot::Source>,
    claim: Option<prepared::Claim>,
}
fn export_plan(
    c: &Value,
    g: &Path,
    blobs: &[git::Blob],
    dest: &Path,
    sources: &mut BTreeMap<String, snapshot::Source>,
    claim: Option<prepared::Claim>,
) -> Result<Exported> {
    fs::create_dir_all(dest)?;
    ensure!(fs::symlink_metadata(dest)?.is_dir(), "unsafe staging root");
    let mut out = Exported {
        claim,
        ..Exported::default()
    };
    let mut linked = prepare_links(c, blobs, dest, sources, out.claim.as_ref())?;
    let mut batch = None;
    for (index, blob) in blobs.iter().enumerate() {
        prepare_parent(dest, &blob.relative)?;
        let target = dest.join(&blob.relative);
        let item;
        if let Some((cached, proof, source)) = linked.remove(&index) {
            item = cached;
            out.proofs.insert(blob.relative.clone(), proof);
            out.reused.insert(blob.relative.clone(), source);
        } else if let Some(source) = sources
            .get_mut(&blob.oid)
            .filter(|s| s.item["size"] == blob.size)
        {
            let mut proof = if out
                .claim
                .as_ref()
                .is_some_and(|c| c.retained.contains(&blob.relative))
            {
                ensure!(
                    snapshot::matches(&target, &source.item, &source.proof, false),
                    "retained snapshot identity changed"
                );
                source.proof.clone()
            } else {
                snapshot::safe_link(&source.path, &target, &source.item, &source.proof)?
            };
            let source_fp = snapshot::fingerprint(&source.path)?;
            if source_fp[0] == proof["fingerprint"][0] && source_fp[1] == proof["fingerprint"][1] {
                source.proof = proof.clone();
            }
            let mut reused = source.item.clone();
            reused["path"] = blob.relative.clone().into();
            reused["content_type"] = mime(&blob.original).into();
            item = reused;
            // Every alias receives the final identity below, without extending byte-proof age.
            proof["fingerprint"] = snapshot::fingerprint(&target)?;
            out.proofs.insert(blob.relative.clone(), proof);
            out.reused.insert(blob.relative.clone(), source.clone());
        } else {
            if batch.is_none() {
                batch = Some(git::Batch::new(c, g)?)
            }
            let digest = batch
                .as_mut()
                .context("Git batch")?
                .write(&blob.oid, blob.size, &target)?;
            item = json!({"path":blob.relative,"size":blob.size,"sha256":digest,"content_type":mime(&blob.original)});
            out.proofs.insert(
                blob.relative.clone(),
                snapshot::proof(&target, &item, now())?,
            );
        }
        if blob.relative.to_ascii_lowercase().ends_with(".svg") {
            snapshot::validate_svg(&target)?
        }
        out.oids.insert(blob.relative.clone(), blob.oid.clone());
        out.files.push(item);
    }
    if let Some(batch) = batch.as_mut() {
        batch.finish()?;
    }
    for (name, proof) in &mut out.proofs {
        proof["fingerprint"] = snapshot::fingerprint(&dest.join(name))?;
    }
    snapshot::images(&out.files)?;
    Ok(out)
}
type Linked = BTreeMap<usize, (Value, Value, snapshot::Source)>;
fn prepare_links(
    c: &Value,
    blobs: &[git::Blob],
    dest: &Path,
    sources: &mut BTreeMap<String, snapshot::Source>,
    claim: Option<&prepared::Claim>,
) -> Result<Linked> {
    let workers = c["publication"]["object_workers"].as_u64().unwrap_or(4) as usize;
    let mut groups: BTreeMap<(u64, u64), Vec<(usize, git::Blob)>> = BTreeMap::new();
    let mut count = 0;
    for (index, blob) in blobs.iter().enumerate() {
        if claim.is_some_and(|c| c.retained.contains(&blob.relative)) {
            continue;
        }
        if let Some(source) = sources
            .get(&blob.oid)
            .filter(|s| s.item["size"] == blob.size)
        {
            let fp = &source.proof["fingerprint"];
            let id = (
                fp[0].as_u64().context("proof dev")?,
                fp[1].as_u64().context("proof inode")?,
            );
            groups.entry(id).or_default().push((index, blob.clone()));
            count += 1;
        }
    }
    if count < 32 || workers == 1 {
        return Ok(BTreeMap::new());
    }
    for entries in groups.values() {
        for (_, blob) in entries {
            prepare_parent(dest, &blob.relative)?
        }
    }
    let groups = groups.into_values().collect::<Vec<_>>();
    let batches = groups.chunks(32).collect::<Vec<_>>();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let original = &*sources;
    let results = thread::scope(
        |scope| -> Result<Vec<(Linked, BTreeMap<String, snapshot::Source>)>> {
            let mut handles = Vec::new();
            for _ in 0..workers {
                handles.push(scope.spawn(
                    || -> Result<(Linked, BTreeMap<String, snapshot::Source>)> {
                        let result = (|| -> Result<_> {
                            let (mut linked, mut updated) = (BTreeMap::new(), BTreeMap::new());
                            loop {
                                if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                                    break;
                                }
                                let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let Some(batch) = batches.get(n) else { break };
                                for entries in *batch {
                                    let mut local = entries
                                        .iter()
                                        .map(|(_, blob)| {
                                            (blob.oid.clone(), original[&blob.oid].clone())
                                        })
                                        .collect::<BTreeMap<_, _>>();
                                    for (index, blob) in entries {
                                        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                                            break;
                                        }
                                        let source =
                                            local.get_mut(&blob.oid).context("link source")?;
                                        let proof = snapshot::safe_link(
                                            &source.path,
                                            &dest.join(&blob.relative),
                                            &source.item,
                                            &source.proof,
                                        )?;
                                        let fp = snapshot::fingerprint(&source.path)?;
                                        if fp[0] == proof["fingerprint"][0]
                                            && fp[1] == proof["fingerprint"][1]
                                        {
                                            source.proof = proof.clone();
                                        }
                                        let mut item = source.item.clone();
                                        item["path"] = blob.relative.clone().into();
                                        item["content_type"] = mime(&blob.original).into();
                                        linked.insert(*index, (item, proof, source.clone()));
                                    }
                                    updated.extend(local);
                                }
                            }
                            Ok((linked, updated))
                        })();
                        if result.is_err() {
                            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        result
                    },
                ));
            }
            let (mut all, mut error) = (Vec::new(), None);
            for handle in handles {
                match handle.join() {
                    Ok(Ok(v)) => all.push(v),
                    Ok(Err(e)) => {
                        error.get_or_insert(e);
                    }
                    Err(_) => {
                        cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                        error.get_or_insert(anyhow!("snapshot linking worker failed"));
                    }
                }
            }
            if let Some(e) = error {
                Err(e)
            } else {
                Ok(all)
            }
        },
    )?;
    let mut linked = BTreeMap::new();
    for (items, updated) in results {
        linked.extend(items);
        sources.extend(updated);
    }
    Ok(linked)
}
/// Git-only export used by restoration; never executes repository code or uploads.
pub fn export(c: &Value, r: &Value, g: &Path, sha: &str, dest: &Path) -> Result<Vec<Value>> {
    let blobs = git::plan(c, r, g, sha)?;
    Ok(export_plan(c, g, &blobs, dest, &mut BTreeMap::new(), None)?.files)
}
fn receipt(c: &Value, key: &str, sha: &str, restored: bool) -> Result<()> {
    let p = state(c)?
        .join("object-retention")
        .join(key)
        .join(format!("{sha}.json"));
    match fs::symlink_metadata(&p) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let v = read_json(&p, 8 << 20)?;
    ensure!(
        v["key"] == key
            && v["sha"] == sha
            && ["complete", "deleting", "restored"].contains(&v["status"].as_str().unwrap_or("")),
        "invalid object restoration record"
    );
    if restored {
        atomic(
            &p,
            &json!({"schema_version":1,"key":key,"sha":sha,"status":"restored","restored_at":now()}),
        )
    } else {
        ensure!(
            v["status"] == "restored",
            "version objects were deleted; restore before rollback"
        );
        Ok(())
    }
}
fn remember_previous(c: &Value, key: &str, next: &str) -> Result<()> {
    let r = repo(c, key)?;
    let count = r["retention"]["rollback_versions"]
        .as_u64()
        .or_else(|| c["retention"]["rollback_versions"].as_u64())
        .unwrap_or(2)
        .min(100) as usize;
    let p = state(c)?
        .join("rollback-protection")
        .join(format!("{key}.json"));
    let previous = match current_sha(c, key) {
        Ok(sha) => Some(sha),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(e) => return Err(e),
    };
    let mut values = Vec::new();
    for sha in previous
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(next))
    {
        for prefix in ["", "images-"] {
            let path = base(c, key)?
                .join("releases")
                .join(format!("{prefix}{sha}"));
            match fs::symlink_metadata(&path) {
                Ok(m) => {
                    ensure!(m.is_dir(), "unsafe rollback protection directory");
                    use std::os::fd::AsRawFd;
                    use std::os::unix::fs::OpenOptionsExt;
                    let directory = fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                        .open(&path)?;
                    ensure!(
                        unsafe { libc::futimens(directory.as_raw_fd(), std::ptr::null()) } == 0,
                        "rollback protection timestamp failed"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    if let Some(previous) = previous {
        values.push(previous)
    }
    if fs::symlink_metadata(&p).is_ok() {
        let old = read_json(&p, 16384)?;
        ensure!(
            old["key"] == key
                && old["shas"].as_array().is_some_and(
                    |v| v.len() <= 100 && v.iter().all(|v| v.as_str().is_some_and(valid_sha))
                ),
            "invalid rollback protection"
        );
        values.extend(
            old["shas"]
                .as_array()
                .context("rollback protection")?
                .iter()
                .filter_map(|v| v.as_str().map(String::from)),
        );
    }
    let mut seen = BTreeSet::new();
    values.retain(|s| s != next && seen.insert(s.clone()));
    values.truncate(count);
    atomic(&p, &json!({"key":key,"shas":values}))
}
pub fn current_sha(c: &Value, key: &str) -> Result<String> {
    let link = fs::read_link(base(c, key)?.join("current"))?;
    let s = link.to_str().context("invalid current link")?;
    let sha = if let Some(s) = s.strip_prefix("releases/images-") {
        s
    } else {
        s.strip_prefix("releases/")
            .and_then(|s| s.strip_suffix("/images"))
            .context("invalid current link")?
    };
    ensure!(valid_sha(sha), "invalid current version");
    Ok(sha.into())
}
pub fn rollback_images_locked(c: &Value, key: &str, sha: &str) -> Result<()> {
    repo(c, key)?;
    ensure!(valid_sha(sha), "invalid SHA");
    receipt(c, key, sha, false)?;
    let manifest = read_json(
        &state(c)?
            .join("manifests")
            .join(key)
            .join(format!("{sha}.json")),
        8 << 20,
    )?;
    ensure!(
        manifest["key"] == key && manifest["sha"] == sha,
        "manifest identity mismatch"
    );
    crate::storage::validate_manifest(&manifest)?;
    let files = manifest["files"].as_array().context("manifest files")?;
    let base = base(c, key)?;
    snapshot::verify(&base.join("releases").join(sha), files, None)?;
    let p = state(c)?.join("published").join(key);
    let saved = if fs::symlink_metadata(&p).is_ok() {
        Some(read_json(&p, 8 << 20)?)
    } else {
        None
    };
    let previous = fs::read_link(base.join("current")).ok();
    remember_previous(c, key, sha)?;
    let result = (|| -> Result<()> {
        snapshot::activate(
            &base,
            &snapshot::image_target(&manifest)?,
            &snapshot::images(files)?,
            None,
        )?;
        atomic(&p, &manifest)
    })();
    if result.is_err() {
        if let Some(previous) = previous {
            let s = previous
                .to_str()
                .context("invalid previous target")?
                .strip_prefix("releases/")
                .context("invalid previous target")?;
            snapshot::switch(&base, s)?
        } else {
            let _ = fs::remove_file(base.join("current"));
            snapshot::sync_dir(&base)?;
        }
        if let Some(saved) = saved {
            atomic(&p, &saved)?
        } else {
            let _ = fs::remove_file(&p);
            if p.parent().is_some_and(Path::exists) {
                snapshot::sync_dir(p.parent().unwrap())?
            }
        }
    }
    result
}
fn transfer(
    client: &crate::storage::Client,
    prefix: &str,
    staging: &Path,
    item: &Value,
    cached: Option<&snapshot::Source>,
) -> Result<(Value, String)> {
    let name = item["path"].as_str().context("file path")?;
    let key = format!("{prefix}{name}");
    let exists = client.head(&key)?.is_some();
    let mode = if exists {
        "existing"
    } else if cached.is_some() {
        "copied-readback"
    } else {
        "uploaded"
    };
    if !exists {
        let metadata = json!({"sha256":item["sha256"],"content_type":item["content_type"],"cache_control":crate::storage::CACHE_CONTROL});
        if let Some(cached) = cached {
            let mut metadata = metadata;
            metadata["etag"] = cached.etag.clone();
            client.copy(
                cached.object["key"].as_str().context("cached source key")?,
                &key,
                &metadata,
            )?;
        } else {
            client.put_file(&key, &staging.join(name), &metadata)?;
        }
    }
    let receipt = client.verify_object(&key, item)?;
    ensure!(
        receipt["size"] == item["size"] && receipt["sha256"] == item["sha256"],
        "object readback integrity mismatch"
    );
    let etag = if receipt["etag"] == receipt["body_etag"] {
        receipt["etag"].clone()
    } else {
        Value::Null
    };
    Ok((
        json!({"etag":etag,"modified":receipt["modified"]}),
        mode.into(),
    ))
}
fn connection(c: &Value) -> Result<crate::storage::Client> {
    use std::os::unix::fs::MetadataExt;
    type Cached = Option<(Value, Instant, crate::storage::Client)>;
    static CLIENT: std::sync::OnceLock<Mutex<Cached>> = std::sync::OnceLock::new();
    let mut identity = vec![c["endpoint"].clone(), c["bucket"].clone()];
    for name in ["S3_ACCESS_KEY_FILE", "S3_SECRET_KEY_FILE", "S3_CA_FILE"] {
        if let Ok(path) = std::env::var(name) {
            let m = fs::metadata(&path)?;
            identity.push(json!([
                path,
                m.dev(),
                m.ino(),
                m.size(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec()
            ]));
        } else {
            identity.push(Value::Null)
        }
    }
    let identity = Value::Array(identity);
    let mut cached = CLIENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow!("transport cache unavailable"))?;
    if let Some((old, created, client)) = &*cached {
        if old == &identity && created.elapsed() < Duration::from_secs(30) {
            return Ok(client.clone());
        }
    }
    let client = crate::storage::Client::from_config(c)?;
    *cached = Some((identity, Instant::now(), client.clone()));
    Ok(client)
}
struct SourceCheck(Option<thread::JoinHandle<Result<Value>>>);
impl SourceCheck {
    fn finish(&mut self) -> Result<Value> {
        self.0
            .take()
            .context("source verification unavailable")?
            .join()
            .map_err(|_| anyhow!("source verifier failed"))?
    }
}
impl Drop for SourceCheck {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}
fn upload(
    c: &Value,
    client: &crate::storage::Client,
    prefix: &str,
    staging: &Path,
    exported: &Exported,
    checked: Option<Value>,
) -> Result<(BTreeMap<String, Value>, Value, Value)> {
    let records = if refs(c) {
        exported.reused.values().map(|s|json!({"key":s.object["key"],"etag":s.etag,"modified":s.modified,"item":s.item})).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut stats = if records.is_empty() {
        json!({})
    } else {
        match checked {
            Some(value) => value,
            None => crate::storage::verify_sources(c, client, &records)?,
        }
    };
    let tasks = exported
        .files
        .iter()
        .filter(|item| {
            !refs(c)
                || !exported
                    .reused
                    .contains_key(item["path"].as_str().unwrap_or(""))
        })
        .collect::<Vec<_>>();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = c["publication"]["object_workers"].as_u64().unwrap_or(4) as usize;
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let results = thread::scope(|scope| -> Result<Vec<(String, Value, String)>> {
        let mut handles = Vec::new();
        for _ in 0..workers {
            handles.push(scope.spawn(|| -> Result<Vec<(String, Value, String)>> {
                let mut out = Vec::new();
                loop {
                    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = tasks.get(n) else { break };
                    let (r, mode) = match transfer(
                        client,
                        prefix,
                        staging,
                        item,
                        exported.reused.get(item["path"].as_str().unwrap_or("")),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                            return Err(e);
                        }
                    };
                    out.push((
                        item["path"].as_str().context("file path")?.to_owned(),
                        r,
                        mode,
                    ));
                }
                Ok(out)
            }));
        }
        let (mut all, mut failure) = (Vec::new(), None);
        for h in handles {
            match h.join() {
                Ok(Ok(v)) => all.extend(v),
                Ok(Err(e)) => {
                    failure.get_or_insert(e);
                }
                Err(_) => {
                    failure.get_or_insert(anyhow!("upload worker failed"));
                }
            }
        }
        if let Some(e) = failure {
            Err(e)
        } else {
            Ok(all)
        }
    })?;
    let mut etags = BTreeMap::new();
    for (name, r, mode) in results {
        etags.insert(name, r);
        let count = stats[&mode].as_u64().unwrap_or(0);
        stats[&mode] = (count + 1).into();
    }
    let mut objects = serde_json::Map::new();
    if refs(c) {
        for (name, s) in &exported.reused {
            etags.insert(name.clone(), json!({"etag":s.etag,"modified":s.modified}));
            objects.insert(name.clone(), s.object.clone());
        }
        stats["referenced"] = exported.reused.len().into();
    }
    Ok((etags, Value::Object(objects), stats))
}
pub fn publish(c: &Value, j: &mut Value) -> Result<()> {
    validate(c)?;
    let start = Instant::now();
    let mut last = Instant::now();
    j["timings"] = json!({});
    let key = j["key"].as_str().context("job key")?.to_string();
    let sha = j["sha"].as_str().context("job SHA")?.to_string();
    let r = repo(c, &key)?;
    let g = state(c)?.join("git").join(&key);
    let head = git::fetch(c, r, &g)?;
    if j["kind"] == "auto" && head != sha {
        j["status"] = "superseded".into();
        return Ok(());
    }
    if sha != head {
        git::git(c, &g, &["merge-base", "--is-ancestor", &sha, &head])?;
    }
    stage(j, "fetch", &mut last);
    let base = base(c, &key)?;
    fs::create_dir_all(base.join("releases"))?;
    ensure!(
        git::free_bytes(&base)?
            >= r["max_bytes"]
                .as_u64()
                .unwrap_or(1073741824)
                .saturating_mul(2),
        "insufficient staging capacity"
    );
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&base)?;
    let mut sources = snapshot::sources(c, r).unwrap_or_default();
    let known = sources
        .iter()
        .filter_map(|(oid, s)| s.item["size"].as_u64().map(|size| (oid.clone(), size)))
        .collect();
    let blobs = git::plan_cached(c, r, &g, &sha, &known)?;
    let planned = if refs(c) {
        blobs.iter().filter_map(|blob|sources.get(&blob.oid).filter(|s|s.item["size"]==blob.size).map(|s|(blob.relative.clone(),json!({"key":s.object["key"],"etag":s.etag,"modified":s.modified,"item":s.item})))).collect::<BTreeMap<_,_>>()
    } else {
        BTreeMap::new()
    };
    let client = connection(c)?;
    let mut checking = if planned.is_empty() {
        SourceCheck(None)
    } else {
        let records = planned.values().cloned().collect::<Vec<_>>();
        let config = c.clone();
        let transport = client.clone();
        SourceCheck(Some(thread::spawn(move || {
            crate::storage::verify_sources(&config, &transport, &records)
        })))
    };
    let claim = prepared::claim(c, r, &sha, staging.path(), &blobs, &mut sources)?;
    let mut exported = export_plan(c, &g, &blobs, staging.path(), &mut sources, claim)?;
    let checked = if planned.is_empty() {
        None
    } else {
        let actual=exported.reused.iter().map(|(name,s)|(name.clone(),json!({"key":s.object["key"],"etag":s.etag,"modified":s.modified,"item":s.item}))).collect::<BTreeMap<_,_>>();
        ensure!(actual == planned, "source verification plan changed");
        Some(checking.finish()?)
    };
    stage(j, "export", &mut last);
    let prefix = format!("repositories/{key}/releases/{sha}/");
    let (etags, object_sources, stats) =
        upload(c, &client, &prefix, staging.path(), &exported, checked)?;
    stage(j, "objects", &mut last);
    let mut manifest = json!({"key":key,"sha":sha,"files":exported.files,"config":c["revision"]});
    if refs(c) {
        manifest["object_layout"] = "references-v1".into();
        manifest["object_sources"] = object_sources.clone();
        manifest["image_layout"] = "snapshot-v1".into();
    }
    crate::storage::validate_manifest(&manifest)?;
    receipt(c, &key, &sha, true)?;
    atomic(
        &state(c)?
            .join("manifests")
            .join(&key)
            .join(format!("{sha}.json")),
        &manifest,
    )?;
    let final_path = base.join("releases").join(&sha);
    if final_path.try_exists()? {
        snapshot::verify(&final_path, &exported.files, None)?;
        for item in &exported.files {
            let name = item["path"].as_str().context("file path")?;
            exported.proofs.insert(
                name.into(),
                snapshot::proof(&final_path.join(name), item, now())?,
            );
        }
    } else {
        if refs(c) {
            snapshot::sync_directories(staging.path(), exported.claim.as_ref().map(|c| &c.dirty))?;
        } else {
            snapshot::sync_tree(staging.path())?;
        }
        fs::rename(staging.path(), &final_path)?;
        snapshot::sync_dir(&base.join("releases"))?;
    }
    let target = snapshot::image_target(&manifest)?;
    if !refs(c) {
        let image = base.join("releases").join(&target);
        if !image.try_exists()? {
            let temporary = tempfile::Builder::new()
                .prefix(".images-")
                .tempdir_in(&base)?;
            for item in &exported.files {
                let name = item["path"].as_str().context("file path")?;
                if let Some(relative) = name.strip_prefix("images/") {
                    prepare_parent(temporary.path(), relative)?;
                    fs::hard_link(final_path.join(name), temporary.path().join(relative))?;
                }
            }
            snapshot::sync_tree(temporary.path())?;
            fs::rename(temporary.path(), image)?;
            snapshot::sync_dir(&base.join("releases"))?;
        }
    }
    for (name, p) in &mut exported.proofs {
        p["fingerprint"] = snapshot::fingerprint(&final_path.join(name))?;
    }
    stage(j, "snapshot", &mut last);
    let head = if refs(c) && j["kind"] == "auto" {
        git::remote_head(c, r, &g)?
    } else {
        git::fetch(c, r, &g)?
    };
    if j["kind"] == "auto" && head != sha {
        j["status"] = "superseded".into();
        return Ok(());
    }
    let previous = fs::read_link(base.join("current"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    j["status"] = "images_prepared".into();
    j["previous"] = json!(previous);
    let jobpath = state(c)?
        .join("jobs")
        .join(format!("{}.json", j["id"].as_str().context("job ID")?));
    atomic(&jobpath, j)?;
    remember_previous(c, &key, &sha)?;
    let images = snapshot::images(&exported.files)?;
    let image_proofs = exported
        .proofs
        .iter()
        .filter_map(|(n, p)| {
            n.strip_prefix("images/")
                .map(|n| (n.to_string(), p.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    snapshot::activate(&base, &target, &images, Some(&image_proofs))?;
    j["status"] = "published".into();
    j["finished"] = now().into();
    stage(j, "activation", &mut last);
    j["timings"]["total"] = start.elapsed().as_secs_f64().into();
    atomic(&state(c)?.join("published").join(&key), &manifest)?;
    if let Some(claim) = &exported.claim {
        j["snapshot"] = json!({"prepared":true,"retained_files":claim.retained.len(),"synced_directories":claim.dirty.len()});
    }
    j["incremental"] = json!({"reused_files":exported.reused.len(),"changed_files":exported.files.len()-exported.reused.len(),"object_transfers":stats});
    if snapshot::save(
        c,
        &key,
        &sha,
        &exported.files,
        &exported.oids,
        &exported.proofs,
        &etags,
        &object_sources,
    )
    .is_err()
    {
        j["incremental"]["seal"] = "unavailable".into();
    }
    stage(j, "evidence", &mut last);
    j["finished"] = now().into();
    j["timings"]["total"] = start.elapsed().as_secs_f64().into();
    Ok(())
}
fn stage(j: &mut Value, name: &str, last: &mut Instant) {
    j["timings"][name] = last.elapsed().as_secs_f64().into();
    *last = Instant::now();
}
pub fn record_limit(c: &Value, code: &str, key: &str, category: &str) {
    if !(code == "unsafe_resource_entry" || git::limit(c, code) > 0)
        || !["queue", "git"].contains(&category)
    {
        return;
    }
    let result = (|| -> Result<()> {
        let path = state(c)?.join("resource-limit-events.json");
        let old = read_json(&path, 16384).unwrap_or(json!({}));
        let keys = c["repositories"]
            .as_array()
            .context("repositories")?
            .iter()
            .filter_map(|r| r["key"].as_str())
            .collect::<BTreeSet<_>>();
        let mut value = json!({});
        for name in ["queue", "git"] {
            let v = &old[name];
            if let (Some(oldcode), Some(at)) = (v["code"].as_str(), v["at"].as_f64()) {
                if (oldcode == "unsafe_resource_entry" || git::limit(c, oldcode) > 0)
                    && at.is_finite()
                    && at >= 0.
                    && at <= now() + 300.
                    && (!oldcode.starts_with("git_") || name == "git")
                    && (!oldcode.starts_with("queue_") || name == "queue")
                    && (v["key"].is_null() || v["key"].as_str().is_some_and(|k| keys.contains(k)))
                {
                    value[name] = json!({"code":oldcode,"at":at,"key":v["key"]});
                }
            }
        }
        value[category] =
            json!({"code":code,"at":now(),"key":if keys.contains(key){Some(key)}else{None}});
        atomic(&path, &value)
    })();
    let _ = result;
}
/// Two bounded private observations; public status never includes raw exceptions.
pub fn private_diagnostic(c: &Value, category: &str, phase: &str, error: &anyhow::Error) {
    let result = (|| -> Result<()> {
        ensure!(
            ["publication", "webhook"].contains(&category),
            "invalid diagnostic category"
        );
        let phase = if [
            "request",
            "event",
            "configuration",
            "fetch",
            "export",
            "objects",
            "snapshot",
            "activation",
            "evidence",
        ]
        .contains(&phase)
        {
            phase
        } else {
            "unknown"
        };
        let message = error.to_string();
        let code = if [
            "git_repository_bytes",
            "git_total_bytes",
            "git_fetch_bytes",
            "git_min_free_bytes",
            "git_timeout_seconds",
            "git_output_bytes",
            "scan_entries",
            "queue_active_records",
            "queue_total_records",
            "queue_reserved_bytes",
            "unsafe_resource_entry",
        ]
        .contains(&message.as_str())
        {
            message.as_str()
        } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
            match error.kind() {
                std::io::ErrorKind::NotFound => "file_missing",
                std::io::ErrorKind::PermissionDenied => "permission_denied",
                std::io::ErrorKind::TimedOut => "timed_out",
                std::io::ErrorKind::WouldBlock => "resource_busy",
                _ => "io_failed",
            }
        } else if error.downcast_ref::<reqwest::Error>().is_some() {
            "remote_request_failed"
        } else {
            match message.as_str() {
                "Git command failed" | "Git batch process failed" => "git_failed",
                "signature mismatch" => "signature_refused",
                "unknown repository"
                | "wrong branch"
                | "invalid event"
                | "invalid signed event" => "event_refused",
                _ => "validation_refused",
            }
        };
        let root = state(c)?.join("private-diagnostics");
        fs::create_dir_all(&root)?;
        ensure!(
            fs::symlink_metadata(&root)?.is_dir(),
            "unsafe diagnostic directory"
        );
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        atomic(
            &root.join(format!("{category}.json")),
            &json!({"schema_version":1,"at":now(),"phase":phase,"code":code}),
        )
    })();
    let _ = result;
}
fn publication_phase(j: &Value) -> &'static str {
    for stage in ["fetch", "export", "objects", "snapshot", "activation"] {
        if j["timings"].get(stage).is_none() {
            return stage;
        }
    }
    "evidence"
}
pub fn work_once(c: &Value) -> Result<()> {
    work_once_heartbeat(c, None)
}
fn work_once_heartbeat(c: &Value, heartbeat: Option<&Arc<Mutex<Value>>>) -> Result<()> {
    let _lock = match queue::Lock::acquire(c, "worker", false) {
        Ok(l) => l,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            return Ok(())
        }
        Err(e) => return Err(e),
    };
    let mut ready = Vec::new();
    for path in queue::jobs(c)? {
        match read_job(&path) {
            Ok(j)
                if ["pending", "running", "images_prepared"]
                    .contains(&j["status"].as_str().unwrap_or("")) =>
            {
                ready.push((path, j))
            }
            Ok(_) => {}
            Err(_) => {
                if queue::quarantine(c, &path).is_err() {
                    eprintln!("{{\"event\":\"job-quarantine-failed\",\"error\":\"IOError\"}}");
                }
            }
        }
    }
    ready.sort_by(|(pa, a), (pb, b)| {
        a["created"]
            .as_f64()
            .unwrap_or(0.)
            .total_cmp(&b["created"].as_f64().unwrap_or(0.))
            .then(pa.cmp(pb))
    });
    let idle = ready.is_empty();
    for (path, mut j) in ready {
        j["status"] = "running".into();
        if atomic(&path, &j).is_err() {
            continue;
        }
        if let Some(h) = heartbeat {
            let mut h = h.lock().map_err(|_| anyhow!("heartbeat poisoned"))?;
            h["job_started_at"] = now().into();
            h["progress_at"] = now().into();
        }
        if let Err(error) = publish(c, &mut j) {
            private_diagnostic(c, "publication", publication_phase(&j), &error);
            j["status"] = "failed".into();
            j["error"] = "PublicationError".into();
            let code = error.to_string();
            if [
                "git_repository_bytes",
                "git_total_bytes",
                "git_fetch_bytes",
                "git_min_free_bytes",
                "git_timeout_seconds",
                "git_output_bytes",
                "scan_entries",
            ]
            .contains(&code.as_str())
            {
                {
                    let _queue = queue::Lock::acquire(c, "queue", true);
                    if _queue.is_ok() {
                        record_limit(c, &code, j["key"].as_str().unwrap_or(""), "git");
                    }
                }
                j["resource_limit"] = code.into();
            }
        }
        if atomic(&path, &j).is_err() {
            eprintln!("{{\"event\":\"job-state-write-failed\",\"error\":\"IOError\"}}");
        }
        if let Some(h) = heartbeat {
            let mut h = h.lock().map_err(|_| anyhow!("heartbeat poisoned"))?;
            h["job_started_at"] = Value::Null;
            h["progress_at"] = now().into();
        }
    }
    if idle {
        let _ = prepared::idle(c);
        let _ = prepared::audit(c);
    }
    Ok(())
}
/// Authenticate original webhook bytes before parsing any repository identity.
pub fn admit_event(
    c: &Value,
    body: &[u8],
    signature: &str,
    event: &str,
    delivery: &str,
    secret: &[u8],
) -> Result<String> {
    use hmac::{Hmac, Mac};
    ensure!(
        !body.is_empty() && body.len() <= 1048576 && hex_string(signature, 64),
        "invalid signed event"
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
    mac.update(body);
    mac.verify_slice(&hex::decode(signature)?)
        .map_err(|_| anyhow!("signature mismatch"))?;
    ensure!(event == "push", "invalid event");
    let payload: Value = serde_json::from_slice(body)?;
    let id = payload["repository"]["id"]
        .as_u64()
        .context("repository ID")?;
    let r = c["repositories"]
        .as_array()
        .context("repositories")?
        .iter()
        .find(|r| r["repository_id"].as_u64() == Some(id))
        .context("unknown repository")?;
    ensure!(
        payload["ref"] == format!("refs/heads/{}", r["branch"].as_str().context("branch")?),
        "wrong branch"
    );
    enqueue(
        c,
        r["key"].as_str().context("repository key")?,
        payload["after"].as_str().context("event SHA")?,
        delivery,
        "auto",
    )
}
mod webhook;
fn serve() -> Result<()> {
    webhook::serve()
}
fn worker(c: &Value) -> Result<()> {
    let _ = queue::Lock::acquire(c, "queue", true)?;
    let path = state(c)?.join("jobs");
    fs::create_dir_all(&path)?;
    let wakeup = queue::Wakeup::new(&path);
    let heartbeat = Arc::new(Mutex::new(
        json!({"updated_at":now(),"progress_at":now(),"job_started_at":null}),
    ));
    let h = heartbeat.clone();
    let p = state(c)?.join("worker-heartbeat.json");
    thread::spawn(move || loop {
        if let Ok(mut v) = h.lock() {
            v["updated_at"] = now().into();
            let _ = atomic(&p, &v);
        }
        thread::sleep(Duration::from_secs(10));
    });
    let mut delay = 2u64;
    loop {
        if let Ok(mut v) = heartbeat.lock() {
            v["progress_at"] = now().into();
        }
        let config = match load(None) {
            Ok(c) => c,
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
                eprintln!("{{\"event\":\"queue-io-failed\",\"error\":\"IOError\"}}");
                delay = (delay * 2).min(60);
                thread::sleep(Duration::from_secs(delay));
                continue;
            }
            Err(e) => return Err(e),
        };
        match work_once_heartbeat(&config, Some(&heartbeat)) {
            Ok(()) => delay = 2,
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {
                eprintln!("{{\"event\":\"queue-io-failed\",\"error\":\"IOError\"}}");
                delay = (delay * 2).min(60);
                thread::sleep(Duration::from_secs(delay));
                continue;
            }
            Err(e) => return Err(e),
        }
        wakeup.wait();
    }
}
pub fn render(c: &Value, directory: &Path) -> Result<()> {
    let policy = crate::public_access::Policy::from_config(c)?;
    let mut locations = Vec::new();
    let mut root = false;
    let mount_pattern = regex::Regex::new(r"^/(?:[a-z0-9][a-z0-9-]*/)?$")?;
    for r in c["repositories"].as_array().context("repositories")? {
        if r["serve"] == false {
            continue;
        }
        let mount = r["mount_path"].as_str().context("mount path")?;
        let key = r["key"].as_str().context("repository key")?;
        ensure!(
            valid_key(key) && mount_pattern.is_match(mount),
            "invalid render input"
        );
        let path = format!("/srv/images/repos/{key}/current/");
        if mount == "/" {
            root = true;
            locations.push(format!(
                "location / {{ root {path}; try_files $uri =404; }}"
            ));
        } else {
            locations.push(format!("location {mount} {{ alias {path}; }}"));
        }
    }
    if !root {
        locations.push("location / { return 404; }".into());
    }
    let mut lines = vec![
        "include /etc/nginx/mime.types;".into(),
        "default_type application/octet-stream;".into(),
        "resolver 127.0.0.11 valid=10s ipv6=off;".into(),
        "map $request_method $oa_allow { default \"GET, HEAD\"; GET \"\"; HEAD \"\"; }".into(),
        "limit_conn_zone $hostname zone=oa_public_reader:1m;".into(),
    ];
    if policy.hotlink_enabled {
        lines.push("map $http_referer $oa_referer_allowed { default 0;".into());
        if policy.allow_empty_referer {
            lines.push("\"\" 1;".into());
        }
        for domain in &policy.allowed_referers {
            let pattern = if let Some(suffix) = domain.strip_prefix("*.") {
                format!(r"[a-z0-9-]+(?:\.[a-z0-9-]+)*\.{}", regex::escape(suffix))
            } else {
                regex::escape(domain)
            };
            lines.push(format!(
                r#""~*^https?://{pattern}(?::0*(?:[0-9]{{1,4}}|[0-5][0-9]{{4}}|6[0-4][0-9]{{3}}|65[0-4][0-9]{{2}}|655[0-2][0-9]|6553[0-5]))?(?:[/?#][^[:space:][:cntrl:]\x5c]*)?$" 1;"#
            ));
        }
        lines.push("}".into());
    }
    let proxy = |port| {
        format!("limit_conn oa_public_reader 16; set $oa_reader http://object-reader:{port}; proxy_pass $oa_reader$request_uri; proxy_http_version 1.1; proxy_pass_request_body off; proxy_pass_request_headers off; proxy_set_header Host object-reader; proxy_set_header Content-Length \"\"; proxy_set_header Transfer-Encoding \"\"; proxy_set_header Connection \"\"; proxy_set_header Cookie \"\"; proxy_set_header Authorization \"\"; proxy_set_header Accept-Encoding $http_accept_encoding; proxy_set_header Range $http_range; proxy_set_header If-Range $http_if_range; proxy_set_header If-Match $http_if_match; proxy_set_header If-Unmodified-Since $http_if_unmodified_since; proxy_set_header If-None-Match $http_if_none_match; proxy_set_header If-Modified-Since $http_if_modified_since; proxy_set_header Referer $http_referer; proxy_request_buffering off; proxy_buffering off;")
    };
    for port in [8080, 8084, 8086] {
        lines.push(format!(
            "server {{ listen {port}; server_tokens off; autoindex off; open_file_cache off; gzip off;"
        ));
        lines.push("if ($request_method !~ ^(GET|HEAD)$) { return 405; }".into());
        lines.push("lingering_close off; error_page 405 = @oa_method_not_allowed; location @oa_method_not_allowed { keepalive_timeout 0; lingering_close off; return 405; }".into());
        lines.push("add_header X-Content-Type-Options nosniff always;".into());
        lines.push("add_header Allow $oa_allow always;".into());
        lines.push(format!(
            "add_header Content-Security-Policy \"{}\" always;",
            snapshot::CSP
        ));
        lines.push("add_header Cache-Control \"public, max-age=300, must-revalidate\";".into());
        if port != 8086 && policy.hotlink_enabled {
            lines.push("if ($oa_referer_allowed = 0) { return 403; }".into());
        }
        lines.push(r"location ~ (^|/)\. { deny all; }".into());
        if port == 8084 {
            lines.push(format!("location / {{ {} }}", proxy(8083)));
        } else {
            if port == 8080 {
                lines.push(format!(
                    r"location ~* \.(?:js|html|css|xml|json|shtml|htm)$ {{ {} }}",
                    proxy(8085)
                ));
            }
            lines.extend(locations.clone());
        }
        lines.push("}".into());
    }
    lines.push("server { listen 8087; server_tokens off; keepalive_timeout 0; lingering_close off; return 302 https://$host$request_uri; }".into());
    fs::create_dir_all(directory)?;
    let outputs = [
        ("images.conf", (lines.join("\n") + "\n").into_bytes()),
        ("public-access.json", serde_json::to_vec_pretty(&policy)?),
    ];
    for (name, bytes) in outputs {
        let mut staged = tempfile::NamedTempFile::new_in(directory)?;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        staged.persist(directory.join(name))?;
    }
    snapshot::sync_dir(directory)
}

fn parse_status(args: &[String]) -> Result<(String, Option<String>, Option<String>)> {
    let (mut scope, mut key, mut sha) = (None, None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--key" => {
                i += 1;
                let value = args.get(i).context("missing status key")?;
                ensure!(valid_key(value), "invalid status key");
                key = Some(value.clone());
            }
            "--sha" => {
                i += 1;
                let value = args.get(i).context("missing status SHA")?;
                ensure!(valid_sha(value), "invalid status SHA");
                sha = Some(value.clone());
            }
            value => {
                ensure!(
                    ["active", "all", "archived"].contains(&value) && scope.is_none(),
                    "invalid status scope"
                );
                scope = Some(value.to_string());
            }
        }
        i += 1;
    }
    Ok((scope.unwrap_or("active".into()), key, sha))
}
fn validate_cli(command: &str, args: &[String]) -> Result<()> {
    let valid = match command {
        "serve" | "worker" | "once" => args.is_empty(),
        "render" => args.len() == 1 && !args[0].starts_with('-'),
        "publish" | "rollback-images" => {
            args.len() == 2 && valid_key(&args[0]) && valid_sha(&args[1])
        }
        "retry" => args.len() == 1 && hex_string(&args[0], 64),
        "status" => parse_status(args).is_ok(),
        _ => false,
    };
    if !valid {
        eprintln!("usage: open-assets {command} [valid arguments]; see --help");
        return Err(crate::common::ExitCode(2).into());
    }
    Ok(())
}
pub fn run(command: &str, args: &[String], c: &Value) -> Result<()> {
    if args == ["--help"] || args == ["-h"] {
        println!(
            "Usage: open-assets {command} {}",
            match command {
                "publish" | "rollback-images" => "KEY SHA",
                "retry" => "JOB_ID",
                "status" => "[active|all|archived] [--key KEY] [--sha SHA]",
                "render" => "DESTINATION",
                _ => "",
            }
        );
        return Ok(());
    }
    validate_cli(command, args)?;
    match command {
        "serve" => serve(),
        "worker" => worker(c),
        "once" => work_once(c),
        "render" => {
            ensure!(args.len() == 1, "render requires destination");
            render(c, Path::new(&args[0]))
        }
        "publish" => {
            ensure!(args.len() == 2, "publish requires repository key and SHA");
            println!("{}", enqueue(c, &args[0], &args[1], "manual", "manual")?);
            Ok(())
        }
        "retry" => {
            ensure!(args.len() == 1, "retry requires job ID");
            queue::retry(c, &args[0])
        }
        "rollback-images" => {
            ensure!(
                args.len() == 2,
                "rollback-images requires repository key and SHA"
            );
            let _lock = queue::Lock::acquire(c, "worker", true)?;
            rollback_images_locked(c, &args[0], &args[1])
        }
        "status" => status(c, args),
        _ => bail!("unsupported publisher command"),
    }
}
fn status(c: &Value, args: &[String]) -> Result<()> {
    let (scope, key, sha) = parse_status(args)?;
    let key = key.as_deref();
    let sha = sha.as_deref();
    let mut paths = if scope != "archived" {
        queue::jobs(c)?
    } else {
        Vec::new()
    };
    let archive = state(c)?.join("job-archive");
    if scope != "active" && archive.exists() {
        for folder in fs::read_dir(archive)? {
            let folder = folder?.path();
            ensure!(
                fs::symlink_metadata(&folder)?.is_dir(),
                "unsafe job archive"
            );
            for file in fs::read_dir(folder)? {
                let p = file?.path();
                if p.extension().is_some_and(|e| e == "json") {
                    paths.push(p)
                }
            }
        }
    }
    for p in paths {
        if let Ok(j) = read_job(&p) {
            if key.is_some_and(|k| j["key"] != k) || sha.is_some_and(|s| j["sha"] != s) {
                continue;
            }
            let k = j["key"].as_str().context("job key")?;
            let image = match current_sha(c, k) {
                Ok(s) => match read_json(&state(c)?.join("published").join(k), 8 << 20) {
                    Ok(m) if m["sha"] == s => {
                        if j["sha"] == s {
                            "current"
                        } else {
                            "other-version"
                        }
                    }
                    _ => "unknown",
                },
                Err(_) => "unavailable",
            };
            println!(
                "{} {} {} {} images={}",
                j["id"].as_str().unwrap_or(""),
                k,
                j["sha"].as_str().unwrap_or(""),
                j["status"].as_str().unwrap_or(""),
                image
            );
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commit_validation() {
        assert!(valid_sha(&"a".repeat(40)));
        assert!(!valid_sha(&"0".repeat(40)));
        assert!(!valid_sha(&"A".repeat(40)));
    }
    #[test]
    fn queue_dedupe_keeps_failure() {
        let t = tempfile::tempdir().unwrap();
        let c = json!({"state_root":t.path(),"revision":"fixture","repositories":[{"key":"images","enabled":true}]});
        let id = enqueue(&c, "images", &"a".repeat(40), "one", "auto").unwrap();
        let p = t.path().join("jobs").join(format!("{id}.json"));
        let mut j = read_job(&p).unwrap();
        j["status"] = "failed".into();
        atomic(&p, &j).unwrap();
        assert_eq!(
            id,
            enqueue(&c, "images", &"a".repeat(40), "two", "auto").unwrap()
        );
        assert_eq!(read_job(&p).unwrap()["status"], "failed");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::process::Command;
    struct Fixture {
        _tmp: tempfile::TempDir,
        c: Value,
        source: PathBuf,
        g: PathBuf,
        sha: String,
    }
    fn git_at(path: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir(&source).unwrap();
        git_at(&source, &["init", "-q", "-b", "main"]);
        git_at(&source, &["config", "user.name", "Fixture"]);
        git_at(
            &source,
            &["config", "user.email", "fixture@localhost.invalid"],
        );
        git_at(&source, &["config", "commit.gpgsign", "false"]);
        fs::create_dir_all(source.join("public/images")).unwrap();
        fs::write(source.join("public/images/1.png"), b"one").unwrap();
        fs::write(source.join("public/images/2.png"), b"two").unwrap();
        fs::write(
            source.join("public/images/date.svg"),
            br##"<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0"/></svg>"##,
        )
        .unwrap();
        git_at(&source, &["add", "."]);
        git_at(&source, &["commit", "-qm", "fixture"]);
        let c = json!({"schema_version":1,"revision":"fixture","state_root":tmp.path().join("state"),"images_root":tmp.path().join("images"),"publication":{"reference_releases":true,"object_workers":2},"resource_limits":{"git_min_free_bytes":1},"repositories":[{"key":"images","enabled":true,"repository_id":1,"source_directory":"public","mount_path":"/","remote":source,"branch":"main","max_bytes":1048576}]});
        let g = tmp.path().join("state/git/images");
        let sha = git::fetch(&c, &c["repositories"][0], &g).unwrap();
        Fixture {
            _tmp: tmp,
            c,
            source,
            g,
            sha,
        }
    }
    fn install(f: &Fixture) -> (Vec<Value>, BTreeMap<String, String>) {
        let b = base(&f.c, "images").unwrap();
        let full = b.join("releases").join(&f.sha);
        fs::create_dir_all(&full).unwrap();
        let files = export(&f.c, &f.c["repositories"][0], &f.g, &f.sha, &full).unwrap();
        let oids = git::plan(&f.c, &f.c["repositories"][0], &f.g, &f.sha)
            .unwrap()
            .into_iter()
            .map(|b| (b.relative, b.oid))
            .collect::<BTreeMap<_, _>>();
        let proofs = files
            .iter()
            .map(|item| {
                let n = item["path"].as_str().unwrap();
                (
                    n.to_string(),
                    snapshot::proof(&full.join(n), item, now()).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let etags = files
            .iter()
            .map(|item| {
                (
                    item["path"].as_str().unwrap().to_string(),
                    json!({"etag":format!("\"{}\"","a".repeat(32)),"modified":1700000000}),
                )
            })
            .collect();
        let manifest = json!({"key":"images","sha":f.sha,"files":files,"config":"fixture","object_layout":"references-v1","object_sources":{},"image_layout":"snapshot-v1"});
        atomic(
            &state(&f.c)
                .unwrap()
                .join("manifests/images")
                .join(format!("{}.json", f.sha)),
            &manifest,
        )
        .unwrap();
        atomic(&state(&f.c).unwrap().join("published/images"), &manifest).unwrap();
        snapshot::switch(&b, &format!("{}/images", f.sha)).unwrap();
        snapshot::save(
            &f.c,
            "images",
            &f.sha,
            &files,
            &oids,
            &proofs,
            &etags,
            &json!({}),
        )
        .unwrap();
        (files, oids)
    }
    #[test]
    fn real_git_batch_export_and_preflight() {
        let f = fixture();
        let dest = f._tmp.path().join("export");
        let files = export(&f.c, &f.c["repositories"][0], &f.g, &f.sha, &dest).unwrap();
        assert_eq!(files.len(), 3);
        verify_tree(&dest, &files).unwrap();
        assert!(!f.g.join("FETCH_HEAD").exists());
        let mut c = f.c.clone();
        c["repositories"][0]["max_files"] = 1.into();
        let failed = f._tmp.path().join("failed");
        assert!(export(&c, &c["repositories"][0], &f.g, &f.sha, &failed).is_err());
        assert!(!failed.exists());
    }
    #[test]
    fn prepared_view_retains_unchanged_inodes_and_atomic_rollback() {
        let f = fixture();
        let (files, _) = install(&f);
        prepared::idle(&f.c).unwrap();
        let record =
            read_json(&state(&f.c).unwrap().join("prepared/images.json"), 8 << 20).unwrap();
        assert_eq!(record["status"], "ready");
        let old = base(&f.c, "images").unwrap().join("releases").join(&f.sha);
        let before = snapshot::fingerprint(&old.join("images/2.png")).unwrap();
        fs::write(f.source.join("public/images/1.png"), b"changed").unwrap();
        git_at(&f.source, &["add", "."]);
        git_at(&f.source, &["commit", "-qm", "update one"]);
        let sha = git::fetch(&f.c, &f.c["repositories"][0], &f.g).unwrap();
        let mut sources = snapshot::sources(&f.c, &f.c["repositories"][0]).unwrap();
        assert_eq!(sources.len(), 3);
        let sizes = sources
            .iter()
            .map(|(oid, s)| (oid.clone(), s.item["size"].as_u64().unwrap()))
            .collect();
        let blobs = git::plan_cached(&f.c, &f.c["repositories"][0], &f.g, &sha, &sizes).unwrap();
        let tmp = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(base(&f.c, "images").unwrap())
            .unwrap();
        let claim = prepared::claim(
            &f.c,
            &f.c["repositories"][0],
            &sha,
            tmp.path(),
            &blobs,
            &mut sources,
        )
        .unwrap()
        .unwrap();
        assert_eq!(claim.retained.len(), 2);
        let exported =
            export_plan(&f.c, &f.g, &blobs, tmp.path(), &mut sources, Some(claim)).unwrap();
        assert_eq!(exported.reused.len(), 2);
        let after = snapshot::fingerprint(&tmp.path().join("images/2.png")).unwrap();
        assert_eq!(before[0], after[0]);
        assert_eq!(before[1], after[1]);
        snapshot::verify(tmp.path(), &exported.files, Some(&exported.proofs)).unwrap();
        rollback_images_locked(&f.c, "images", &f.sha).unwrap();
        assert_eq!(current_sha(&f.c, "images").unwrap(), f.sha);
        assert_eq!(
            read_json(&state(&f.c).unwrap().join("published/images"), 8 << 20).unwrap()["files"],
            Value::Array(files)
        );
    }
    #[test]
    fn unsafe_git_entry_cannot_write_staging() {
        let f = fixture();
        std::os::unix::fs::symlink("/etc/passwd", f.source.join("public/images/link.png")).unwrap();
        git_at(&f.source, &["add", "."]);
        git_at(&f.source, &["commit", "-qm", "unsafe"]);
        let sha = git::fetch(&f.c, &f.c["repositories"][0], &f.g).unwrap();
        let dest = f._tmp.path().join("export");
        assert!(export(&f.c, &f.c["repositories"][0], &f.g, &sha, &dest).is_err());
        assert!(!dest.exists());
    }
    #[test]
    fn active_pointer_does_not_move_for_corrupt_rollback() {
        let f = fixture();
        install(&f);
        let full = base(&f.c, "images").unwrap().join("releases").join(&f.sha);
        use std::os::unix::fs::PermissionsExt;
        let p = full.join("images/1.png");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(p, b"bad").unwrap();
        let before = fs::read_link(base(&f.c, "images").unwrap().join("current")).unwrap();
        assert!(rollback_images_locked(&f.c, "images", &f.sha).is_err());
        assert_eq!(
            fs::read_link(base(&f.c, "images").unwrap().join("current")).unwrap(),
            before
        );
    }
}

#[cfg(test)]
mod parallel_link_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    #[test]
    fn inode_aliases_stay_serial_and_keep_original_proof_age() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("original.png");
        fs::write(&source, b"immutable bytes").unwrap();
        let size = fs::metadata(&source).unwrap().len();
        let item = json!({"path":"images/original.png","size":size,"sha256":snapshot::hash_file(&source,size).unwrap(),"content_type":"image/png"});
        let at = now() - 100.;
        let proof = snapshot::proof(&source, &item, at).unwrap();
        let oid = "a".repeat(40);
        let mut sources = BTreeMap::from([(
            oid.clone(),
            snapshot::Source {
                item: item.clone(),
                path: source.clone(),
                oid: oid.clone(),
                object: json!({}),
                etag: Value::Null,
                modified: Value::Null,
                proof,
            },
        )]);
        let blobs = (0..96)
            .map(|n| git::Blob {
                oid: oid.clone(),
                relative: format!("images/{n}.png"),
                original: format!("public/images/{n}.png"),
                size,
            })
            .collect::<Vec<_>>();
        let dest = t.path().join("export");
        fs::create_dir(&dest).unwrap();
        let c = json!({"publication":{"object_workers":4}});
        let result =
            export_plan(&c, Path::new("unused"), &blobs, &dest, &mut sources, None).unwrap();
        assert_eq!(result.reused.len(), 96);
        snapshot::verify(&dest, &result.files, Some(&result.proofs)).unwrap();
        let current = fs::metadata(&source).unwrap();
        assert_eq!(current.nlink(), 97);
        for p in result.proofs.values() {
            assert_eq!(p["verified_at"], at);
            assert_eq!(p["fingerprint"][1], current.ino());
        }
    }
}

#[cfg(test)]
mod webhook_regression {
    use super::*;
    use hmac::{Hmac, Mac};
    fn signed(body: &[u8]) -> String {
        let mut m = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        m.update(body);
        hex::encode(m.finalize().into_bytes())
    }
    #[test]
    fn signed_original_bytes_and_numeric_allowlist_control_admission() {
        let t = tempfile::tempdir().unwrap();
        let c = json!({"state_root":t.path(),"revision":"fixture","repositories":[{"key":"images","enabled":true,"repository_id":7,"branch":"main"}]});
        let body=serde_json::to_vec(&json!({"repository":{"id":7,"clone_url":"https://attacker.invalid/repository"},"after":"a".repeat(40),"ref":"refs/heads/main"})).unwrap();
        let signature = signed(&body);
        let id = admit_event(&c, &body, &signature, "push", "delivery", b"secret").unwrap();
        let path = t.path().join("jobs").join(format!("{id}.json"));
        assert!(path.exists());
        let mut altered = body.clone();
        altered.push(b' ');
        assert!(admit_event(&c, &altered, &signature, "push", "delivery", b"secret").is_err());
        assert!(admit_event(&c, &body, &signature, "pull_request", "delivery", b"secret").is_err());
        let other = serde_json::to_vec(
            &json!({"repository":{"id":8},"after":"a".repeat(40),"ref":"refs/heads/main"}),
        )
        .unwrap();
        assert!(admit_event(&c, &other, &signed(&other), "push", "delivery", b"secret").is_err());
        let branch = serde_json::to_vec(
            &json!({"repository":{"id":7},"after":"a".repeat(40),"ref":"refs/heads/other"}),
        )
        .unwrap();
        assert!(admit_event(&c, &branch, &signed(&branch), "push", "delivery", b"secret").is_err());
    }
}

#[cfg(test)]
mod additional_safety_tests {
    use super::*;
    #[test]
    fn concurrent_admission_cannot_exceed_budget() {
        let t = tempfile::tempdir().unwrap();
        let c = json!({"state_root":t.path(),"revision":"fixture","resource_limits":{"queue_active_records":3,"queue_total_records":3},"repositories":[{"key":"images","enabled":true}]});
        let accepted = thread::scope(|scope| {
            let mut jobs = Vec::new();
            for n in 1..17 {
                let c = &c;
                jobs.push(scope.spawn(move || {
                    enqueue(c, "images", &format!("{n:040x}"), "concurrent", "auto").is_ok()
                }));
            }
            jobs.into_iter()
                .map(|h| h.join().unwrap())
                .filter(|accepted| *accepted)
                .count()
        });
        assert_eq!(accepted, 3);
        assert_eq!(queue::jobs(&c).unwrap().len(), 3);
    }
    #[test]
    fn prepared_aliases_admit_current_inode_provenance() {
        let t = tempfile::tempdir().unwrap();
        let sha = "b".repeat(40);
        let oid = "a".repeat(40);
        let c = json!({"state_root":t.path().join("state"),"images_root":t.path().join("images"),"revision":"fixture","publication":{"reference_releases":true,"object_workers":2},"repositories":[{"key":"images","enabled":true}]});
        let base = base(&c, "images").unwrap();
        let full = base.join("releases").join(&sha);
        fs::create_dir_all(full.join("images")).unwrap();
        let mut files = Vec::new();
        let mut oids = BTreeMap::new();
        let mut proofs = BTreeMap::new();
        let mut etags = BTreeMap::new();
        for name in ["images/a.png", "images/b.png"] {
            fs::write(full.join(name), b"same bytes").unwrap();
            let item = json!({"path":name,"size":10,"sha256":snapshot::hash_file(&full.join(name),10).unwrap(),"content_type":"image/png"});
            proofs.insert(
                name.to_string(),
                snapshot::proof(&full.join(name), &item, now()).unwrap(),
            );
            oids.insert(name.to_string(), oid.clone());
            etags.insert(
                name.to_string(),
                json!({"etag":format!("\"{}\"","a".repeat(32)),"modified":1700000000}),
            );
            files.push(item);
        }
        let manifest = json!({"key":"images","sha":sha,"config":"fixture","files":files,"object_layout":"references-v1","object_sources":{},"image_layout":"snapshot-v1"});
        atomic(&state(&c).unwrap().join("published/images"), &manifest).unwrap();
        snapshot::switch(&base, &format!("{sha}/images")).unwrap();
        snapshot::save(
            &c,
            "images",
            &sha,
            &files,
            &oids,
            &proofs,
            &etags,
            &json!({}),
        )
        .unwrap();
        prepared::idle(&c).unwrap();
        let mut sources = snapshot::sources(&c, &c["repositories"][0]).unwrap();
        assert_eq!(sources.len(), 1);
        let blobs = files
            .iter()
            .map(|i| git::Blob {
                oid: oid.clone(),
                relative: i["path"].as_str().unwrap().into(),
                original: format!("public/{}", i["path"].as_str().unwrap()),
                size: 10,
            })
            .collect::<Vec<_>>();
        let dest = tempfile::tempdir_in(&base).unwrap();
        let claim = prepared::claim(
            &c,
            &c["repositories"][0],
            &"c".repeat(40),
            dest.path(),
            &blobs,
            &mut sources,
        )
        .unwrap()
        .unwrap();
        assert_eq!(claim.retained.len(), 2);
        let exported = export_plan(
            &c,
            Path::new("unused"),
            &blobs,
            dest.path(),
            &mut sources,
            Some(claim),
        )
        .unwrap();
        snapshot::verify(dest.path(), &exported.files, Some(&exported.proofs)).unwrap();
    }
}

#[cfg(test)]
mod private_diagnostic_tests {
    use super::*;
    #[test]
    fn diagnostics_are_bounded_private_and_never_store_exception_text() {
        let t = tempfile::tempdir().unwrap();
        let c = json!({"state_root":t.path()});
        let secret = "https://credential:secret@example.invalid/private?token=TOKEN";
        private_diagnostic(&c, "publication", "export", &anyhow!(secret));
        let p = t.path().join("private-diagnostics/publication.json");
        let raw = fs::read_to_string(&p).unwrap();
        assert!(!raw.contains(secret) && !raw.contains("TOKEN"));
        assert_eq!(read_json(&p, 4096).unwrap()["code"], "validation_refused");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(t.path().join("private-diagnostics"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[cfg(test)]
pub(crate) mod proof_reuse_fixtures {
    use super::*;
    use sha2::Digest;
    pub struct Fixture {
        pub _temporary: tempfile::TempDir,
        pub c: Value,
        pub full: PathBuf,
        pub files: Vec<Value>,
        pub manifest: Value,
        pub seal_path: PathBuf,
        pub oids: BTreeMap<String, String>,
    }
    impl Fixture {
        pub fn new(count: usize, aliases: bool) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let c = json!({"state_root":temporary.path().join("state"),"images_root":temporary.path().join("images"),"revision":"proof-reuse-fixture","publication":{"reference_releases":true,"object_workers":4},"repositories":[{"key":"images","enabled":true,"max_bytes":10485760}]});
            let sha = "a".repeat(40);
            let full = base(&c, "images").unwrap().join("releases").join(&sha);
            fs::create_dir_all(full.join("images")).unwrap();
            let mut files = Vec::new();
            let mut oids = BTreeMap::new();
            let mut etags = BTreeMap::new();
            let mut objects = serde_json::Map::new();
            for n in 0..count {
                let name = format!("images/{n:04}.png");
                let body =
                    format!("proof fixture bytes {}", if aliases { 0 } else { n }).into_bytes();
                if aliases && n > 0 {
                    fs::hard_link(full.join("images/0000.png"), full.join(&name)).unwrap();
                } else {
                    fs::write(full.join(&name), &body).unwrap();
                }
                let item = json!({"path":name,"size":body.len(),"sha256":hex::encode(sha2::Sha256::digest(&body)),"content_type":"image/png"});
                let git = std::process::Command::new("git")
                    .args(["hash-object", "--"])
                    .arg(full.join(&name))
                    .output()
                    .unwrap();
                assert!(git.status.success());
                oids.insert(
                    name.clone(),
                    String::from_utf8(git.stdout).unwrap().trim().to_string(),
                );
                etags.insert(
                    name.clone(),
                    json!({"etag":format!("\"{:x}\"",md5::compute(&body)),"modified":1700000000}),
                );
                objects.insert(name.clone(),json!({"key":format!("repositories/images/releases/{sha}/{name}"),"content_type":"image/png"}));
                files.push(item);
            }
            let mut proofs = BTreeMap::new();
            for item in &files {
                let name = item["path"].as_str().unwrap();
                proofs.insert(
                    name.to_string(),
                    snapshot::proof(&full.join(name), item, now() - 20.).unwrap(),
                );
            }
            let manifest = json!({"key":"images","sha":sha,"files":files,"config":c["revision"],"object_layout":"references-v1","object_sources":objects,"image_layout":"snapshot-v1"});
            atomic(&state(&c).unwrap().join("published/images"), &manifest).unwrap();
            snapshot::switch(&base(&c, "images").unwrap(), &format!("{sha}/images")).unwrap();
            snapshot::save(
                &c,
                "images",
                &sha,
                &files,
                &oids,
                &proofs,
                &etags,
                &manifest["object_sources"],
            )
            .unwrap();
            let seal_path = state(&c).unwrap().join("incremental/images.json");
            Self {
                _temporary: temporary,
                c,
                full,
                files,
                manifest,
                seal_path,
                oids,
            }
        }
        pub fn seal(&self) -> Value {
            read_json(&self.seal_path, 8 << 20).unwrap()
        }
        pub fn age(&self, names: &[usize], seconds: f64) {
            let mut seal = self.seal();
            for n in names {
                seal["records"][format!("images/{n:04}.png")]["immutable"]["verified_at"] =
                    (now() - seconds).into();
            }
            atomic(&self.seal_path, &seal).unwrap();
        }
        pub fn repository(&self) -> &Value {
            &self.c["repositories"][0]
        }
        pub fn corrupt(&self, index: usize) {
            use std::os::unix::fs::PermissionsExt;
            let name = format!("images/{index:04}.png");
            let p = self.full.join(&name);
            let size = fs::metadata(&p).unwrap().len();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(&p, vec![b'X'; size as usize]).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        }
    }
}

#[cfg(test)]
mod public_render_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    #[test]
    fn public_servers_keep_policy_and_internal_static_origin_separate() {
        let temp = tempfile::tempdir().unwrap();
        let c = json!({"repositories":[{"key":"images","mount_path":"/"},{"key":"other","mount_path":"/other/"}],"public_access":{"hotlink_enabled":true,"allowed_referers":["example.com","*.example.org"]}});
        render(&c, temp.path()).unwrap();
        let text = fs::read_to_string(temp.path().join("images.conf")).unwrap();
        assert_eq!(text.matches("return 405;").count(), 6);
        assert!(text.contains("proxy_set_header Transfer-Encoding \"\";"));
        let redirect = text.split("server { listen 8087;").nth(1).unwrap();
        assert!(redirect.contains("return 302 https://$host$request_uri;"));
        assert!(redirect.contains("keepalive_timeout 0; lingering_close off;"));
        assert!(!redirect.contains("request_method"));
        assert!(!redirect.contains("nosniff"));
        assert!(text.contains("if ($request_method !~ ^(GET|HEAD)$)"));
        assert!(text.contains("proxy_set_header If-Match $http_if_match;"));
        assert!(text.contains("proxy_set_header If-Unmodified-Since $http_if_unmodified_since;"));
        assert_eq!(text.matches("if ($oa_referer_allowed = 0)").count(), 2);
        assert!(text.contains("resolver 127.0.0.11"));
        assert!(text.contains("(?:js|html|css|xml|json|shtml|htm)$"));
        assert!(text.contains("proxy_pass_request_body off"));
        assert!(text.contains("proxy_pass_request_headers off"));
        assert!(text.contains("alias /srv/images/repos/other/current/"));
        let internal = text.split("server { listen 8086;").nth(1).unwrap();
        assert!(!internal.contains("proxy_pass"));
        assert!(!internal.contains("invalid_referer"));
        assert!(!text.contains("Strict-Transport-Security"));
        for name in ["images.conf", "public-access.json"] {
            assert_eq!(
                fs::metadata(temp.path().join(name)).unwrap().mode() & 0o777,
                0o644
            );
        }
        assert_eq!(
            crate::public_access::Policy::load(&temp.path().join("public-access.json"))
                .unwrap()
                .qps,
            100
        );
    }
    #[test]
    fn bad_render_input_cannot_replace_valid_generated_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let mut c = json!({"repositories":[{"key":"images","mount_path":"/"}]});
        render(&c, temp.path()).unwrap();
        let original = fs::read(temp.path().join("images.conf")).unwrap();
        c["public_access"] = json!({"allowed_referers":["evil;include /secret;"]});
        assert!(render(&c, temp.path()).is_err());
        assert_eq!(fs::read(temp.path().join("images.conf")).unwrap(), original);
    }
}
