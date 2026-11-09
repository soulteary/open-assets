//! Checkpointed private hard-link views. Never serves or writes through shared bytes.
use super::{git, queue, snapshot};
use crate::common::{atomic, now, read_json};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct Claim {
    pub retained: BTreeSet<String>,
    pub dirty: BTreeSet<PathBuf>,
}
fn checkpoint(c: &Value, key: &str) -> Result<PathBuf> {
    Ok(Path::new(c["state_root"].as_str().context("state_root")?)
        .join("prepared")
        .join(format!("{key}.json")))
}
fn view(c: &Value, key: &str, record: &Value) -> Result<PathBuf> {
    let name = record["directory"].as_str().context("prepared directory")?;
    ensure!(
        regex::Regex::new(r"^\.staging-prefetch-[a-f0-9]{32}$")?.is_match(name),
        "unsafe prepared directory"
    );
    Ok(super::base(c, key)?.join(name))
}
fn seal_paths(c: &Value, key: &str) -> Result<(Value, Value, PathBuf)> {
    let state = super::state(c)?;
    let manifest = read_json(&state.join("published").join(key), 8 << 20)?;
    let p = state.join("incremental").join(format!("{key}.json"));
    let seal = read_json(&p, 8 << 20)?;
    ensure!(
        seal["version"] == 3
            && seal["sha"] == manifest["sha"]
            && seal["config"] == c["revision"]
            && seal["manifest"]
                == crate::common::digest(
                    &json!({"files":manifest["files"],"object_sources":manifest.get("object_sources").cloned().unwrap_or(json!({}))})
                )?,
        "prepared baseline mismatch"
    );
    Ok((manifest, seal, p))
}
fn proofs(seal: &Value) -> Result<BTreeMap<String, Value>> {
    Ok(seal["records"]
        .as_object()
        .context("seal records")?
        .iter()
        .map(|(n, r)| (n.clone(), r["immutable"].clone()))
        .collect())
}
fn refresh(seal: &mut Value, full: &Path, changed: &BTreeMap<(u64, u64), Value>) -> Result<()> {
    for (name, record) in seal["records"].as_object_mut().context("seal records")? {
        let old = &record["immutable"];
        let id = (
            old["fingerprint"][0].as_u64().context("proof dev")?,
            old["fingerprint"][1].as_u64().context("proof inode")?,
        );
        if let Some(proof) = changed.get(&id) {
            ensure!(
                proof["sha256"] == old["sha256"] && proof["size"] == old["size"],
                "prepared inode collision"
            );
            let fp = snapshot::fingerprint(&full.join(name))?;
            ensure!(
                fp == proof["fingerprint"],
                "prepared shared identity mismatch"
            );
            record["immutable"]["fingerprint"] = fp.clone();
            record["fingerprint"] =
                Value::Array(fp.as_array().context("proof fingerprint")?[..5].to_vec());
        }
    }
    Ok(())
}
fn dirty(set: &mut BTreeSet<PathBuf>, dest: &Path, path: &Path) -> Result<()> {
    let mut p = path;
    loop {
        set.insert(p.strip_prefix(dest)?.to_path_buf());
        if p == dest {
            break;
        }
        p = p.parent().context("dirty parent")?;
    }
    Ok(())
}
fn waiting(c: &Value) -> Result<bool> {
    for p in queue::jobs(c)? {
        let j = queue::read_job(&p)?;
        if ["pending", "running", "images_prepared"].contains(&j["status"].as_str().unwrap_or("")) {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn claim(
    c: &Value,
    r: &Value,
    sha: &str,
    dest: &Path,
    blobs: &[git::Blob],
    sources: &mut BTreeMap<String, snapshot::Source>,
) -> Result<Option<Claim>> {
    if !super::refs(c) {
        return Ok(None);
    }
    let key = r["key"].as_str().context("key")?;
    let p = checkpoint(c, key)?;
    let record = match read_json(&p, 8 << 20) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if record["schema_version"] != 1 || record["status"] != "ready" || record["sha"] == sha {
        return Ok(None);
    }
    let outcome = (|| -> Result<(PathBuf, Value, Value, BTreeMap<String, Value>)> {
        let root = view(c, key, &record)?;
        let (manifest, seal, _) = seal_paths(c, key)?;
        ensure!(
            record["sha"] == manifest["sha"]
                && record["config"] == c["revision"]
                && record["manifest"] == seal["manifest"]
                && record["files"] == manifest["files"],
            "prepared provenance mismatch"
        );
        let oids = record["oids"].as_object().context("prepared OIDs")?;
        let records = seal["records"].as_object().context("seal records")?;
        ensure!(
            oids.len() == records.len()
                && record["proofs"]
                    .as_object()
                    .is_some_and(|p| p.len() == records.len()),
            "prepared proof set mismatch"
        );
        for (n, r) in records {
            ensure!(
                oids.get(n) == Some(&r["oid"]),
                "prepared object identity mismatch"
            );
        }
        let live = proofs(&seal)?
            .into_values()
            .map(|p| {
                Ok((
                    (
                        p["fingerprint"][0].as_u64().context("proof dev")?,
                        p["fingerprint"][1].as_u64().context("proof inode")?,
                    ),
                    p,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut view_proofs = BTreeMap::new();
        for (name, old) in record["proofs"].as_object().context("prepared proofs")? {
            let id = (
                old["fingerprint"][0].as_u64().context("proof dev")?,
                old["fingerprint"][1].as_u64().context("proof inode")?,
            );
            let fresh = live.get(&id).context("missing current inode provenance")?;
            ensure!(
                old["sha256"] == fresh["sha256"] && old["size"] == fresh["size"],
                "prepared proof identity mismatch"
            );
            view_proofs.insert(name.clone(), fresh.clone());
        }
        snapshot::verify(
            &root,
            manifest["files"].as_array().context("files")?,
            Some(&view_proofs),
        )?;
        Ok((root, manifest, seal, view_proofs))
    })();
    let (root, _manifest, _seal, _proofs) = match outcome {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let wanted = blobs
        .iter()
        .map(|b| (b.relative.clone(), b.oid.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut updated = record.clone();
    updated["status"] = "claimed".into();
    atomic(&p, &updated)?;
    fs::rename(&root, dest)?;
    snapshot::sync_dir(dest.parent().context("view parent")?)?;
    let mut claim = Claim::default();
    claim.dirty.insert(PathBuf::new());
    for (name, oid) in record["oids"].as_object().context("prepared OIDs")? {
        let oid = oid.as_str().context("prepared OID")?;
        if wanted.get(name).is_some_and(|s| s == oid) {
            claim.retained.insert(name.clone());
            continue;
        }
        let source = sources.get_mut(oid).context("missing prepared source")?;
        let path = dest.join(name);
        ensure!(
            snapshot::matches(&path, &source.item, &source.proof, false)
                && snapshot::fingerprint(&source.path)? == snapshot::fingerprint(&path)?,
            "prepared unlink identity mismatch"
        );
        let before = fs::symlink_metadata(&source.path)?;
        fs::remove_file(&path)?;
        let after = fs::symlink_metadata(&source.path)?;
        ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.size() == after.size()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.mode() == after.mode()
                && before.nlink() == after.nlink() + 1,
            "prepared source changed while unlinking"
        );
        source.proof["fingerprint"] = snapshot::fingerprint(&source.path)?;
        let mut parent = path.parent().context("view parent")?.to_path_buf();
        dirty(&mut claim.dirty, dest, &parent)?;
        while parent != dest {
            if fs::remove_dir(&parent).is_err() {
                break;
            }
            claim.dirty.remove(parent.strip_prefix(dest)?);
            parent = parent.parent().context("view parent")?.to_path_buf();
            dirty(&mut claim.dirty, dest, &parent)?;
        }
    }
    for blob in blobs {
        if !claim.retained.contains(&blob.relative) {
            dirty(
                &mut claim.dirty,
                dest,
                dest.join(&blob.relative).parent().context("view parent")?,
            )?;
        }
    }
    Ok(Some(claim))
}
fn discard(c: &Value, key: &str, record: &Value) -> Result<()> {
    let root = view(c, key, record)?;
    let (manifest, mut seal, p) = seal_paths(c, key)?;
    let full = super::base(c, key)?
        .join("releases")
        .join(manifest["sha"].as_str().context("manifest SHA")?);
    let files = manifest["files"].as_array().context("files")?;
    snapshot::verify(&full, files, Some(&proofs(&seal)?))?;
    let mut tracked = BTreeMap::new();
    for item in files {
        let name = item["path"].as_str().context("path")?;
        let source = full.join(name);
        let m = fs::symlink_metadata(&source)?;
        tracked.entry((m.dev(), m.ino())).or_insert((
            source,
            seal["records"][name]["immutable"].clone(),
            m.nlink(),
            0u64,
        ));
    }
    let mut stack = vec![root.clone()];
    let mut count = 0;
    while let Some(dir) = stack.pop() {
        ensure!(
            fs::symlink_metadata(&dir)?.is_dir(),
            "unsafe prepared cleanup directory"
        );
        for e in fs::read_dir(dir)? {
            count += 1;
            ensure!(count <= 200000, "prepared cleanup entry budget");
            let path = e?.path();
            let m = fs::symlink_metadata(&path)?;
            if m.is_dir() {
                stack.push(path)
            } else if let Some((_, proof, _, n)) = tracked.get_mut(&(m.dev(), m.ino())) {
                ensure!(
                    snapshot::matches(&path, proof, proof, false),
                    "prepared cleanup identity changed"
                );
                *n += 1;
            }
        }
    }
    fs::remove_dir_all(&root)?;
    let mut changed = BTreeMap::new();
    for (id, (source, mut proof, before, count)) in tracked {
        if count == 0 {
            continue;
        }
        let m = fs::symlink_metadata(&source)?;
        let old = proof["fingerprint"].as_array().context("proof")?;
        let fp = snapshot::fingerprint(&source)?;
        let cur = fp.as_array().context("fingerprint")?;
        ensure!(
            cur[..4] == old[..4] && cur[5..] == old[5..] && m.nlink() + count == before,
            "prepared cleanup source changed"
        );
        proof["fingerprint"] = fp;
        changed.insert(id, proof);
    }
    refresh(&mut seal, &full, &changed)?;
    snapshot::verify(&full, files, Some(&proofs(&seal)?))?;
    atomic(&p, &seal)?;
    fs::remove_file(checkpoint(c, key)?)?;
    snapshot::sync_dir(root.parent().context("prepared parent")?)?;
    Ok(())
}
pub fn idle(c: &Value) -> Result<()> {
    if !super::refs(c) {
        return Ok(());
    }
    for r in c["repositories"].as_array().context("repositories")? {
        if waiting(c)? {
            return Ok(());
        }
        if r["enabled"] != true {
            continue;
        }
        let _ = prepare_one(c, r);
    }
    Ok(())
}
fn prepare_one(c: &Value, r: &Value) -> Result<()> {
    let key = r["key"].as_str().context("key")?;
    let checkpoint = checkpoint(c, key)?;
    if fs::symlink_metadata(&checkpoint).is_ok() {
        let old = read_json(&checkpoint, 8 << 20)?;
        let root = view(c, key, &old)?;
        match fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::remove_file(&checkpoint)?;
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {
                let current = read_json(&super::state(c)?.join("published").join(key), 8 << 20)?;
                if old["sha"] != current["sha"] {
                    discard(c, key, &old)?
                } else {
                    return Ok(());
                }
            }
        }
    }
    let (manifest, mut seal, seal_path) = seal_paths(c, key)?;
    if manifest["image_layout"] != "snapshot-v1" {
        return Ok(());
    }
    let mut sources = snapshot::sources(c, r)?;
    let files = manifest["files"].as_array().context("manifest files")?;
    ensure!(!sources.is_empty(), "missing prepared source");
    for item in files {
        let name = item["path"].as_str().context("path")?;
        ensure!(
            sources.contains_key(seal["records"][name]["oid"].as_str().context("OID")?),
            "incomplete prepared provenance"
        );
    }
    if waiting(c)? {
        return Ok(());
    }
    let name = format!(".staging-prefetch-{}", uuid::Uuid::new_v4().simple());
    let root = super::base(c, key)?.join(&name);
    let mut record = json!({"schema_version":1,"status":"building","directory":name,"sha":manifest["sha"],"config":c["revision"],"manifest":seal["manifest"]});
    atomic(&checkpoint, &record)?;
    fs::create_dir(&root)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    let start = Instant::now();
    let mut oids = serde_json::Map::new();
    let mut updated_proofs = BTreeMap::new();
    let mut changed = BTreeMap::new();
    for item in files {
        ensure!(
            start.elapsed() < Duration::from_secs(5),
            "prepared snapshot time budget exceeded"
        );
        let path = item["path"].as_str().context("path")?;
        let oid = seal["records"][path]["oid"].as_str().context("OID")?;
        let source = sources.get_mut(oid).context("source")?;
        super::prepare_parent(&root, path)?;
        let proof = snapshot::safe_link_required(
            &source.path,
            &root.join(path),
            item,
            &source.proof,
            true,
        )?;
        source.proof = proof.clone();
        oids.insert(path.into(), oid.into());
        updated_proofs.insert(path.to_string(), proof);
    }
    for (path, p) in &mut updated_proofs {
        p["fingerprint"] = snapshot::fingerprint(&root.join(path))?;
        let fp = p["fingerprint"].as_array().context("proof fingerprint")?;
        changed.insert(
            (
                fp[0].as_u64().context("dev")?,
                fp[1].as_u64().context("inode")?,
            ),
            p.clone(),
        );
    }
    snapshot::verify(&root, files, Some(&updated_proofs))?;
    snapshot::sync_directories(&root, None)?;
    snapshot::sync_dir(root.parent().context("view parent")?)?;
    let full = super::base(c, key)?
        .join("releases")
        .join(manifest["sha"].as_str().context("SHA")?);
    refresh(&mut seal, &full, &changed)?;
    snapshot::verify(&full, files, Some(&proofs(&seal)?))?;
    atomic(&seal_path, &seal)?;
    record["status"] = "ready".into();
    record["files"] = manifest["files"].clone();
    record["oids"] = Value::Object(oids);
    record["proofs"] = serde_json::to_value(updated_proofs)?;
    atomic(&checkpoint, &record)
}
/// Full-byte audit renews proof ages only after a bounded complete read.
pub fn audit(c: &Value) -> Result<()> {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let time = now() as u64;
    let before = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if time.saturating_sub(before) < 15 {
        return Ok(());
    }
    LAST.store(time, std::sync::atomic::Ordering::Relaxed);
    if !super::refs(c) {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    for r in c["repositories"].as_array().context("repositories")? {
        if Instant::now() >= deadline || waiting(c)? {
            break;
        }
        let _ = audit_one(c, r, deadline);
    }
    Ok(())
}
fn audit_one(c: &Value, r: &Value, deadline: Instant) -> Result<()> {
    let key = r["key"].as_str().context("key")?;
    let (manifest, mut seal, p) = seal_paths(c, key)?;
    let full = super::base(c, key)?
        .join("releases")
        .join(manifest["sha"].as_str().context("SHA")?);
    let mut changed = false;
    let mut refreshed = BTreeMap::new();
    for item in manifest["files"].as_array().context("files")? {
        if Instant::now() >= deadline || waiting(c)? {
            break;
        }
        let name = item["path"].as_str().context("path")?;
        let proof = &seal["records"][name]["immutable"];
        if !snapshot::matches(&full.join(name), item, proof, true)
            || now() - proof["verified_at"].as_f64().unwrap_or(0.) < 1800.
        {
            continue;
        }
        let mut f = File::open(full.join(name))?;
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        let mut complete = true;
        loop {
            if Instant::now() >= deadline {
                complete = false;
                break;
            }
            let n = f.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        if !complete {
            break;
        }
        if hex::encode(hash.finalize()) == item["sha256"].as_str().unwrap_or("")
            && snapshot::matches(&full.join(name), item, proof, true)
        {
            let mut proof = proof.clone();
            proof["verified_at"] = now().into();
            seal["records"][name]["immutable"] = proof.clone();
            refreshed.insert(name.to_string(), proof);
            changed = true;
        } else {
            seal["records"]
                .as_object_mut()
                .context("records")?
                .remove(name);
            changed = true;
        }
    }
    if changed {
        atomic(&p, &seal)?;
        let path = checkpoint(c, key)?;
        if let Ok(mut record) = read_json(&path, 8 << 20) {
            if record["sha"] == manifest["sha"] && record["status"] == "ready" {
                for (name, proof) in refreshed {
                    if record["proofs"][&name]["fingerprint"] == proof["fingerprint"] {
                        record["proofs"][&name] = proof;
                    }
                }
                atomic(&path, &record)?;
            }
        }
    }
    Ok(())
}
