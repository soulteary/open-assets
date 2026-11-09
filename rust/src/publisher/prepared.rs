//! Checkpointed private hard-link views. Never serves or writes through shared bytes.
use super::{git, queue, snapshot};
use crate::common::{atomic, now, read_json};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
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
    ensure!(
        manifest["key"] == key && manifest["sha"].as_str().is_some_and(super::valid_sha),
        "prepared manifest identity mismatch"
    );
    crate::storage::validate_manifest(&manifest)?;
    ensure!(
        fs::read_link(super::base(c, key)?.join("current"))?
            == Path::new(&format!("releases/{}", snapshot::image_target(&manifest)?)),
        "prepared current mismatch"
    );
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
                } else if old["status"] == "building" {
                    return recover_building(c, r, &old, &root);
                } else {
                    return Ok(());
                }
            }
        }
    }
    let (manifest, _, _) = seal_paths(c, key)?;
    if manifest["image_layout"] != "snapshot-v1" {
        return Ok(());
    }
    let mut sources = snapshot::sources(c, r)?;
    let (latest, mut seal, seal_path) = seal_paths(c, key)?;
    ensure!(latest == manifest, "prepared baseline changed");
    let files = manifest["files"].as_array().context("manifest files")?;
    let full = super::base(c, key)?
        .join("releases")
        .join(manifest["sha"].as_str().context("manifest SHA")?);
    snapshot::verify(&full, files, Some(&proofs(&seal)?))?;
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
/// Resume only the checkpointed, unserved directory. All current immutable bytes
/// are rehashed before adopting any ctime changed by an interrupted hardlink.
fn recover_building(c: &Value, r: &Value, old: &Value, root: &Path) -> Result<()> {
    let key = r["key"].as_str().context("key")?;
    let (manifest, seal, _) = seal_paths(c, key)?;
    ensure!(
        old["schema_version"] == 1
            && old["status"] == "building"
            && old["sha"] == manifest["sha"]
            && old["config"] == c["revision"]
            && old["manifest"] == seal["manifest"],
        "prepared recovery provenance mismatch"
    );
    let files = manifest["files"].as_array().context("files")?;
    for item in files {
        let name = item["path"].as_str().context("path")?;
        ensure!(
            snapshot::source_record_valid(&seal["records"][name]),
            "incomplete prepared source receipt"
        );
    }
    let mut sources = snapshot::sources_reverified(c, r)?;
    let (latest, mut seal, seal_path) = seal_paths(c, key)?;
    ensure!(latest == manifest, "prepared recovery baseline changed");
    let full = super::base(c, key)?
        .join("releases")
        .join(manifest["sha"].as_str().context("SHA")?);
    snapshot::verify(&full, files, Some(&proofs(&seal)?))?;
    let mut providers = BTreeMap::new();
    for proof in proofs(&seal)?.into_values() {
        let id = (
            proof["fingerprint"][0].as_u64().context("dev")?,
            proof["fingerprint"][1].as_u64().context("inode")?,
        );
        providers.insert(id, proof);
    }
    let expected = files
        .iter()
        .map(|i| Ok((i["path"].as_str().context("path")?.to_string(), i)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut present = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(dir) = stack.pop() {
        ensure!(
            fs::symlink_metadata(&dir)?.is_dir(),
            "unsafe prepared recovery directory"
        );
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            count += 1;
            ensure!(count <= 200000, "prepared recovery entry budget");
            let info = fs::symlink_metadata(&path)?;
            if info.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.strip_prefix(root)?.to_str().context("path encoding")?;
            let item = expected
                .get(name)
                .context("unexpected prepared recovery file")?;
            ensure!(
                info.is_file()
                    && snapshot::matches(
                        &path,
                        item,
                        providers
                            .get(&(info.dev(), info.ino()))
                            .unwrap_or(&Value::Null),
                        false
                    ),
                "unproven prepared recovery inode"
            );
            present.insert(name.to_string());
        }
    }
    let mut oids = serde_json::Map::new();
    let started = Instant::now();
    for item in files {
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "prepared recovery time budget exceeded"
        );
        let name = item["path"].as_str().context("path")?;
        let oid = seal["records"][name]["oid"].as_str().context("OID")?;
        if !present.contains(name) {
            super::prepare_parent(root, name)?;
            let source = sources
                .get_mut(oid)
                .context("missing prepared recovery source")?;
            source.proof = snapshot::safe_link_required(
                &source.path,
                &root.join(name),
                item,
                &source.proof,
                true,
            )?;
        }
        oids.insert(name.into(), oid.into());
    }
    // Only checked links above may change identities after the full-byte pass.
    for source in sources.values() {
        let fingerprint = &source.proof["fingerprint"];
        providers.insert(
            (
                fingerprint[0].as_u64().context("dev")?,
                fingerprint[1].as_u64().context("inode")?,
            ),
            source.proof.clone(),
        );
    }
    let mut view_proofs = BTreeMap::new();
    for item in files {
        let name = item["path"].as_str().context("path")?;
        let metadata = fs::symlink_metadata(root.join(name))?;
        let proof = providers
            .get(&(metadata.dev(), metadata.ino()))
            .context("missing prepared recovery proof")?;
        ensure!(
            snapshot::matches(&root.join(name), item, proof, false),
            "prepared recovery identity changed"
        );
        view_proofs.insert(name.into(), proof.clone());
    }
    snapshot::verify(root, files, Some(&view_proofs))?;
    snapshot::sync_directories(root, None)?;
    snapshot::sync_dir(root.parent().context("prepared parent")?)?;
    refresh(&mut seal, &full, &providers)?;
    snapshot::verify(&full, files, Some(&proofs(&seal)?))?;
    atomic(&seal_path, &seal)?;
    let mut record = old.clone();
    record["status"] = "ready".into();
    record["files"] = manifest["files"].clone();
    record["oids"] = Value::Object(oids);
    record["proofs"] = serde_json::to_value(view_proofs)?;
    atomic(&checkpoint(c, key)?, &record)
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
    audit_slice(c, r, deadline, usize::MAX, || waiting(c)).map(|_| ())
}
/// The cursor is only a scheduling hint, never verification evidence. Advancing
/// it after a slow/failed item prevents one source or a fresh prefix starving all
/// later files. Each accepted proof still requires a complete stable byte read.
fn audit_slice(
    c: &Value,
    r: &Value,
    deadline: Instant,
    max_groups: usize,
    mut queued: impl FnMut() -> Result<bool>,
) -> Result<usize> {
    let key = r["key"].as_str().context("key")?;
    let (manifest, mut seal, path) = seal_paths(c, key)?;
    let original_seal = seal.clone();
    let full = super::base(c, key)?
        .join("releases")
        .join(manifest["sha"].as_str().context("SHA")?);
    let files = manifest["files"].as_array().context("files")?;
    let tree = snapshot::inspect_tree(&full, files)?;
    type InodeContent = (u64, u64, String, u64);
    type AuditEntries = Vec<(String, Value)>;
    let mut groups: BTreeMap<InodeContent, AuditEntries> = BTreeMap::new();
    for item in files {
        let name = item["path"].as_str().context("path")?;
        let record = &seal["records"][name];
        if !snapshot::source_record_valid(record) {
            continue;
        }
        let proof = &record["immutable"];
        let age = now() - proof["verified_at"].as_f64().unwrap_or(f64::NEG_INFINITY);
        // No durable queue scan for an already-qualified fresh inode.
        if (0. ..1800.).contains(&age) && snapshot::matches(&full.join(name), item, proof, false) {
            continue;
        }
        let metadata = &tree[name];
        if metadata.mode() & 0o222 != 0
            || metadata.len() != item["size"].as_u64().context("size")?
        {
            continue;
        }
        groups
            .entry((
                metadata.dev(),
                metadata.ino(),
                item["sha256"].as_str().context("SHA256")?.into(),
                metadata.len(),
            ))
            .or_default()
            .push((name.into(), item.clone()));
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|a, b| a[0].0.cmp(&b[0].0));
    let cursor_path = super::state(c)?
        .join("incremental-audit")
        .join(format!("{key}.json"));
    let cursor = read_json(&cursor_path, 8192)
        .ok()
        .filter(|v| v["schema_version"] == 1 && v["sha"] == manifest["sha"]);
    if let Some(name) = cursor.as_ref().and_then(|v| v["last"].as_str()) {
        let offset = groups.partition_point(|g| g[0].0.as_str() <= name);
        if offset < groups.len() {
            groups.rotate_left(offset);
        }
    }
    let (mut attempted, mut changed) = (0usize, false);
    let mut renewed = BTreeMap::<(u64, u64), Value>::new();
    let mut last = None;
    for group in groups {
        if attempted >= max_groups || Instant::now() >= deadline || queued()? {
            break;
        }
        attempted += 1;
        let (name, item) = &group[0];
        last = Some(name.clone());
        if let Ok(proof) = snapshot::revalidate_proof(&full.join(name), item, Some(deadline)) {
            let id = (
                proof["fingerprint"][0].as_u64().context("dev")?,
                proof["fingerprint"][1].as_u64().context("inode")?,
            );
            renewed.insert(id, proof);
        }
        // A timeout cannot authenticate a partial read. Keep old records so a
        // later idle slice or synchronous publication can retry the whole body.
    }
    for item in files {
        let name = item["path"].as_str().context("path")?;
        let metadata = &tree[name];
        if let Some(proof) = renewed.get(&(metadata.dev(), metadata.ino())) {
            if snapshot::matches(&full.join(name), item, proof, false) {
                let record = seal["records"].get_mut(name).context("seal record")?;
                record["immutable"] = proof.clone();
                record["fingerprint"] = Value::Array(
                    proof["fingerprint"].as_array().context("fingerprint")?[..5].to_vec(),
                );
                changed = true;
            }
        }
    }
    if changed {
        let (current, current_seal, _) = seal_paths(c, key)?;
        ensure!(
            current == manifest && current_seal == original_seal,
            "audit baseline changed"
        );
        atomic(&path, &seal)?;
        let prepared = checkpoint(c, key)?;
        if let Ok(mut record) = read_json(&prepared, 8 << 20) {
            if record["sha"] == manifest["sha"] && record["status"] == "ready" {
                for proof in record["proofs"]
                    .as_object_mut()
                    .context("prepared proofs")?
                    .values_mut()
                {
                    if let (Some(dev), Some(inode)) = (
                        proof["fingerprint"][0].as_u64(),
                        proof["fingerprint"][1].as_u64(),
                    ) {
                        if let Some(fresh) = renewed.get(&(dev, inode)) {
                            if proof["fingerprint"] == fresh["fingerprint"]
                                && proof["sha256"] == fresh["sha256"]
                                && proof["size"] == fresh["size"]
                            {
                                *proof = fresh.clone();
                            }
                        }
                    }
                }
                atomic(&prepared, &record)?;
            }
        }
    }
    if let Some(last) = last {
        atomic(
            &cursor_path,
            &json!({"schema_version":1,"sha":manifest["sha"],"last":last}),
        )?;
    }
    Ok(attempted)
}

#[cfg(test)]
mod proof_maintenance_regression {
    use super::*;
    use crate::publisher::proof_reuse_fixtures::Fixture;
    #[test]
    fn fresh_prefix_does_not_scan_queue_or_starve_expired_tail() {
        let f = Fixture::new(64, false);
        f.age(&[62, 63], 3700.);
        let before = f.seal();
        let mut queue_scans = 0;
        let attempted = audit_slice(
            &f.c,
            f.repository(),
            Instant::now() + Duration::from_secs(2),
            1,
            || {
                queue_scans += 1;
                Ok(false)
            },
        )
        .unwrap();
        assert_eq!(attempted, 1);
        assert_eq!(queue_scans, 1);
        let middle = f.seal();
        assert_eq!(
            middle["records"]["images/0000.png"],
            before["records"]["images/0000.png"]
        );
        assert!(
            middle["records"]["images/0062.png"]["immutable"]["verified_at"]
                .as_f64()
                .unwrap()
                > before["records"]["images/0062.png"]["immutable"]["verified_at"]
                    .as_f64()
                    .unwrap()
        );
        let attempted = audit_slice(
            &f.c,
            f.repository(),
            Instant::now() + Duration::from_secs(2),
            1,
            || Ok(false),
        )
        .unwrap();
        assert_eq!(attempted, 1);
        let after = f.seal();
        assert!(
            after["records"]["images/0063.png"]["immutable"]["verified_at"]
                .as_f64()
                .unwrap()
                > before["records"]["images/0063.png"]["immutable"]["verified_at"]
                    .as_f64()
                    .unwrap()
        );
    }
    #[test]
    fn audit_same_oid_inode_hashes_once_and_updates_all_aliases() {
        let f = Fixture::new(4, true);
        f.age(&[0, 1, 2, 3], 3700.);
        let mut scans = 0;
        let attempted = audit_slice(
            &f.c,
            f.repository(),
            Instant::now() + Duration::from_secs(2),
            usize::MAX,
            || {
                scans += 1;
                Ok(false)
            },
        )
        .unwrap();
        assert_eq!(attempted, 1);
        assert_eq!(scans, 1);
        let seal = f.seal();
        let proof = seal["records"]["images/0000.png"]["immutable"].clone();
        for item in &f.files {
            assert_eq!(
                seal["records"][item["path"].as_str().unwrap()]["immutable"],
                proof
            );
        }
    }
    #[test]
    fn failed_budget_does_not_create_proof_and_cursor_advances() {
        let f = Fixture::new(3, false);
        f.age(&[0, 1, 2], 3700.);
        let before = f.seal();
        let attempted = audit_slice(
            &f.c,
            f.repository(),
            Instant::now() + Duration::from_millis(200),
            1,
            || {
                std::thread::sleep(Duration::from_millis(250));
                Ok(false)
            },
        )
        .unwrap();
        assert_eq!(attempted, 1);
        assert_eq!(f.seal(), before);
        audit_slice(
            &f.c,
            f.repository(),
            Instant::now() + Duration::from_secs(2),
            1,
            || Ok(false),
        )
        .unwrap();
        let after = f.seal();
        assert_eq!(
            after["records"]["images/0000.png"],
            before["records"]["images/0000.png"]
        );
        assert_ne!(
            after["records"]["images/0001.png"],
            before["records"]["images/0001.png"]
        );
    }
    #[test]
    fn same_sha_building_resumes_without_removing_directory_or_copying_bytes() {
        let f = Fixture::new(3, false);
        let root = crate::publisher::base(&f.c, "images")
            .unwrap()
            .join(format!(".staging-prefetch-{}", "b".repeat(32)));
        fs::create_dir_all(root.join("images")).unwrap();
        let before_seal = f.seal();
        let record = json!({"schema_version":1,"status":"building","directory":root.file_name().unwrap().to_str().unwrap(),"sha":f.manifest["sha"],"config":f.c["revision"],"manifest":before_seal["manifest"]});
        atomic(&checkpoint(&f.c, "images").unwrap(), &record).unwrap();
        fs::hard_link(f.full.join("images/0000.png"), root.join("images/0000.png")).unwrap();
        let root_inode = fs::metadata(&root).unwrap().ino();
        let old_file_inode = fs::metadata(root.join("images/0000.png")).unwrap().ino();
        assert!(!snapshot::matches(
            &f.full.join("images/0000.png"),
            &f.files[0],
            &before_seal["records"]["images/0000.png"]["immutable"],
            false
        ));
        prepare_one(&f.c, f.repository()).unwrap();
        let ready = read_json(&checkpoint(&f.c, "images").unwrap(), 8 << 20).unwrap();
        assert_eq!(ready["status"], "ready");
        assert_eq!(fs::metadata(&root).unwrap().ino(), root_inode);
        assert_eq!(
            fs::metadata(root.join("images/0000.png")).unwrap().ino(),
            old_file_inode
        );
        for item in &f.files {
            let name = item["path"].as_str().unwrap();
            assert_eq!(
                fs::metadata(root.join(name)).unwrap().ino(),
                fs::metadata(f.full.join(name)).unwrap().ino()
            );
        }
        let refreshed = f.seal();
        snapshot::verify(&f.full, &f.files, Some(&proofs(&refreshed).unwrap())).unwrap();
        snapshot::verify(
            &root,
            &f.files,
            Some(
                &serde_json::from_value::<BTreeMap<String, Value>>(ready["proofs"].clone())
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            read_json(
                &crate::publisher::state(&f.c)
                    .unwrap()
                    .join("published/images"),
                8 << 20
            )
            .unwrap(),
            f.manifest
        );
    }
    #[test]
    fn building_recovery_refuses_unproven_existing_file_without_deletion() {
        let f = Fixture::new(2, false);
        let root = crate::publisher::base(&f.c, "images")
            .unwrap()
            .join(format!(".staging-prefetch-{}", "c".repeat(32)));
        fs::create_dir_all(root.join("images")).unwrap();
        let seal = f.seal();
        let record = json!({"schema_version":1,"status":"building","directory":root.file_name().unwrap().to_str().unwrap(),"sha":f.manifest["sha"],"config":f.c["revision"],"manifest":seal["manifest"]});
        atomic(&checkpoint(&f.c, "images").unwrap(), &record).unwrap();
        let rogue = root.join("images/0000.png");
        fs::write(&rogue, b"unexpected bytes").unwrap();
        assert!(prepare_one(&f.c, f.repository()).is_err());
        assert_eq!(fs::read(&rogue).unwrap(), b"unexpected bytes");
        assert_eq!(
            read_json(&checkpoint(&f.c, "images").unwrap(), 8 << 20).unwrap(),
            record
        );
    }
}
