use super::*;
use crate::publisher::snapshot;
fn manifests(c: &Value, key: &str) -> Result<Vec<(PathBuf, Value)>> {
    let folder = state(c)?.join("manifests").join(key);
    if !present(&folder)? {
        return Ok(Vec::new());
    }
    real_dir(&folder)?;
    let mut out = Vec::new();
    for e in fs::read_dir(folder)? {
        ensure!(out.len() < 10000, "manifest count budget");
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let m = read_json(&p)?;
        ensure!(
            m["key"] == key && p.file_stem().and_then(|s| s.to_str()) == m["sha"].as_str(),
            "manifest identity mismatch"
        );
        validate_manifest(&m)?;
        out.push((p, m));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
pub fn local(c: &Value, apply: bool) -> Result<Value> {
    idle(c)?;
    let mut report = json!({"apply":apply,"files":0u64,"replaced":0u64,"reclaimable_bytes":0u64});
    let mut groups = BTreeMap::<(String, String, u64), Vec<(PathBuf, Value)>>::new();
    let limit = crate::publisher::git::limit(c, "scan_entries");
    let mut hashed = BTreeMap::<(u64, u64), String>::new();
    for r in selected(c, None)? {
        let key = text(r, "key")?;
        let base = image_base(c, key)?;
        for (_, m) in manifests(c, key)? {
            let sha = text(&m, "sha")?;
            let files = m["files"].as_array().unwrap();
            for (root, items) in [
                (base.join("releases").join(sha), files.clone()),
                (
                    base.join("releases").join(format!("images-{sha}")),
                    snapshot::images(files)?,
                ),
            ] {
                if !present(&root)? {
                    continue;
                }
                crate::publisher::verify_tree(&root, &items)?;
                for item in items {
                    let p = root.join(text(&item, "path")?);
                    let before = fs::symlink_metadata(&p)?;
                    let id = (before.dev(), before.ino());
                    let digest = if let Some(h) = hashed.get(&id) {
                        h.clone()
                    } else {
                        let (_, digest, _) = file_digest(&p, item["size"].as_u64().unwrap())?;
                        let after = fs::symlink_metadata(&p)?;
                        ensure!(
                            before.dev() == after.dev()
                                && before.ino() == after.ino()
                                && before.len() == after.len()
                                && before.mtime() == after.mtime()
                                && before.mtime_nsec() == after.mtime_nsec()
                                && before.ctime() == after.ctime()
                                && before.ctime_nsec() == after.ctime_nsec(),
                            "dedup source changed while hashing"
                        );
                        hashed.insert(id, digest.clone());
                        digest
                    };
                    ensure!(
                        before.len() == item["size"].as_u64().unwrap() && item["sha256"] == digest,
                        "dedup content mismatch"
                    );
                    groups
                        .entry((key.to_owned(), digest, before.len()))
                        .or_default()
                        .push((p, item));
                    report["files"] = json!(report["files"].as_u64().unwrap() + 1);
                    ensure!(
                        report["files"].as_u64().unwrap() <= limit,
                        "dedup scan budget"
                    );
                }
            }
        }
    }
    // No directory entry is replaced until every manifest and byte has passed preflight.
    for ((_key, _digest, size), entries) in groups {
        let mut physical = BTreeSet::new();
        for (p, _) in &entries {
            let m = fs::symlink_metadata(p)?;
            if physical.insert((m.dev(), m.ino())) && physical.len() > 1 {
                report["reclaimable_bytes"] =
                    json!(report["reclaimable_bytes"].as_u64().unwrap() + size);
            }
        }
        if !apply {
            continue;
        }
        let (source, item) = &entries[0];
        verify_file(source, item)?;
        let mut proof = snapshot::proof(source, item, crate::common::now())?;
        for (target, item) in entries.iter().skip(1) {
            let source_m = fs::symlink_metadata(source)?;
            let target_m = fs::symlink_metadata(target)?;
            if (source_m.dev(), source_m.ino()) == (target_m.dev(), target_m.ino()) {
                continue;
            }
            verify_file(target, item)?;
            let before = snapshot::fingerprint(target)?;
            let temp = target.with_file_name(format!(".dedup-{}", uuid::Uuid::new_v4().simple()));
            let result = (|| -> Result<()> {
                proof = snapshot::safe_link(source, &temp, item, &proof)?;
                ensure!(
                    snapshot::fingerprint(target)? == before,
                    "dedup target changed before replace"
                );
                fs::rename(&temp, target)?;
                proof["fingerprint"] = snapshot::fingerprint(source)?;
                sync_dir(target.parent().unwrap())?;
                Ok(())
            })();
            if present(&temp)? {
                let _ = fs::remove_file(&temp);
            }
            result?;
            report["replaced"] = json!(report["replaced"].as_u64().unwrap() + 1);
        }
    }
    if apply {
        let mut final_proofs = BTreeMap::<(u64, u64), Value>::new();
        for r in selected(c, None)? {
            let key = text(r, "key")?;
            let path = state(c)?.join("incremental").join(format!("{key}.json"));
            if !present(&path)? {
                continue;
            }
            let result = (|| -> Result<Value> {
                let mut seal = read_json(&path)?;
                let m = read_json(&state(c)?.join("published").join(key))?;
                ensure!(
                    seal["version"] == 3
                        && seal["sha"] == m["sha"]
                        && seal["config"] == c["revision"]
                        && m["key"] == key
                        && current_sha(&image_base(c, key)?)?.as_deref() == m["sha"].as_str(),
                    "stale incremental provenance"
                );
                let expected = crate::common::digest(
                    &json!({"files":m["files"],"object_sources":m.get("object_sources").cloned().unwrap_or_else(||json!({}))}),
                )?;
                ensure!(seal["manifest"] == expected, "stale manifest digest");
                let mut records = serde_json::Map::new();
                for item in m["files"].as_array().unwrap() {
                    let name = text(item, "path")?;
                    let mut old = seal["records"][name].clone();
                    if old.is_null() {
                        continue;
                    }
                    let p = image_base(c, key)?
                        .join("releases")
                        .join(text(&m, "sha")?)
                        .join(name);
                    verify_file(&p, item)?;
                    let metadata = fs::symlink_metadata(&p)?;
                    let identity = (metadata.dev(), metadata.ino());
                    let proof = if let Some(proof) = final_proofs.get(&identity) {
                        proof.clone()
                    } else {
                        let proof = snapshot::proof(&p, item, crate::common::now())?;
                        final_proofs.insert(identity, proof.clone());
                        proof
                    };
                    old["immutable"] = proof.clone();
                    old["fingerprint"] = json!(&proof["fingerprint"].as_array().unwrap()[..5]);
                    records.insert(name.to_owned(), old);
                }
                seal["records"] = json!(records);
                Ok(seal)
            })();
            let value=result.unwrap_or_else(|_|json!({"version":0,"reason":"verified local deduplication; no reusable source provenance"}));
            crate::common::atomic(&path, &value)?;
        }
        crate::common::atomic(&state(c)?.join("local-dedup.json"), &report)?;
    }
    Ok(report)
}
pub fn legacy(c: &Value, apply: bool) -> Result<Value> {
    idle(c)?;
    let mut count = 0;
    for r in selected(c, None)? {
        let key = text(r, "key")?;
        let base = image_base(c, key)?;
        for (_, m) in manifests(c, key)? {
            if m["image_layout"] != "snapshot-v1" {
                continue;
            }
            let sha = text(&m, "sha")?;
            let full = base.join("releases").join(sha);
            if !present(&full)? {
                continue;
            }
            let files = m["files"].as_array().unwrap();
            crate::publisher::verify_tree(&full, files)?;
            let images = snapshot::images(files)?;
            let target = base.join("releases").join(format!("images-{sha}"));
            if present(&target)? {
                crate::publisher::verify_tree(&target, &images)?;
                continue;
            }
            count += 1;
            if apply {
                let tmp = tempfile::Builder::new()
                    .prefix(".legacy-view-")
                    .tempdir_in(&base)?;
                for item in &images {
                    let name = text(item, "path")?;
                    let source = full.join("images").join(name);
                    let dest = tmp.path().join(name);
                    fs::create_dir_all(dest.parent().unwrap())?;
                    fs::hard_link(source, dest)?;
                }
                crate::publisher::verify_tree(tmp.path(), &images)?;
                snapshot::sync_tree(tmp.path())?;
                fs::rename(tmp.path(), &target)?;
                sync_dir(target.parent().unwrap())?;
            }
        }
        if apply {
            if let Some(sha) = current_sha(&base)? {
                let current = fs::read_link(base.join("current"))?;
                if current.to_string_lossy().ends_with("/images") {
                    snapshot::switch(&base, &format!("images-{sha}"))?;
                }
            }
            let cache = state(c)?.join("incremental").join(format!("{key}.json"));
            if present(&cache)? {
                crate::common::atomic(
                    &cache,
                    &json!({"version":0,"reason":"legacy view materialization"}),
                )?;
            }
        }
    }
    Ok(json!({"apply":apply,"versions":count}))
}
pub fn compact(c: &Value, apply: bool) -> Result<Value> {
    idle(c)?;
    let client = Client::from_config(c)?;
    ensure!(
        client.versioning()?.is_none(),
        "compaction requires unversioned bucket"
    );
    if apply {
        let response = http_client(None, 3)?
            .get("http://object-reader:8083/healthz")
            .send()?;
        ensure!(
            response.status().as_u16() == 200
                && response
                    .headers()
                    .get("X-Open-Assets-Reader")
                    .is_some_and(|h| h == "guard-v1"),
            "upgrade object reader before compaction"
        );
    }
    let mut plans = Vec::<(PathBuf, Value, Value)>::new();
    let mut expected = BTreeMap::<String, Value>::new();
    let mut skipped = Vec::new();
    for r in selected(c, None)? {
        let key = text(r, "key")?;
        let current_path = state(c)?.join("published").join(key);
        if !present(&current_path)? {
            continue;
        }
        let current = read_json(&current_path)?;
        if current["object_layout"] != "references-v1" {
            skipped.push(key.to_owned());
            continue;
        }
        let mut list = manifests(c, key)?;
        list.sort_by_key(|(p, m)| (m["sha"] != current["sha"], p.clone()));
        let mut canonical = BTreeMap::<(String, u64), Value>::new();
        for (path, original) in list {
            let sha = text(&original, "sha")?;
            let marker = state(c)?
                .join("object-retention")
                .join(key)
                .join(format!("{sha}.json"));
            if present(&marker)? {
                let rec = read_json(&marker)?;
                ensure!(rec["key"] == key && rec["sha"] == sha, "receipt mismatch");
                if rec["status"] == "complete" {
                    continue;
                }
                ensure!(rec["status"] != "deleting", "deletion already in progress");
            }
            let refs = validate_manifest(&original)?;
            let mut updated = original.clone();
            updated["object_layout"] = json!("references-v1");
            updated["object_sources"] = json!({});
            if original["object_layout"] != "references-v1" {
                updated["legacy_extra_objects"] = json!(true);
            }
            for item in original["files"].as_array().unwrap() {
                let path = text(item, "path")?;
                let src = &refs[path];
                let identity = (
                    text(item, "sha256")?.to_owned(),
                    item["size"].as_u64().unwrap(),
                );
                let owner = canonical.entry(identity).or_insert_with(|| src.clone());
                let mut metadata = item.clone();
                metadata["content_type"] = src["content_type"].clone();
                let source_key = text(src, "key")?;
                if let Some(old) = expected.insert(source_key.to_owned(), metadata.clone()) {
                    ensure!(
                        old["sha256"] == metadata["sha256"]
                            && old["size"] == metadata["size"]
                            && old["content_type"] == metadata["content_type"],
                        "conflicting compaction ownership"
                    );
                }
                let own = format!("repositories/{key}/releases/{sha}/{path}");
                if owner["key"] != own {
                    updated["object_sources"][path] = owner.clone();
                }
            }
            plans.push((path, original, updated));
            ensure!(
                plans.len() <= 10000 && expected.len() <= 100000,
                "compaction budget"
            );
        }
        let matching = plans
            .iter()
            .filter(|(_, old, _)| old["key"] == key && old["sha"] == current["sha"])
            .map(|(_, _, new)| new)
            .collect::<Vec<_>>();
        ensure!(
            matching == vec![&current],
            "current manifest must use canonical objects"
        );
    }
    let entries = expected.iter().collect::<Vec<_>>();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let client = &client;
            let entries = &entries;
            let next = &next;
            handles.push(scope.spawn(move || -> Result<()> {
                loop {
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((key, item)) = entries.get(n) else {
                        break;
                    };
                    let meta = client.verify_object(key, item)?;
                    ensure!(
                        meta["sha256"] == item["sha256"]
                            && meta["content_type"] == item["content_type"],
                        "compaction source mismatch"
                    );
                }
                Ok(())
            }));
        }
        let mut err = None;
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    err.get_or_insert(e);
                }
                Err(_) => {
                    err.get_or_insert(anyhow!("compaction verifier panic"));
                }
            }
        }
        if let Some(e) = err {
            Err(e)
        } else {
            Ok(())
        }
    })?;
    let mut used = BTreeSet::new();
    for (_, _, new) in &plans {
        for src in validate_manifest(new)?.values() {
            used.insert(text(src, "key")?.to_owned());
        }
    }
    let redundant = expected
        .keys()
        .filter(|k| !used.contains(*k))
        .cloned()
        .collect::<Vec<_>>();
    let mut report = json!({"apply":apply,"objects_before":expected.len(),"objects_after":used.len(),"redundant_bytes":redundant.iter().map(|k|expected[k]["size"].as_u64().unwrap()).sum::<u64>(),"deleted":0,"skipped_repositories":skipped});
    if !apply {
        return Ok(report);
    }
    for (path, old, _) in &plans {
        ensure!(
            read_json(path)? == *old,
            "manifest changed before compaction"
        );
    }
    let journal = state(c)?
        .join("object-compaction")
        .join(uuid::Uuid::new_v4().simple().to_string());
    fs::create_dir_all(&journal)?;
    real_dir(&journal)?;
    crate::common::atomic(
        &journal.join("plan.json"),
        &json!({"bucket":c["bucket"],"redundant":redundant,"report":report}),
    )?;
    let lockpath = state(c)?.join("object-retention/compaction.lock");
    let guard =
        read_guard(&lockpath, true, true)?.ok_or_else(|| anyhow!("compaction lock required"))?;
    for (path, old, new) in &plans {
        crate::common::atomic(
            &journal.join(format!("{}-{}.json", text(old, "key")?, text(old, "sha")?)),
            old,
        )?;
        if new != old {
            crate::common::atomic(path, new)?;
        }
    }
    crate::common::atomic(
        &journal.join("committed.json"),
        &json!({"status":"all manifests committed","objects":used}),
    )?;
    fs2::FileExt::unlock(&guard)?;
    for batch in redundant.chunks(500) {
        client.delete(batch)?;
        report["deleted"] = json!(report["deleted"].as_u64().unwrap() + batch.len() as u64);
        crate::common::atomic(&journal.join("result.json"), &report)?;
    }
    crate::common::atomic(&journal.join("result.json"), &report)?;
    Ok(report)
}
