use super::*;
use std::io::Cursor;
const FILES: [(&str, u64); 4] = [
    ("intent.json", 8192),
    ("result.json", 8192),
    ("source.json", 16384),
    ("check.json", 16384),
];
fn audit_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn limit(name: &str) -> Option<u64> {
    FILES.iter().find(|(n, _)| *n == name).map(|(_, n)| *n)
}
fn finite(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|n| n.is_finite() && n >= 0. && n <= crate::common::now() + 300.)
}
fn read_directory(path: &Path, partial: bool) -> Result<BTreeMap<String, Vec<u8>>> {
    real_dir(path)?;
    let mut files = BTreeMap::new();
    for e in fs::read_dir(path)? {
        let p = e?.path();
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid audit file"))?;
        let max = limit(name).ok_or_else(|| anyhow!("unexpected audit file"))?;
        ensure!(files.len() < 4, "audit file count");
        files.insert(name.to_owned(), crate::common::read_regular(&p, max)?);
    }
    ensure!(
        partial || files.contains_key("intent.json"),
        "missing audit intent"
    );
    Ok(files)
}
fn header(v: &Value, key: &str, id: &str) -> Result<()> {
    ensure!(
        v["schema_version"] == 1
            && v["key"] == key
            && v["audit_id"] == id
            && v["sha"].as_str().is_some_and(valid_sha)
            && finite(&v["started_at"]),
        "invalid closure header"
    );
    Ok(())
}
fn validate(files: &BTreeMap<String, Vec<u8>>, key: &str, id: &str) -> Result<Value> {
    ensure!(
        files.len() <= 4 && files.contains_key("intent.json") && files.contains_key("result.json"),
        "incomplete audit files"
    );
    for (name, raw) in files {
        ensure!(
            limit(name).is_some_and(|n| raw.len() as u64 <= n),
            "audit file limit"
        );
    }
    let intent: Value = serde_json::from_slice(&files["intent.json"])?;
    let result: Value = serde_json::from_slice(&files["result.json"])?;
    header(&intent, key, id)?;
    header(&result, key, id)?;
    ensure!(
        intent["status"] == "running"
            && ["sha", "started_at", "scope_digest"]
                .iter()
                .all(|k| result[*k] == intent[*k])
            && matches!(
                result["status"].as_str(),
                Some("closed" | "refused" | "failed")
            )
            && finite(&result["finished_at"])
            && result["finished_at"].as_f64() >= intent["started_at"].as_f64(),
        "invalid closure outcome"
    );
    crate::info::reports::clean_closure(&intent, Some(&result), key, id)?;
    for (name, field) in [
        ("source.json", "source_report_digest"),
        ("check.json", "check_digest"),
    ] {
        if files.contains_key(name) || !result[field].is_null() {
            let raw = files
                .get(name)
                .ok_or_else(|| anyhow!("missing audit snapshot"))?;
            ensure!(result[field] == hash(raw), "audit snapshot digest mismatch");
        }
    }
    if let Some(raw) = files.get("source.json") {
        let s: Value = serde_json::from_slice(raw)?;
        ensure!(
            s["schema_version"] == 1
                && s["key"] == key
                && s["sha"] == intent["sha"]
                && s["scope_digest"] == intent["scope_digest"]
                && s["status"] == result["source_status"],
            "audit source binding mismatch"
        );
        let cleaned = crate::info::reports::clean_restore(
            &s,
            key,
            text(&intent, "sha")?,
            text(&intent, "scope_digest")?,
        )?;
        let uncertain = cleaned["uncertain"] == true;
        ensure!(
            result["source_uncertain"] == uncertain,
            "audit uncertainty binding mismatch"
        );
    }
    if let Some(raw) = files.get("check.json") {
        let check: Value = serde_json::from_slice(raw)?;
        crate::info::reports::clean_check(&check, key, text(&intent, "sha")?)?;
        ensure!(
            check["schema_version"] == 1
                && check["key"] == key
                && check["sha"] == intent["sha"]
                && check["source_report_digest"] == result["source_report_digest"],
            "audit check binding mismatch"
        );
        if result["status"] == "closed" {
            ensure!(
                result["reason"] == "verified"
                    && result["source_uncertain"] == true
                    && check["diagnosis"] == "verified"
                    && check["status"] == "complete"
                    && check["scope_digest"] == intent["scope_digest"]
                    && check["source_uncertain"] == true
                    && check["checks"]
                        .as_object()
                        .is_some_and(|m| m.len() == 5 && m.values().all(|v| v["status"] == "ok"))
                    && [
                        "manifest_digest",
                        "reference_digest",
                        "current_is_target",
                        "published_is_target"
                    ]
                    .iter()
                    .all(|k| check[*k] == result[*k])
                    && matches!(
                        check["checks"]["references"]["reason"].as_str(),
                        Some("target-current" | "other-current")
                    )
                    && check["started_at"].as_f64() >= intent["started_at"].as_f64()
                    && check["finished_at"].as_f64() <= result["finished_at"].as_f64(),
                "invalid closed check evidence"
            );
        }
    } else {
        ensure!(
            result["status"] != "closed",
            "closed audit missing check evidence"
        );
    }
    if result["status"] == "closed" {
        crate::info::reports::verify_closure_snapshots(
            &files["source.json"],
            &files["check.json"],
            &intent,
            &result,
        )?;
    }
    let digests = files
        .iter()
        .map(|(n, b)| (n.clone(), json!(hash(b))))
        .collect::<serde_json::Map<_, _>>();
    Ok(
        json!({"audit_id":id,"sha":intent["sha"],"status":result["status"],"started_at":intent["started_at"],"finished_at":result["finished_at"],"reason":result["reason"],"source_uncertain":result.get("source_uncertain").cloned().unwrap_or(json!(false)),"source_digest":result["source_report_digest"],"bytes":files.values().map(Vec::len).sum::<usize>(),"file_digest":crate::common::digest(&json!(digests))?}),
    )
}
pub fn close(c: &Value, key: &str, sha: &str, reason: &str) -> Result<Value> {
    ensure!(
        !reason.trim().is_empty()
            && reason.trim().chars().count() <= 256
            && !reason.chars().any(|c| c < ' ' || c == '\u{7f}'),
        "invalid closure reason"
    );
    let id = uuid::Uuid::new_v4().simple().to_string();
    let folder = state(c)?.join("restore-closures").join(key).join(&id);
    fs::create_dir_all(&folder)?;
    real_dir(&folder)?;
    let intent = json!({"schema_version":1,"key":key,"sha":sha,"audit_id":id,"status":"running","started_at":crate::common::now(),"scope_digest":scoped_digest(c)?,"reason":reason.trim(),"execution_uid":unsafe{libc::getuid()}});
    write_once(
        &folder.join("intent.json"),
        &crate::common::python_json(&intent, true)?,
    )?;
    let mut result = intent.clone();
    result.as_object_mut().unwrap().remove("reason");
    result.as_object_mut().unwrap().remove("execution_uid");
    result["status"] = json!("failed");
    result["reason"] = json!("unavailable");
    let operation = (|| -> Result<()> {
        let (digest, source_status, evidence) = super::restore::source_evidence(c, key, sha)
            .map_err(|_| anyhow!("source-unavailable"))?;
        let digest = digest.ok_or_else(|| anyhow!("source-unavailable"))?;
        let raw = crate::common::read_regular(
            &state(c)?
                .join("object-restores")
                .join(key)
                .join(format!("{sha}.json")),
            16384,
        )?;
        ensure!(hash(&raw) == digest, "evidence-changed");
        write_once(&folder.join("source.json"), &raw)?;
        result["source_report_digest"] = json!(digest);
        result["source_status"] = json!(source_status);
        result["source_uncertain"] = evidence["uncertain"].clone();
        ensure!(evidence["uncertain"] == true, "ineligible");
        let checked = super::restore::check(c, key, sha)?;
        let raw = crate::common::python_json(&checked, true)?;
        write_once(&folder.join("check.json"), &raw)?;
        result["check_digest"] = json!(hash(&raw));
        result["manifest_digest"] = checked["manifest_digest"].clone();
        result["reference_digest"] = checked["reference_digest"].clone();
        ensure!(
            checked["status"] == "complete" && checked["diagnosis"] == "verified",
            "check-failed"
        );
        ensure!(
            matches!(
                checked["checks"]["references"]["reason"].as_str(),
                Some("target-current" | "other-current")
            ),
            "references-missing"
        );
        let (current, _, _) = super::restore::source_evidence(c, key, sha)?;
        let m = read_json(
            &state(c)?
                .join("manifests")
                .join(key)
                .join(format!("{sha}.json")),
        )?;
        ensure!(
            current.as_deref() == Some(&digest)
                && checked["source_report_digest"] == digest
                && crate::common::digest(&m)? == checked["manifest_digest"]
                && super::restore::reference_fingerprint(c, key)? == checked["reference_digest"]
                && matches!(
                    super::restore::barrier(c, key, sha)?.as_str(),
                    "none" | "restored"
                ),
            "evidence-changed"
        );
        result["status"] = json!("closed");
        result["reason"] = json!("verified");
        result["current_is_target"] = checked["current_is_target"].clone();
        result["published_is_target"] = checked["published_is_target"].clone();
        Ok(())
    })();
    if let Err(e) = operation {
        let s = e.to_string();
        if matches!(
            s.as_str(),
            "source-unavailable"
                | "ineligible"
                | "check-failed"
                | "references-missing"
                | "evidence-changed"
        ) {
            result["status"] = json!("refused");
            result["reason"] = json!(s);
        } else {
            result["status"] = json!("failed");
            result["reason"] = json!(match super::restore::error_class(&e) {
                "FileNotFoundError" => "missing",
                "PermissionError" => "permission",
                "OSError" => "io",
                "TimeoutError" => "timeout",
                _ => "mismatch",
            });
        }
    }
    result["finished_at"] = json!(crate::common::now());
    write_once(
        &folder.join("result.json"),
        &crate::common::python_json(&result, true)?,
    )?;
    Ok(result)
}
fn archive_bytes(
    files: &BTreeMap<String, Vec<u8>>,
    key: &str,
    id: &str,
    sha: &str,
) -> Result<Vec<u8>> {
    let manifest = json!({"schema_version":1,"key":key,"audit_id":id,"sha":sha,"archived_at":crate::common::now(),"execution_uid":unsafe{libc::getuid()},"files":files.iter().map(|(n,b)|(n.clone(),json!({"size":b.len(),"sha256":hash(b)}))).collect::<serde_json::Map<_,_>>()});
    let mut all = files.clone();
    all.insert(
        "manifest.json".into(),
        crate::common::python_json(&manifest, true)?,
    );
    let mut tar = tar::Builder::new(Vec::new());
    for (name, raw) in all {
        let mut h = tar::Header::new_ustar();
        h.set_size(raw.len() as u64);
        h.set_mode(0o600);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        tar.append_data(&mut h, name, raw.as_slice())?;
    }
    let bytes = tar.into_inner()?;
    ensure!(bytes.len() <= 131072, "audit tar byte limit");
    let mut gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::default());
    gzip.write_all(&bytes)?;
    let raw = gzip.finish()?;
    ensure!(raw.len() <= 262144, "audit archive byte limit");
    Ok(raw)
}
fn verify_archive(path: &Path, key: &str, id: &str) -> Result<(Value, BTreeMap<String, Vec<u8>>)> {
    let raw = crate::common::read_regular(path, 262144)?;
    let gzip = flate2::read::MultiGzDecoder::new(raw.as_slice());
    let mut decoded = Vec::new();
    gzip.take(131073).read_to_end(&mut decoded)?;
    ensure!(decoded.len() <= 131072, "audit expansion limit");
    let mut archive = tar::Archive::new(Cursor::new(&decoded));
    let mut files = BTreeMap::new();
    let mut end = 0usize;
    for e in archive.entries()?.raw(true) {
        let mut e = e?;
        let name = e
            .path()?
            .to_str()
            .ok_or_else(|| anyhow!("invalid audit member"))?
            .to_owned();
        let max = if name == "manifest.json" {
            8192
        } else {
            limit(&name).ok_or_else(|| anyhow!("unsafe archive member"))?
        };
        ensure!(
            files.len() < 5
                && !files.contains_key(&name)
                && e.header().entry_type() == tar::EntryType::Regular
                && e.size() <= max,
            "unsafe audit archive member"
        );
        let mut b = Vec::new();
        e.read_to_end(&mut b)?;
        end = e.raw_file_position() as usize + (e.size().div_ceil(512) * 512) as usize;
        files.insert(name, b);
    }
    ensure!(
        decoded[end..].iter().all(|b| *b == 0),
        "unexpected archive trailer"
    );
    let manifest: Value = serde_json::from_slice(
        &files
            .remove("manifest.json")
            .ok_or_else(|| anyhow!("missing archive manifest"))?,
    )?;
    let expected = files
        .iter()
        .map(|(n, b)| (n.clone(), json!({"size":b.len(),"sha256":hash(b)})))
        .collect::<serde_json::Map<_, _>>();
    ensure!(
        manifest["schema_version"] == 1
            && manifest["key"] == key
            && manifest["audit_id"] == id
            && finite(&manifest["archived_at"])
            && manifest["files"] == json!(expected),
        "archive manifest mismatch"
    );
    let row = validate(&files, key, id)?;
    ensure!(manifest["sha"] == row["sha"], "archive SHA mismatch");
    Ok((row, files))
}
pub type AuditPaths = BTreeMap<&'static str, PathBuf>;
pub type AuditScan = (AuditPaths, Vec<Value>);
fn dirs(c: &Value, key: &str) -> Result<AuditPaths> {
    let root = state(c)?;
    let mut out = BTreeMap::new();
    for (name, folder) in [
        ("online", "restore-closures"),
        ("archive", "restore-audit-archives"),
        ("retired", "restore-audit-retired"),
    ] {
        let p = root.join(folder).join(key);
        for d in [p.parent().unwrap(), &p] {
            if fs::symlink_metadata(d).is_ok() {
                real_dir(d)?;
            }
        }
        out.insert(name, p);
    }
    Ok(out)
}
pub fn scan(c: &Value, key: &str, deadline: Instant) -> Result<AuditScan> {
    let paths = dirs(c, key)?;
    let mut rows = BTreeMap::<String, Value>::new();
    let mut count = 0;
    for (location, path) in &paths {
        if !present(path)? {
            continue;
        }
        for e in fs::read_dir(path)? {
            count += 1;
            ensure!(
                count <= 20000 && Instant::now() < deadline,
                "audit scan budget"
            );
            let p = e?.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let id = if *location == "archive" {
                name.strip_suffix(".tar.gz").unwrap_or("")
            } else {
                name
            };
            ensure!(audit_id(id), "unknown audit entry");
            let checked = if *location == "archive" {
                verify_archive(&p, key, id).map(|(row, _)| row)
            } else if *location == "retired" {
                verify_archive(&paths["archive"].join(format!("{id}.tar.gz")), key, id).and_then(
                    |(record, saved)| {
                        let remaining = read_directory(&p, true)?;
                        ensure!(
                            remaining.iter().all(|(n, b)| saved.get(n) == Some(b)),
                            "retired audit conflicts with archive"
                        );
                        Ok(record)
                    },
                )
            } else {
                read_directory(&p,false).and_then(|files|{
                    if !files.contains_key("result.json") {
                        let intent:Value=serde_json::from_slice(files.get("intent.json").ok_or_else(||anyhow!("missing intent"))?)?;
                        header(&intent,key,id)?;
                        return Ok(json!({"audit_id":id,"sha":intent["sha"],"status":"running","started_at":intent["started_at"],"bytes":files.values().map(Vec::len).sum::<usize>()}))
                    }
                    validate(&files,key,id)
                })
            };
            let mut row = checked
                .unwrap_or_else(|_| json!({"audit_id":id,"invalid":true,"status":"invalid"}));
            if let Some(old) = rows.get_mut(id) {
                if (old["status"] != row["status"] || old["file_digest"] != row["file_digest"])
                    && old["invalid"] != true
                    && row["invalid"] != true
                {
                    old["invalid"] = json!(true);
                }
                old["locations"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(location));
            } else {
                row["locations"] = json!([location]);
                rows.insert(id.to_owned(), row);
            }
        }
    }
    Ok((paths, rows.into_values().collect()))
}
fn option(args: &[String], name: &str, default: u64, low: u64, high: u64) -> Result<u64> {
    let n = argument(args, name)
        .map(str::parse::<u64>)
        .transpose()?
        .unwrap_or(default);
    ensure!((low..=high).contains(&n), "invalid audit limit");
    Ok(n)
}
pub fn run(c: &Value, command: &str, args: &[String]) -> Result<Value> {
    let key = args
        .first()
        .ok_or_else(|| anyhow!("audit repository required"))?;
    ensure!(
        valid_key(key)
            && c["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["key"] == *key),
        "unknown audit repository"
    );
    let sha = args
        .get(1)
        .filter(|s| !s.starts_with('-'))
        .map(String::as_str);
    if let Some(sha) = sha {
        ensure!(valid_sha(sha), "invalid audit SHA");
    }
    let apply = args.iter().any(|s| s == "--apply");
    ensure!(
        !apply || command == "restore-audit-archive",
        "--apply only allowed for archive"
    );
    let keep = option(args, "--keep-last", 20, 1, 1000)?;
    let age = option(args, "--min-age-days", 30, 0, 36500)?;
    let max = option(args, "--max-records", 100, 1, 500)?;
    let limit = option(args, "--limit", 100, 1, 1000)? as usize;
    let offset = option(args, "--offset", 0, 0, 1000000)? as usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    let (paths, mut rows) = scan(c, key, deadline)?;
    if command != "restore-audit-archive" {
        rows.retain(|r| {
            (sha.is_none_or(|s| r["sha"] == s) || r["invalid"] == true)
                && (command != "restore-audit-verify"
                    || r["locations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s == "archive" || s == "retired"))
        });
        rows.sort_by(|a, b| {
            b["started_at"]
                .as_f64()
                .unwrap_or(0.)
                .total_cmp(&a["started_at"].as_f64().unwrap_or(0.))
                .then_with(|| b["audit_id"].as_str().cmp(&a["audit_id"].as_str()))
        });
        return Ok(
            json!({"status":if rows.iter().any(|r|r["invalid"]==true){"failed"}else{"complete"},"key":key,"total":rows.len(),"offset":offset,"has_more":rows.len()>offset+limit,"records":rows.into_iter().skip(offset).take(limit).collect::<Vec<_>>()}),
        );
    }
    let online = |r: &Value| {
        r["locations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "online")
    };
    let retired = |r: &Value| {
        r["locations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "retired")
    };
    let online_count = rows.iter().filter(|r| online(r)).count();
    let mut ordered = rows
        .iter()
        .filter(|r| online(r) && r["invalid"] != true)
        .collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        b["started_at"]
            .as_f64()
            .unwrap_or(0.)
            .total_cmp(&a["started_at"].as_f64().unwrap_or(0.))
    });
    let protected_ids = ordered
        .iter()
        .take(keep as usize)
        .filter_map(|r| r["audit_id"].as_str())
        .collect::<BTreeSet<_>>();
    let covered = rows
        .iter()
        .filter(|r| r["status"] == "closed" && r["invalid"] != true)
        .filter_map(|r| r["source_digest"].as_str())
        .collect::<BTreeSet<_>>();
    let mut candidates = Vec::new();
    let mut protected = Vec::new();
    for row in &rows {
        if sha.is_some_and(|s| row["sha"] != s) && row["invalid"] != true {
            continue;
        }
        if !online(row) && !retired(row) {
            continue;
        }
        let id = text(row, "audit_id")?;
        let reason = if row["invalid"] == true {
            Some("invalid")
        } else if retired(row) {
            None
        } else if row["status"] == "running" {
            Some("unfinished")
        } else if protected_ids.contains(id) {
            Some("keep-last")
        } else if row["source_uncertain"] == true
            && row["status"] != "closed"
            && !row["source_digest"]
                .as_str()
                .is_some_and(|s| covered.contains(s))
        {
            Some("unresolved-source")
        } else if row["finished_at"].as_f64().unwrap_or(crate::common::now())
            > crate::common::now() - age as f64 * 86400.
        {
            Some("recent")
        } else if candidates.len() >= max as usize {
            Some("batch-limit")
        } else {
            None
        };
        if let Some(reason) = reason {
            let mut row = row.clone();
            row["protection"] = json!(reason);
            protected.push(row);
        } else {
            candidates.push(row.clone());
        }
    }
    let mut counts = BTreeMap::<String, u64>::new();
    for r in &protected {
        *counts.entry(text(r, "protection")?.to_owned()).or_default() += 1;
    }
    let mut result = json!({"status":"complete","key":key,"mode":if apply{"apply"}else{"preview"},"online_records":online_count,"candidates":candidates,"protected":protected.iter().take(1000).collect::<Vec<_>>(),"protected_total":protected.len(),"protected_truncated":protected.len()>1000,"protection_counts":counts,"archived_records":0,"partial_possible":false,"info_limit":512,"invalid_records":rows.iter().filter(|r|r["invalid"]==true).count(),"candidate_bytes":candidates.iter().map(|r|r["bytes"].as_u64().unwrap_or(0)).sum::<u64>(),"projected_online_records":online_count-candidates.iter().filter(|r|online(r)).count()});
    if !apply {
        return Ok(result);
    }
    let observation = state(c)?
        .join("audit-archive-results")
        .join(format!("{key}.json"));
    let started = crate::common::now();
    let save_observation = |result: &Value, finished: bool| -> Result<()> {
        let mut value = json!({"status":if finished{result["status"].clone()}else{json!("running")},"started_at":started,"candidate_records":candidates.len(),"archived_records":result["archived_records"],"protected_records":result["protected_total"],"protection_counts":result["protection_counts"],"partial_possible":result["partial_possible"],"keep_last":keep,"min_age_days":age,"max_records":max});
        if finished {
            value["finished_at"] = json!(crate::common::now());
        }
        if let Some(failure) = result.get("failure_reason") {
            value["failure_reason"] = failure.clone();
        }
        let mut output = json!({"schema_version":1,"key":key,"attempt":value});
        if present(&observation)? {
            if let Ok(old) = read_json(&observation) {
                if old["last_complete"].is_object() {
                    output["last_complete"] = old["last_complete"].clone();
                }
            }
        }
        if finished && result["status"] == "complete" {
            output["last_complete"] = value;
        }
        crate::common::atomic(&observation, &output)
    };
    let operation = (|| -> Result<()> {
        for name in ["archive", "retired"] {
            fs::create_dir_all(&paths[name])?;
            real_dir(&paths[name])?;
        }
        for row in &candidates {
            ensure!(Instant::now() < deadline, "audit archive timeout");
            result["partial_possible"] = json!(true);
            save_observation(&result, false)?;
            let id = text(row, "audit_id")?;
            let source = paths["online"].join(id);
            let retired = paths["retired"].join(id);
            let target = paths["archive"].join(format!("{id}.tar.gz"));
            if !present(&retired)? {
                let files = read_directory(&source, false)?;
                let record = validate(&files, key, id)?;
                ensure!(
                    record["file_digest"] == row["file_digest"],
                    "audit changed during planning"
                );
                if !present(&target)? {
                    write_once(
                        &target,
                        &archive_bytes(&files, key, id, text(&record, "sha")?)?,
                    )?;
                }
                ensure!(
                    verify_archive(&target, key, id)?.1 == files
                        && read_directory(&source, false)? == files,
                    "archive verification failed"
                );
                fs::rename(&source, &retired)?;
                sync_dir(&paths["online"])?;
                sync_dir(&paths["retired"])?;
            }
            let (_, saved) = verify_archive(&target, key, id)?;
            let remaining = read_directory(&retired, true)?;
            ensure!(
                remaining.iter().all(|(n, b)| saved.get(n) == Some(b)),
                "retired audit changed"
            );
            for name in remaining.keys() {
                fs::remove_file(retired.join(name))?;
            }
            fs::remove_dir(&retired)?;
            sync_dir(&paths["retired"])?;
            result["archived_records"] = json!(result["archived_records"].as_u64().unwrap() + 1);
        }
        result["partial_possible"] = json!(false);
        Ok(())
    })();
    if operation.is_err() {
        result["status"] = json!("failed");
        result["failure_reason"] = json!("unavailable");
    }
    save_observation(&result, true)?;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_rejects_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.tar.gz");
        fs::write(&path, b"not gzip").unwrap();
        assert!(verify_archive(&path, "images", &"a".repeat(32)).is_err());
    }
    #[test]
    fn incomplete_audit_not_archivable() {
        assert!(validate(&BTreeMap::new(), "images", &"a".repeat(32)).is_err());
    }
    fn fixture_files(id: &str, started: f64) -> BTreeMap<String, Vec<u8>> {
        let intent = json!({"schema_version":1,"key":"images","sha":"a".repeat(40),"audit_id":id,"status":"running","started_at":started,"scope_digest":"b".repeat(64),"reason":"fixture audit","execution_uid":1000});
        let mut result = intent.clone();
        result["status"] = json!("refused");
        result["reason"] = json!("source-unavailable");
        result["finished_at"] = json!(started + 1.);
        BTreeMap::from([
            (
                "intent.json".to_owned(),
                crate::common::python_json(&intent, true).unwrap(),
            ),
            (
                "result.json".to_owned(),
                crate::common::python_json(&result, true).unwrap(),
            ),
        ])
    }
    #[test]
    fn archive_roundtrip_preserves_exact_evidence_and_recovers_partial_retirement() {
        let tmp = tempfile::tempdir().unwrap();
        let c = json!({"state_root":tmp.path(),"repositories":[{"key":"images"}]});
        let paths = dirs(&c, "images").unwrap();
        let ids = ["a".repeat(32), "b".repeat(32)];
        for (i, id) in ids.iter().enumerate() {
            let dir = paths["online"].join(id);
            fs::create_dir_all(&dir).unwrap();
            for (name, raw) in fixture_files(id, 1700000000. + i as f64 * 10.) {
                fs::write(dir.join(name), raw).unwrap();
            }
        }
        let mut args = vec![
            "images".into(),
            "--keep-last".into(),
            "1".into(),
            "--min-age-days".into(),
            "0".into(),
        ];
        let preview = run(&c, "restore-audit-archive", &args).unwrap();
        assert_eq!(preview["candidates"].as_array().unwrap().len(), 1);
        assert!(paths["online"].join(&ids[0]).exists());
        args.push("--apply".into());
        let applied = run(&c, "restore-audit-archive", &args).unwrap();
        assert_eq!(applied["archived_records"], 1);
        assert!(!paths["online"].join(&ids[0]).exists());
        assert!(paths["online"].join(&ids[1]).exists());
        let target = paths["archive"].join(format!("{}.tar.gz", ids[0]));
        let (_, files) = verify_archive(&target, "images", &ids[0]).unwrap();
        assert_eq!(files, fixture_files(&ids[0], 1700000000.));
        let retired = paths["retired"].join(&ids[0]);
        fs::create_dir(&retired).unwrap();
        fs::write(retired.join("intent.json"), &files["intent.json"]).unwrap();
        let verified = run(&c, "restore-audit-verify", &["images".into()]).unwrap();
        assert_eq!(verified["status"], "complete");
        let resumed = run(&c, "restore-audit-archive", &args).unwrap();
        assert_eq!(resumed["archived_records"], 1);
        assert!(!retired.exists());
        let observation = read_json(&tmp.path().join("audit-archive-results/images.json")).unwrap();
        assert_eq!(observation["last_complete"]["status"], "complete");
    }
    #[test]
    fn archive_rejects_extra_gzip_member() {
        let dir = tempfile::tempdir().unwrap();
        let id = "a".repeat(32);
        let files = fixture_files(&id, 1700000000.);
        let mut raw = archive_bytes(&files, "images", &id, &"a".repeat(40)).unwrap();
        let mut extra = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        extra.write_all(b"unexpected nonzero data").unwrap();
        raw.extend(extra.finish().unwrap());
        let path = dir.path().join("archive.tar.gz");
        fs::write(&path, raw).unwrap();
        assert!(verify_archive(&path, "images", &id).is_err());
    }
}
