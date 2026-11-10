//! Filtered host observations. Never disclose raw state, paths, credentials or
//! remote errors to the read-only dashboard.
use crate::common::{digest, limits, load, now, read_json, read_regular, valid_sha};
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn finite(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|n| n.is_finite() && (0.0..=9007199254740991.0).contains(&n))
}
fn count(v: &Value, max: u64) -> bool {
    v.as_u64().is_some_and(|n| n <= max)
}
fn allowed(v: &Value, values: &[&str]) -> bool {
    v.as_str().is_some_and(|s| values.contains(&s))
}
fn pick(v: &Value, names: &[&str]) -> Value {
    let mut o = json!({});
    for name in names {
        if let Some(x) = v.get(name) {
            o[*name] = x.clone();
        }
    }
    o
}
fn time_header(v: &Value, key: &str, sha: &str, status: &[&str]) -> Result<Value> {
    ensure!(
        v["schema_version"] == 1 && v["key"] == key && v["sha"] == sha && valid_sha(sha),
        "invalid observation identity"
    );
    ensure!(
        allowed(&v["status"], status) && finite(&v["started_at"]),
        "invalid observation status"
    );
    let mut out = pick(v, &["status", "started_at"]);
    if v["status"] != "running" {
        ensure!(
            finite(&v["finished_at"]) && v["finished_at"].as_f64() >= v["started_at"].as_f64(),
            "invalid observation time"
        );
        out["finished_at"] = v["finished_at"].clone();
    }
    Ok(out)
}
fn flags(v: &Value, out: &mut Value, names: &[&str]) -> Result<()> {
    for name in names {
        if let Some(x) = v.get(name) {
            ensure!(x.is_boolean(), "invalid observation flag");
            out[*name] = x.clone();
        }
    }
    Ok(())
}
fn counters(v: &Value, out: &mut Value, names: &[&str], max: u64) -> Result<()> {
    for name in names {
        if let Some(x) = v.get(name) {
            ensure!(count(x, max), "invalid observation count");
            out[*name] = x.clone();
        }
    }
    Ok(())
}

pub fn bucket_scope(c: &Value) -> Result<String> {
    digest(&json!([c["endpoint"], c["bucket"]]))
}
pub fn reference_fingerprint(c: &Value, key: &str) -> Result<String> {
    let state = Path::new(c["state_root"].as_str().context("state root")?);
    let p = Path::new(c["images_root"].as_str().context("image root")?)
        .join("repos")
        .join(key)
        .join("current");
    let target = match fs::symlink_metadata(&p) {
        Ok(m) => {
            ensure!(m.file_type().is_symlink(), "current must be link");
            json!(fs::read_link(&p)?
                .to_str()
                .context("invalid current target")?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(e.into()),
    };
    let p = state.join("published").join(key);
    let published = match read_json(&p, 4 << 20) {
        Ok(v) => v,
        Err(e) if !p.try_exists()? && fs::symlink_metadata(&p).is_err() => {
            let _ = e;
            Value::Null
        }
        Err(e) => return Err(e),
    };
    digest(&json!([target, published]))
}

pub fn clean_restore(v: &Value, key: &str, sha: &str, scope: &str) -> Result<Value> {
    let mut out = time_header(v, key, sha, &["running", "complete", "failed"])?;
    ensure!(
        allowed(
            &v["phase"],
            &[
                "validate",
                "git",
                "export",
                "local-preflight",
                "preflight",
                "upload",
                "verify",
                "local-install",
                "activate",
                "complete"
            ]
        ),
        "invalid restoration phase"
    );
    out["phase"] = v["phase"].clone();
    flags(
        v,
        &mut out,
        &[
            "partial_possible",
            "with_local",
            "rollback_requested",
            "local_full_rebuilt",
            "local_images_rebuilt",
            "local_verified",
            "local_partial_possible",
            "activated",
            "activation_uncertain",
        ],
    )?;
    counters(
        v,
        &mut out,
        &[
            "files",
            "verified_objects",
            "uploaded_objects",
            "repaired_objects",
        ],
        10000,
    )?;
    if v["status"] == "failed" {
        out["failure_reason"] = json!(match v["error"].as_str().unwrap_or("") {
            "ValueError" => "validation",
            "OSError" => "io",
            "PermissionError" => "permission",
            "FileNotFoundError" => "missing",
            "TimeoutError" | "TimeoutExpired" => "timeout",
            "CalledProcessError" => "git",
            "ClientError" => "storage",
            "KeyboardInterrupt" => "interrupted",
            _ => "unknown",
        });
    }
    out["scope_changed"] = json!(v["scope_digest"] != scope);
    let verified = allowed(&v["phase"], &["local-install", "activate", "complete"])
        && v["files"].as_u64().is_some()
        && v["files"] == v["verified_objects"];
    out["objects_verified"] = json!(verified);
    out["uncertain"] = json!(
        v["status"] == "running"
            || v["activation_uncertain"] == true
            || (v["status"] != "complete"
                && (v["partial_possible"] == true || v["local_partial_possible"] == true))
    );
    if v["status"] == "complete" {
        ensure!(
            v["phase"] == "complete" && verified,
            "inconsistent completed restore"
        );
    }
    if v["activated"] == true {
        ensure!(
            ["rollback_requested", "with_local", "local_verified"]
                .iter()
                .all(|k| v[*k] == true),
            "inconsistent activation"
        );
    }
    Ok(out)
}

pub fn clean_check(v: &Value, key: &str, sha: &str) -> Result<Value> {
    let mut out = time_header(v, key, sha, &["running", "complete", "failed"])?;
    ensure!(
        allowed(&v["diagnosis"], &["verified", "issues", "unresolved"])
            && allowed(
                &v["phase"],
                &[
                    "objects",
                    "full",
                    "images",
                    "references",
                    "barrier",
                    "validate",
                    "complete"
                ]
            ),
        "invalid check diagnosis"
    );
    out["diagnosis"] = v["diagnosis"].clone();
    out["phase"] = v["phase"].clone();
    let sections = ["objects", "full", "images", "references", "barrier"];
    ensure!(
        v["checks"].as_object().is_some_and(
            |x| x.len() == sections.len() && sections.iter().all(|k| x.contains_key(*k))
        ),
        "invalid check sections"
    );
    out["checks"] = json!({});
    let reasons = [
        "not-checked",
        "verified",
        "target-current",
        "other-current",
        "not-published",
        "none",
        "deleting",
        "complete",
        "restored",
        "missing",
        "permission",
        "timeout",
        "mismatch",
        "io",
        "storage",
        "interrupted",
        "unavailable",
    ];
    for name in sections {
        let x = &v["checks"][name];
        ensure!(
            allowed(&x["status"], &["ok", "blocked", "failed", "unavailable"])
                && allowed(&x["reason"], &reasons),
            "invalid check result"
        );
        out["checks"][name] = pick(x, &["status", "reason"]);
    }
    flags(
        v,
        &mut out,
        &[
            "current_is_target",
            "published_is_target",
            "source_uncertain",
        ],
    )?;
    counters(v, &mut out, &["files", "verified_objects"], 10000)?;
    if allowed(
        &v["source_status"],
        &["missing", "unavailable", "running", "failed", "complete"],
    ) {
        out["source_status"] = v["source_status"].clone();
    }
    if allowed(&v["failure_reason"], &reasons) {
        out["failure_reason"] = v["failure_reason"].clone();
    }
    if v["diagnosis"] == "verified" {
        ensure!(
            v["status"] == "complete"
                && sections.iter().all(|k| out["checks"][*k]["status"] == "ok")
                && v["files"].as_u64().is_some_and(|x| x > 0)
                && v["files"] == v["verified_objects"]
                && v["source_status"] != "unavailable",
            "inconsistent verified result"
        );
    }
    Ok(out)
}

fn directory(path: &Path) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(path) {
        ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "unsafe observation directory"
        );
    }
    Ok(())
}
fn source_record(c: &Value, key: &str, sha: &str) -> Result<(Value, String)> {
    let p = Path::new(c["state_root"].as_str().context("state root")?)
        .join("object-restores")
        .join(key)
        .join(format!("{sha}.json"));
    directory(p.parent().unwrap().parent().unwrap())?;
    directory(p.parent().unwrap())?;
    let raw = read_regular(&p, 16384)?;
    let v: Value = serde_json::from_slice(&raw)?;
    let s = bucket_scope(c)?;
    clean_restore(&v, key, sha, &s)?;
    ensure!(v["scope_digest"] == s, "source scope changed");
    Ok((v, hex::encode(Sha256::digest(raw))))
}

fn restoration_summary(c: &Value, data: &Path, checked: bool, evidence: &mut Value) -> Value {
    let temporary_record =
        regex::Regex::new(r"^tmp[a-z0-9_]{8}$").expect("constant temporary record pattern");
    let keys = c["repositories"].as_array().unwrap();
    let mut rows = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let scope = bucket_scope(c).unwrap_or_default();
    let mut host = c.clone();
    host["state_root"] = json!(data.join("publisher"));
    host["images_root"] = json!(data.join("images"));
    for r in keys {
        let key = r["key"].as_str().unwrap();
        let group = if checked {
            "restore-checks"
        } else {
            "object-restores"
        };
        let dir = data.join("publisher").join(group).join(key);
        let mut row = json!({"key":key,"result_status":"never"});
        let result = (|| -> Result<()> {
            directory(dir.parent().unwrap())?;
            directory(&dir)?;
            if !dir.try_exists()? {
                return Ok(());
            }
            let mut latest: Option<(Value, Value, String)> = None;
            for (i, e) in fs::read_dir(&dir)?.enumerate() {
                ensure!(
                    i < 512 && Instant::now() < deadline,
                    "observation scan budget"
                );
                let p = e?.path();
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("invalid observation name")?;
                if temporary_record.is_match(name) || name.starts_with(".tmp") {
                    continue;
                }
                let sha = name
                    .strip_suffix(".json")
                    .context("unexpected observation entry")?;
                ensure!(valid_sha(sha), "invalid observation name");
                let raw = read_regular(&p, 16384)?;
                let v: Value = serde_json::from_slice(&raw)?;
                let clean = if checked {
                    clean_check(&v, key, sha)?
                } else {
                    clean_restore(&v, key, sha, &scope)?
                };
                if latest.as_ref().is_none_or(|(old, _, _)| {
                    clean["started_at"].as_f64() > old["started_at"].as_f64()
                }) {
                    latest = Some((clean, v, hex::encode(Sha256::digest(raw))));
                }
            }
            if let Some((mut clean, v, report_digest)) = latest {
                let sha = v["sha"].as_str().unwrap();
                if checked {
                    clean["scope_changed"] = json!(v["scope_digest"] != scope);
                    let age = (now()
                        - clean["finished_at"]
                            .as_f64()
                            .unwrap_or(clean["started_at"].as_f64().unwrap()))
                    .round_ties_even()
                    .max(0.0);
                    clean["age_seconds"] = json!(age as u64);
                    clean["stale"] = json!(age > 86400.0);
                    let observed = (|| -> Result<()> {
                        let current = source_record(&host, key, sha)
                            .map(|(_, d)| json!(d))
                            .or_else(|e| {
                                let p = data
                                    .join("publisher/object-restores")
                                    .join(key)
                                    .join(format!("{sha}.json"));
                                if !p.try_exists()? && fs::symlink_metadata(&p).is_err() {
                                    Ok(Value::Null)
                                } else {
                                    Err(e)
                                }
                            })?;
                        clean["source_changed"] = json!(current != v["source_report_digest"]);
                        let m = read_json(
                            &data
                                .join("publisher/manifests")
                                .join(key)
                                .join(format!("{sha}.json")),
                            4 << 20,
                        )?;
                        clean["manifest_changed"] = json!(digest(&m)? != v["manifest_digest"]);
                        clean["references_changed"] =
                            json!(reference_fingerprint(&host, key)? != v["reference_digest"]);
                        Ok(())
                    })();
                    if observed.is_err() {
                        clean["observation_unavailable"] = json!(true);
                    }
                }
                row["result_status"] = json!("ok");
                row["attempt"] = clean;
                evidence[key] = json!({"sha":sha,"report_digest":report_digest});
                if checked {
                    evidence[key]["source_digest"] = v["source_report_digest"].clone();
                }
            }
            Ok(())
        })();
        if result.is_err() {
            row = json!({"key":key,"result_status":"unavailable"});
        }
        rows.push(row);
    }
    let mut result = json!({"status":"ok","repositories":rows});
    if checked {
        result["stale_seconds"] = json!(86400);
    }
    result
}

pub fn clean_closure(intent: &Value, result: Option<&Value>, key: &str, id: &str) -> Result<Value> {
    let header = |v: &Value| -> Result<()> {
        ensure!(
            v["schema_version"] == 1
                && v["key"] == key
                && v["audit_id"] == id
                && v["sha"].as_str().is_some_and(valid_sha)
                && finite(&v["started_at"]),
            "invalid closure identity"
        );
        Ok(())
    };
    header(intent)?;
    ensure!(intent["status"] == "running", "invalid closure intent");
    let Some(v) = result else {
        return Ok(json!({"status":"running","started_at":intent["started_at"]}));
    };
    header(v)?;
    ensure!(
        ["sha", "started_at", "scope_digest"]
            .iter()
            .all(|k| v[*k] == intent[*k])
            && allowed(&v["status"], &["closed", "refused", "failed"])
            && allowed(
                &v["reason"],
                &[
                    "verified",
                    "source-unavailable",
                    "ineligible",
                    "check-failed",
                    "references-missing",
                    "evidence-changed",
                    "io",
                    "permission",
                    "timeout",
                    "interrupted",
                    "unavailable",
                    "missing",
                    "mismatch",
                    "storage"
                ]
            )
            && finite(&v["finished_at"])
            && v["finished_at"].as_f64() >= intent["started_at"].as_f64(),
        "invalid closure outcome"
    );
    let mut out = pick(v, &["status", "reason", "started_at", "finished_at"]);
    if allowed(&v["source_status"], &["running", "failed", "complete"]) {
        out["source_status"] = v["source_status"].clone();
    }
    flags(
        v,
        &mut out,
        &[
            "source_uncertain",
            "current_is_target",
            "published_is_target",
        ],
    )?;
    if v["status"] == "closed" {
        ensure!(
            v["reason"] == "verified"
                && v["source_uncertain"] == true
                && v["current_is_target"].is_boolean()
                && v["current_is_target"] == v["published_is_target"],
            "inconsistent closure"
        );
    }
    Ok(out)
}

pub fn verify_closure_snapshots(
    source: &[u8],
    check: &[u8],
    intent: &Value,
    result: &Value,
) -> Result<()> {
    ensure!(
        hex::encode(Sha256::digest(source)) == result["source_report_digest"]
            && hex::encode(Sha256::digest(check)) == result["check_digest"],
        "closure evidence changed"
    );
    let source: Value = serde_json::from_slice(source)?;
    let check: Value = serde_json::from_slice(check)?;
    let key = intent["key"].as_str().context("closure key")?;
    let sha = intent["sha"].as_str().context("closure version")?;
    let scope = intent["scope_digest"].as_str().context("closure scope")?;
    ensure!(
        clean_restore(&source, key, sha, scope)?["uncertain"] == true
            && source["scope_digest"] == scope,
        "ineligible source evidence"
    );
    ensure!(
        clean_check(&check, key, sha)?["diagnosis"] == "verified"
            && check["scope_digest"] == scope
            && check["source_uncertain"] == true
            && check["source_report_digest"] == result["source_report_digest"]
            && [
                "manifest_digest",
                "reference_digest",
                "current_is_target",
                "published_is_target"
            ]
            .iter()
            .all(|k| check[*k] == result[*k])
            && allowed(
                &check["checks"]["references"]["reason"],
                &["target-current", "other-current"]
            )
            && check["started_at"].as_f64() >= intent["started_at"].as_f64()
            && check["finished_at"].as_f64() <= result["finished_at"].as_f64(),
        "invalid closure verification evidence"
    );
    Ok(())
}

fn closure_summary(c: &Value, data: &Path, evidence: &mut Value) -> Value {
    let mut rows = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let scope = bucket_scope(c).unwrap_or_default();
    let mut host = c.clone();
    host["state_root"] = json!(data.join("publisher"));
    host["images_root"] = json!(data.join("images"));
    for r in c["repositories"].as_array().unwrap() {
        let key = r["key"].as_str().unwrap();
        let parent = data.join("publisher/restore-closures").join(key);
        let mut count = 0usize;
        let mut row = json!({"key":key,"result_status":"never"});
        let done = (|| -> Result<()> {
            directory(parent.parent().unwrap())?;
            directory(&parent)?;
            let mut latest: Option<(Value, Value, Option<Value>, PathBuf)> = None;
            if parent.try_exists()? {
                for (i, e) in fs::read_dir(&parent)?.enumerate() {
                    count = i + 1;
                    ensure!(i < 512 && Instant::now() < deadline, "closure scan budget");
                    let p = e?.path();
                    let id = p
                        .file_name()
                        .and_then(|s| s.to_str())
                        .context("closure ID")?;
                    ensure!(
                        id.len() == 32
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "invalid closure ID"
                    );
                    directory(&p)?;
                    let intent = read_json(&p.join("intent.json"), 8192)?;
                    let result = if p.join("result.json").try_exists()? {
                        Some(read_json(&p.join("result.json"), 8192)?)
                    } else {
                        None
                    };
                    let record = clean_closure(&intent, result.as_ref(), key, id)?;
                    if latest.as_ref().is_none_or(|(old, _, _, _)| {
                        record["started_at"].as_f64() > old["started_at"].as_f64()
                    }) {
                        latest = Some((record, intent, result, p));
                    }
                }
            }
            row["online_attempts"] = json!(count);
            row["archive_hint"] = json!(if count >= 448 { "near-limit" } else { "none" });
            if let Some((mut record, intent, result, p)) = latest {
                if record["status"] == "closed" {
                    verify_closure_snapshots(
                        &read_regular(&p.join("source.json"), 16384)?,
                        &read_regular(&p.join("check.json"), 16384)?,
                        &intent,
                        result.as_ref().context("closure outcome")?,
                    )?;
                }
                record["scope_changed"] = json!(intent["scope_digest"] != scope);
                if let Some(v) = &result {
                    if v["source_report_digest"].is_string() {
                        let observed = (|| -> Result<()> {
                            let sha = intent["sha"].as_str().context("closure sha")?;
                            let (_, d) = source_record(&host, key, sha)?;
                            record["source_changed"] = json!(d != v["source_report_digest"]);
                            if v["manifest_digest"].is_string() {
                                record["manifest_changed"] = json!(
                                    digest(&read_json(
                                        &data
                                            .join("publisher/manifests")
                                            .join(key)
                                            .join(format!("{sha}.json")),
                                        4 << 20
                                    )?)? != v["manifest_digest"]
                                );
                            }
                            if v["reference_digest"].is_string() {
                                record["references_changed"] = json!(
                                    reference_fingerprint(&host, key)? != v["reference_digest"]
                                );
                            }
                            Ok(())
                        })();
                        if observed.is_err() {
                            record["observation_unavailable"] = json!(true);
                        }
                    }
                }
                row["result_status"] = json!("ok");
                row["attempt"] = record;
                evidence[key] = json!({"audit_id":intent["audit_id"],"sha":intent["sha"],"source_digest":result.as_ref().map(|x|x["source_report_digest"].clone())});
            }
            Ok(())
        })();
        if done.is_err() {
            row = json!({"key":key,"result_status":"unavailable","online_attempts_at_least":count,"archive_hint":if count>512{"scan-limit"}else{"unavailable"}});
        }
        rows.push(row);
    }
    json!({"status":"ok","repositories":rows})
}

fn stable(a: &Value) -> bool {
    ![
        "scope_changed",
        "source_changed",
        "manifest_changed",
        "observation_unavailable",
    ]
    .iter()
    .any(|k| a[*k] == true)
}
fn verified(a: &Value) -> bool {
    a["status"] == "complete" && a["diagnosis"] == "verified" && stable(a)
}
fn same(a: &Value, b: &Value, source: bool) -> bool {
    let k = if source {
        "source_digest"
    } else {
        "report_digest"
    };
    a["sha"].is_string()
        && a["sha"] == b["sha"]
        && a[k].as_str().is_some_and(|s| s.len() == 64)
        && a[k] == b["source_digest"]
}
pub fn annotate(snapshot: &mut Value, evidence: &Value) {
    let original = snapshot.clone();
    let groups = ["restorations", "restoration_checks", "restoration_closures"];
    for name in groups {
        if let Some(rows) = snapshot[name]["repositories"].as_array_mut() {
            for row in rows {
                let key = row["key"].as_str().unwrap_or("").to_owned();
                let a = &row["attempt"];
                let find = |group: &str| {
                    original[group]["repositories"]
                        .as_array()
                        .and_then(|rs| rs.iter().find(|r| r["key"] == key))
                        .map(|r| r["attempt"].clone())
                        .unwrap_or(Value::Null)
                };
                let check = find("restoration_checks");
                let close = find("restoration_closures");
                let own = &evidence[name][&key];
                let checked = &evidence["restoration_checks"][&key];
                let closed = &evidence["restoration_closures"][&key];
                let closed_ok = close["status"] == "closed" && stable(&close);
                let (state, reason) = if row["result_status"] == "never" {
                    ("none", "never")
                } else if row["result_status"] != "ok" || !a.is_object() {
                    ("active", "unavailable")
                } else if name == "restorations" {
                    if a["scope_changed"] == true {
                        ("active", "scope-changed")
                    } else if closed_ok && same(own, closed, false) {
                        ("resolved", "explicitly-closed")
                    } else if a["status"] == "complete" && a["uncertain"] != true {
                        ("none", "complete")
                    } else if same(own, checked, false)
                        && verified(&check)
                        && check["references_changed"] != true
                    {
                        if a["uncertain"] == true {
                            ("active", "needs-close")
                        } else {
                            ("resolved", "later-verified")
                        }
                    } else {
                        ("active", "needs-check")
                    }
                } else if name == "restoration_checks" {
                    if closed_ok
                        && same(own, closed, true)
                        && close["finished_at"].as_f64() >= a["started_at"].as_f64()
                    {
                        ("resolved", "explicitly-closed")
                    } else if verified(a) && a["source_uncertain"] != true {
                        if a["references_changed"] == true || a["stale"] == true {
                            ("historical", "verified-history")
                        } else {
                            ("none", "verified")
                        }
                    } else if verified(a) && a["references_changed"] != true && a["stale"] != true {
                        ("active", "needs-close")
                    } else {
                        ("active", "needs-check")
                    }
                } else if a["status"] == "refused"
                    && allowed(
                        &row["acknowledgment"]["status"],
                        &["invalid", "unavailable"],
                    )
                {
                    ("active", "acknowledgment-invalid")
                } else if a["status"] == "refused" && row["acknowledgment"]["status"] == "valid" {
                    ("resolved", "manually-acknowledged")
                } else if closed_ok {
                    ("resolved", "explicitly-closed")
                } else if a["status"] == "refused"
                    && a["reason"] == "ineligible"
                    && a["source_status"] == "complete"
                    && a["source_uncertain"] == false
                    && stable(a)
                {
                    ("historical", "close-not-needed")
                } else {
                    ("active", "close-review")
                };
                row["reminder"] = json!({"state":state,"reason":reason});
            }
        }
    }
}

fn scope(root: &Path) -> Result<(Value, Vec<String>)> {
    let c = load(Some(&root.join("private/repositories.yaml")))?;
    ensure!(
        c["state_root"] == "/state" && c["images_root"] == "/images",
        "unsupported host mapping"
    );
    ensure!(
        c["repositories"]
            .as_array()
            .is_some_and(|r| !r.is_empty() && r.len() <= 100),
        "repository observation limit"
    );
    let keys = c["repositories"]
        .as_array()
        .context("repositories")?
        .iter()
        .map(|r| r["key"].as_str().unwrap().to_owned())
        .collect();
    Ok((c, keys))
}

pub fn observations(root: &Path, data: &Path) -> Result<(Value, Value)> {
    let (c, _) = scope(root)?;
    let mut evidence = json!({"restorations":{},"restoration_checks":{},"restoration_closures":{}});
    let mut snapshot = json!({});
    snapshot["restorations"] = restoration_summary(&c, data, false, &mut evidence["restorations"]);
    snapshot["restoration_checks"] =
        restoration_summary(&c, data, true, &mut evidence["restoration_checks"]);
    snapshot["restoration_closures"] =
        closure_summary(&c, data, &mut evidence["restoration_closures"]);
    Ok((snapshot, evidence))
}

pub fn augment(snapshot: &mut Value, root: &Path, data: &Path) -> Result<()> {
    let (c, keys) = scope(root)?;
    let (reports, evidence) = observations(root, data)?;
    for name in ["restorations", "restoration_checks", "restoration_closures"] {
        snapshot[name] = reports[name].clone();
    }
    snapshot["object_storage"] = object_summary(&c, &keys, data);
    snapshot["resources"] = resource_summary(&c, &keys, data, root);
    snapshot["restoration_audits"] = audit_summary(&keys, data);
    snapshot["object_retention"] = retention_summary(&c, data);
    snapshot["diagnostics"] = doctor_summary(root, data);
    let observed = snapshot.clone();
    if let Some(rows) = snapshot["restoration_closures"]["repositories"].as_array_mut() {
        for row in rows {
            row["acknowledgment"] =
                crate::ops::acknowledgment_summary(root, data, &observed, &evidence, &row.clone());
        }
    }
    annotate(snapshot, &evidence);
    Ok(())
}

fn object_scope(c: &Value, keys: &[String]) -> Result<String> {
    let mut sorted = keys.to_vec();
    sorted.sort();
    digest(&json!([c["endpoint"], c["bucket"], sorted]))
}
fn object_summary(c: &Value, keys: &[String], data: &Path) -> Value {
    let Ok(scope) = object_scope(c, keys) else {
        return json!({"status":"unavailable","interval_seconds":900,"repositories":[]});
    };
    let cache = read_json(&data.join("info/objects-cache.json"), 4 << 20).unwrap_or(Value::Null);
    let valid = cache["scope"] == scope;
    let rows: Vec<Value> = keys
        .iter()
        .map(|key| {
            let v = &cache["repositories"][key];
            let mut row = json!({"key":key,"status":"never","stale":true});
            if !valid {
                return row;
            }
            if allowed(&v["status"], &["ok", "scanning", "unavailable"]) {
                row["status"] = v["status"].clone();
            }
            let m = &v["complete"];
            if count(&m["objects"], (1 << 53) - 1)
                && count(&m["bytes"], (1 << 53) - 1)
                && finite(&m["started_at"])
                && finite(&m["measured_at"])
                && m["started_at"].as_f64() <= m["measured_at"].as_f64()
            {
                for k in ["objects", "bytes", "started_at", "measured_at"] {
                    row[k] = m[k].clone();
                }
                let age = (now() - m["measured_at"].as_f64().unwrap())
                    .round_ties_even()
                    .max(0.0);
                row["age_seconds"] = json!(age as u64);
                row["stale"] = json!(age > 1800.0);
            }
            row
        })
        .collect();
    json!({"status":"ok","interval_seconds":900,"repositories":rows})
}

fn clean_resource_sample(v: &Value, keys: &[String]) -> Result<Value> {
    ensure!(
        finite(&v["measured_at"]) && count(&v["git_bytes"], (1 << 53) - 1),
        "invalid resource sample"
    );
    let repos = v["repositories"]
        .as_object()
        .context("resource repositories")?;
    ensure!(
        repos.len() == keys.len()
            && keys
                .iter()
                .all(|k| repos.get(k).is_some_and(|n| count(n, (1 << 53) - 1)))
            && repos.values().map(|n| n.as_u64().unwrap()).sum::<u64>()
                <= v["git_bytes"].as_u64().unwrap(),
        "inconsistent Git total"
    );
    let q = &v["queue"];
    ensure!(
        ["active_records", "total_records", "reserved_bytes", "bytes"]
            .iter()
            .all(|k| count(&q[*k], (1 << 53) - 1)),
        "invalid queue counts"
    );
    let locations = q["locations"].as_object().context("queue locations")?;
    ensure!(
        locations.len() == 3
            && ["jobs", "job-archive", "quarantine"]
                .iter()
                .all(|k| locations.get(*k).is_some_and(|n| count(n, (1 << 53) - 1)))
            && locations.values().map(|n| n.as_u64().unwrap()).sum::<u64>()
                == q["total_records"].as_u64().unwrap()
            && q["active_records"] == q["locations"]["jobs"]
            && q["reserved_bytes"].as_u64().unwrap()
                >= q["bytes"]
                    .as_u64()
                    .unwrap()
                    .max(q["total_records"].as_u64().unwrap() * 65536),
        "inconsistent queue sample"
    );
    Ok(
        json!({"measured_at":v["measured_at"],"git_bytes":v["git_bytes"],"repositories":v["repositories"],"queue":pick(q,&["active_records","total_records","reserved_bytes","bytes","locations"])}),
    )
}

pub fn clean_resource_events(value: &Value, keys: &[String], budget: &Value) -> Value {
    let mut result = json!({});
    for category in ["queue", "git"] {
        let v = &value[category];
        let code = v["code"].as_str().unwrap_or("");
        let key = v.get("key").unwrap_or(&Value::Null);
        if (budget.get(code).is_some() || code == "unsafe_resource_entry")
            && finite(&v["at"])
            && (key.is_null() || key.as_str().is_some_and(|k| keys.iter().any(|x| x == k)))
            && (!code.starts_with("queue_") || category == "queue")
            && (!code.starts_with("git_") || category == "git")
        {
            result[category] = json!({"code":code,"at":v["at"],"key":key});
        }
    }
    result
}

fn resource_summary(c: &Value, keys: &[String], data: &Path, root: &Path) -> Value {
    let mut out = json!({"status":"unavailable","stale":true,"repositories":[],"alerts":[],"events":{},"event_status":"unavailable"});
    let result = (|| -> Result<()> {
        let env = super::environment(root)?;
        let interval = env
            .get("INFO_RESOURCE_INTERVAL_SECONDS")
            .map(|n| n.parse::<u64>())
            .transpose()?
            .unwrap_or(900);
        let warning = env
            .get("INFO_RESOURCE_WARN_PERCENT")
            .map(|n| n.parse::<u64>())
            .transpose()?
            .unwrap_or(80);
        ensure!(
            (60..=86400).contains(&interval) && (1..=99).contains(&warning),
            "invalid resource settings"
        );
        let budget = limits(c)?;
        let mut sorted = keys.to_vec();
        sorted.sort();
        let expected = digest(&json!([sorted, budget]))?;
        let events_path = data.join("publisher/resource-limit-events.json");
        if !events_path.exists() {
            out["event_status"] = json!("never");
        } else if let Ok(events) = read_json(&events_path, 16384) {
            let clean = clean_resource_events(&events, keys, &budget);
            out["events"] = clean.clone();
            if clean.as_object().map(|m| m.len()) == events.as_object().map(|m| m.len()) {
                out["event_status"] = json!("ok");
            }
        }

        let cache = read_json(&data.join("info/resource-cache.json"), 1 << 20)?;
        ensure!(
            cache["schema_version"] == 1
                && cache["scope"] == expected
                && allowed(&cache["status"], &["ok", "busy", "unavailable"]),
            "resource scope changed"
        );
        let sample = clean_resource_sample(&cache["sample"], keys)?;
        let at = sample["measured_at"].as_f64().unwrap();
        out["status"] = cache["status"].clone();
        out["stale"] = json!(now() - at > (interval * 2) as f64 || now() < at - 5.0);
        out["interval_seconds"] = json!(interval);
        out["warning_percent"] = json!(warning);
        out["measured_at"] = sample["measured_at"].clone();
        out["budget"] = budget.clone();
        out["queue"] = sample["queue"].clone();
        out["git_bytes"] = sample["git_bytes"].clone();
        out["repositories"] = json!(keys
            .iter()
            .map(|k| json!({"key":k,"bytes":sample["repositories"][k]}))
            .collect::<Vec<_>>());
        let mut alerts = Vec::new();
        for (code, used) in [
            ("git_total_bytes", sample["git_bytes"].as_u64().unwrap()),
            (
                "queue_active_records",
                sample["queue"]["active_records"].as_u64().unwrap(),
            ),
            (
                "queue_total_records",
                sample["queue"]["total_records"].as_u64().unwrap(),
            ),
            (
                "queue_reserved_bytes",
                sample["queue"]["reserved_bytes"].as_u64().unwrap(),
            ),
        ] {
            let limit = budget[code].as_u64().unwrap();
            if (used as u128) * 100 >= (limit as u128) * (warning as u128) {
                alerts.push(json!({"code":code,"level":if used>=limit{"critical"}else{"warning"}}));
            }
        }
        for key in keys {
            let used = sample["repositories"][key].as_u64().unwrap();
            let limit = budget["git_repository_bytes"].as_u64().unwrap();
            if (used as u128) * 100 >= (limit as u128) * (warning as u128) {
                alerts.push(json!({"code":"git_repository_bytes","key":key,"level":if used>=limit{"critical"}else{"warning"}}));
            }
        }
        out["alerts"] = json!(alerts);
        Ok(())
    })();
    let _ = result;
    out
}

fn audit_summary(keys: &[String], data: &Path) -> Value {
    let mut sorted = keys.to_vec();
    sorted.sort();
    let scope = digest(&json!(["audit-cache-v1", sorted])).unwrap_or_default();
    let cache = read_json(&data.join("info/audit-cache.json"), 1 << 20).unwrap_or(Value::Null);
    let valid = cache["scope"] == scope;
    let rows:Vec<Value>=keys.iter().map(|key|{let v=&cache["repositories"][key];let mut row=json!({"key":key,"status":"never","stale":true,"archival":{"status":"never"},"changed_since_sample":false});if valid{if allowed(&v["status"],&["ok","busy","unavailable"]){row["status"]=v["status"].clone();}
if let Ok(sample)=clean_audit_sample(&v["sample"]){let age=(now()-sample["measured_at"].as_f64().unwrap()).round_ties_even().max(0.0);for(k,v)in sample.as_object().unwrap(){row[k]=v.clone();}row["age_seconds"]=json!(age as u64);row["stale"]=json!(age>1800.0);}}
let p=data.join("publisher/audit-archive-results").join(format!("{key}.json"));if p.exists(){row["archival"]=read_json(&p,16384).ok().and_then(|r|clean_archival(&r,key).ok()).unwrap_or(json!({"status":"unavailable"}));let at=row["archival"]["attempt"]["finished_at"].as_f64().or_else(||row["archival"]["attempt"]["started_at"].as_f64()).unwrap_or(0.0);row["changed_since_sample"]=json!(at>row["measured_at"].as_f64().unwrap_or(f64::MAX));}row}).collect();
    json!({"status":"ok","repositories":rows,"interval_seconds":900,"policy":{"keep_last":20,"min_age_days":30,"max_records":100}})
}
fn clean_audit_sample(v: &Value) -> Result<Value> {
    ensure!(finite(&v["measured_at"]), "invalid audit measurement time");
    let mut out = pick(v, &["measured_at"]);
    for area in ["online", "archive", "retired"] {
        ensure!(
            ["records", "bytes", "disk_bytes"]
                .iter()
                .all(|k| count(&v[area][*k], (1 << 53) - 1)),
            "invalid audit metrics"
        );
        out[area] = pick(&v[area], &["records", "bytes", "disk_bytes"]);
    }
    for k in [
        "invalid_records",
        "candidate_records",
        "candidate_bytes",
        "eligible_records",
    ] {
        ensure!(count(&v[k], (1 << 53) - 1), "invalid audit count");
        out[k] = v[k].clone();
    }
    out["protection_counts"] = clean_counts(
        &v["protection_counts"],
        &[
            "invalid",
            "unfinished",
            "keep-last",
            "unresolved-source",
            "recent",
            "batch-limit",
        ],
        (1 << 53) - 1,
    )?;
    Ok(out)
}
fn clean_counts(v: &Value, allowed: &[&str], max: u64) -> Result<Value> {
    let map = v.as_object().context("invalid count map")?;
    ensure!(
        map.iter()
            .all(|(k, v)| allowed.contains(&k.as_str()) && count(v, max)),
        "invalid filtered count"
    );
    Ok(v.clone())
}
fn clean_archival(v: &Value, key: &str) -> Result<Value> {
    ensure!(
        v["schema_version"] == 1 && v["key"] == key,
        "invalid archive identity"
    );
    let clean = |a: &Value| -> Result<Value> {
        ensure!(
            allowed(&a["status"], &["running", "complete", "failed"]) && finite(&a["started_at"]),
            "invalid archive result"
        );
        let mut r = pick(a, &["status", "started_at"]);
        if a["status"] != "running" {
            ensure!(
                finite(&a["finished_at"]) && a["finished_at"].as_f64() >= a["started_at"].as_f64(),
                "invalid archive time"
            );
            r["finished_at"] = a["finished_at"].clone();
        }
        for k in [
            "candidate_records",
            "archived_records",
            "protected_records",
            "keep_last",
            "min_age_days",
            "max_records",
        ] {
            ensure!(
                count(&a[k], if k == "min_age_days" { 36500 } else { 20000 }),
                "invalid archive count"
            );
            r[k] = a[k].clone();
        }
        ensure!(a["partial_possible"].is_boolean(), "invalid archive intent");
        r["partial_possible"] = a["partial_possible"].clone();
        r["protection_counts"] = clean_counts(
            &a["protection_counts"],
            &[
                "invalid",
                "unfinished",
                "keep-last",
                "unresolved-source",
                "recent",
                "batch-limit",
            ],
            20000,
        )?;
        if allowed(
            &a["failure_reason"],
            &[
                "missing",
                "permission",
                "timeout",
                "mismatch",
                "io",
                "storage",
                "interrupted",
                "unavailable",
            ],
        ) {
            r["failure_reason"] = a["failure_reason"].clone();
        }
        Ok(r)
    };
    let mut out = json!({"status":"ok","attempt":clean(&v["attempt"])?});
    if v.get("last_complete").is_some() {
        let x = clean(&v["last_complete"])?;
        ensure!(x["status"] == "complete", "invalid last archive completion");
        out["last_complete"] = x;
    }
    Ok(out)
}

const RETENTION_COUNTS: &[&str] = &[
    "candidate_versions",
    "candidate_objects",
    "candidate_bytes",
    "eligible_versions",
    "eligible_objects",
    "eligible_bytes",
    "unknown_objects",
    "skipped_versions",
    "removed_versions",
    "removed_objects",
    "removed_bytes",
];
const PROTECTIONS: &[&str] = &[
    "current",
    "rollback",
    "pinned",
    "previous-job",
    "keep-last",
    "recent",
    "no-current-reference",
    "missing-manifest",
    "incomplete-or-extra-objects",
    "manifest-size-mismatch",
    "job-pending",
    "job-running",
    "job-images_prepared",
    "job-failed",
    "object-reference",
];
fn clean_retention_record(v: &Value, complete: bool) -> Result<Value> {
    ensure!(
        allowed(&v["mode"], &["preview", "apply"])
            && finite(&v["started_at"])
            && finite(&v["finished_at"])
            && v["finished_at"].as_f64() >= v["started_at"].as_f64()
            && allowed(&v["status"], &["running", "complete", "failed", "busy"])
            && allowed(
                &v["reason"],
                &[
                    "started",
                    "preview-only",
                    "disabled",
                    "no-candidates",
                    "applied",
                    "budget-skipped",
                    "busy",
                    "exclusive-writer-required",
                    "versioned-bucket",
                    "time-budget",
                    "scan-limit",
                    "changed-objects",
                    "permission-denied",
                    "storage-error",
                    "unsafe-state",
                    "io-error",
                    "unknown-error"
                ]
            ),
        "invalid retention observation"
    );
    let mut out = pick(
        v,
        &["mode", "started_at", "finished_at", "status", "reason"],
    );
    if v["partial_possible"].is_boolean() {
        out["partial_possible"] = v["partial_possible"].clone();
    }
    counters(v, &mut out, RETENTION_COUNTS, (1 << 53) - 1)?;
    if complete {
        ensure!(
            v["status"] == "complete"
                && RETENTION_COUNTS.iter().all(|k| out.get(*k).is_some())
                && v["policy_digest"].as_str().is_some_and(|s| s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
            "incomplete retention result"
        );
        out["policy_digest"] = v["policy_digest"].clone();
        out["protection_counts"] = clean_counts(&v["protection_counts"], PROTECTIONS, 50000)?;
        out["skip_counts"] = clean_counts(
            v.get("skip_counts").unwrap_or(&json!({})),
            &[
                "versions-limit",
                "objects-limit",
                "bytes-limit",
                "request-budget",
                "batch-remaining",
            ],
            50000,
        )?;
    }
    Ok(out)
}
fn retention_summary(c: &Value, data: &Path) -> Value {
    let rows: Vec<Value> = c["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let key = r["key"].as_str().unwrap();
            let mut row = json!({"key":key,"result_status":"never"});
            let result = (|| -> Result<()> {
                let settings = crate::storage::maintenance::policy(c, r, true)?;
                let local = crate::storage::maintenance::policy(c, r, false)?;
                let mut public = settings.clone();
                public.as_object_mut().unwrap().remove("pinned_shas");
                let pins: BTreeSet<ValueKey> = settings["pinned_shas"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .chain(local["pinned_shas"].as_array().unwrap())
                    .map(|v| ValueKey(v.as_str().unwrap().to_owned()))
                    .collect();
                public["pinned_count"] = json!(pins.len());
                public["rollback_versions"] = local["rollback_versions"].clone();
                row["policy"] = public;
                let expected = digest(&json!([c["endpoint"], c["bucket"], key, settings, local]))?;
                let p = data
                    .join("publisher/object-retention-results")
                    .join(format!("{key}.json"));
                if fs::symlink_metadata(&p).is_err() {
                    return Ok(());
                }
                let v = read_json(&p, 262144)?;
                ensure!(
                    v["schema_version"] == 1 && v["key"] == key,
                    "invalid retention identity"
                );
                let mut records = json!({});
                let mut invalid = BTreeSet::new();
                for name in ["attempt", "last_plan", "last_preview", "last_apply"] {
                    if let Some(x) = v.get(name) {
                        if let Ok(mut x) = clean_retention_record(x, name != "attempt") {
                            if name != "attempt" {
                                let age = (now() - x["finished_at"].as_f64().unwrap())
                                    .round_ties_even()
                                    .max(0.0);
                                let changed = x["policy_digest"] != expected;
                                x.as_object_mut().unwrap().remove("policy_digest");
                                x["age_seconds"] = json!(age as u64);
                                x["policy_changed"] = json!(changed);
                                x["stale"] = json!(age > 43200.0 || changed);
                            }
                            records[name] = x;
                        } else {
                            invalid.insert(name.to_owned());
                        }
                    }
                }
                if let Some(fields) = v["invalid_fields"].as_array() {
                    for f in fields {
                        if let Some(f) = f.as_str() {
                            if ["attempt", "last_plan", "last_preview", "last_apply"].contains(&f)
                                && records.get(f).is_none()
                            {
                                invalid.insert(f.to_owned());
                            }
                        }
                    }
                }
                ensure!(
                    !records.as_object().unwrap().is_empty() || !invalid.is_empty(),
                    "missing retention observations"
                );
                for (k, v) in records.as_object().unwrap() {
                    row[k] = v.clone();
                }
                if !invalid.is_empty() {
                    row["invalid_fields"] = json!(invalid);
                }
                row["result_status"] = json!("ok");
                Ok(())
            })();
            if result.is_err() {
                row["result_status"] = json!("unavailable");
            }
            row
        })
        .collect();
    json!({"status":"ok","repositories":rows,"stale_seconds":43200})
}
#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct ValueKey(String);

const TIMERS: &[(&str, bool)] = &[
    ("info", true),
    ("objects", true),
    ("audits", true),
    ("resources", true),
    ("logrotate", true),
    ("repair", false),
    ("retention", false),
    ("object-retention", false),
];
const DOCTOR_REASONS: &[&str] = &[
    "ok",
    "running",
    "not-enabled",
    "failed",
    "condition-skipped",
    "never-or-unknown",
    "overdue",
    "unavailable",
    "fresh",
    "never-or-invalid",
    "clock-skew",
    "stale",
    "collector-busy",
    "collector-scanning",
    "collector-never",
    "collector-unavailable",
    "unavailable-or-scope-changed",
];
pub fn clean_doctor(value: &Value) -> Result<Value> {
    ensure!(
        allowed(&value["status"], &["ok", "attention"]),
        "invalid diagnostic report"
    );
    let mut result = pick(value, &["status"]);
    let services = [
        "publisher",
        "webhook",
        "info",
        "object-reader",
        "public-limiter",
        "gitea",
        "nginx",
        "traefik",
        "oc",
        "otterio",
        "stargate",
    ];
    for (group, max) in [
        ("checks", 16),
        ("timers", TIMERS.len()),
        ("caches", 401),
        ("components", services.len()),
    ] {
        let rows = value[group].as_array().context("invalid diagnostic rows")?;
        ensure!(rows.len() <= max, "diagnostic row limit");
        let mut clean = Vec::new();
        for row in rows {
            let mut out = json!({});
            if group == "components" {
                ensure!(allowed(&row["service"], &services), "unknown component");
                out["service"] = row["service"].clone();
                for (k, allowed_values) in [
                    (
                        "state",
                        &[
                            "running",
                            "exited",
                            "created",
                            "restarting",
                            "paused",
                            "dead",
                            "removing",
                            "missing",
                            "unknown",
                        ][..],
                    ),
                    (
                        "health",
                        &["healthy", "unhealthy", "starting", "unknown", ""][..],
                    ),
                ] {
                    out[k] = if allowed(&row[k], allowed_values) {
                        row[k].clone()
                    } else {
                        json!("unknown")
                    };
                }
            } else {
                ensure!(
                    allowed(
                        &row["status"],
                        if group == "timers" {
                            &["ok", "attention", "optional"][..]
                        } else {
                            &["ok", "attention"][..]
                        }
                    ),
                    "invalid diagnostic status"
                );
                let hint = row["hint"].as_str().context("missing diagnostic hint")?;
                ensure!(
                    !hint.is_empty()
                        && hint.chars().count() <= 1024
                        && !hint.chars().any(|c| c < ' '),
                    "invalid diagnostic text"
                );
                out["status"] = row["status"].clone();
                out["hint"] = json!(hint);
                if group == "checks" {
                    ensure!(
                        allowed(
                            &row["check"],
                            &[
                                "configuration",
                                "containers",
                                "maintenance",
                                "timers",
                                "info-snapshot",
                                "caches"
                            ]
                        ),
                        "unknown diagnostic check"
                    );
                    out["check"] = row["check"].clone();
                } else {
                    ensure!(
                        allowed(&row["reason"], DOCTOR_REASONS),
                        "unknown diagnostic reason"
                    );
                    out["reason"] = row["reason"].clone();
                }
                if group == "timers" {
                    let t = TIMERS
                        .iter()
                        .find(|(n, _)| row["timer"] == *n)
                        .context("unknown timer")?;
                    ensure!(row["required"] == t.1, "timer requirement mismatch");
                    out["timer"] = row["timer"].clone();
                    out["required"] = row["required"].clone();
                    for (k, values) in [
                        (
                            "active",
                            &[
                                "active",
                                "inactive",
                                "failed",
                                "activating",
                                "deactivating",
                                "reloading",
                            ][..],
                        ),
                        (
                            "enabled",
                            &[
                                "enabled",
                                "enabled-runtime",
                                "disabled",
                                "static",
                                "masked",
                                "masked-runtime",
                                "linked",
                                "linked-runtime",
                                "indirect",
                                "alias",
                                "generated",
                                "transient",
                                "unknown",
                                "",
                            ][..],
                        ),
                        (
                            "result",
                            &[
                                "success",
                                "exit-code",
                                "signal",
                                "core-dump",
                                "timeout",
                                "watchdog",
                                "start-limit-hit",
                                "resources",
                                "protocol",
                                "oom-kill",
                                "exec-condition",
                                "unknown",
                            ][..],
                        ),
                    ] {
                        if row.get(k).is_some() {
                            out[k] = if allowed(&row[k], values) {
                                row[k].clone()
                            } else {
                                json!("unknown")
                            };
                        }
                    }
                    if let Some(v) = row.get("exit_code") {
                        ensure!(
                            v.as_f64()
                                .is_some_and(|n| n.is_finite() && (0.0..=1e12).contains(&n)),
                            "invalid exit code"
                        );
                        out["exit_code"] = v.clone();
                    }
                }
                if group == "caches" {
                    ensure!(
                        allowed(
                            &row["cache"],
                            &["local-versions", "objects", "audits", "resources"]
                        ),
                        "unknown diagnostic cache"
                    );
                    out["cache"] = row["cache"].clone();
                    if let Some(v) = row.get("key") {
                        if !v.is_null() {
                            ensure!(
                                v.as_str().is_some_and(crate::storage::valid_key),
                                "invalid diagnostic key"
                            );
                            out["key"] = v.clone();
                        }
                    }
                }
                for k in ["age_seconds", "max_age_seconds"] {
                    if let Some(v) = row.get(k) {
                        if !v.is_null() {
                            ensure!(
                                v.as_f64()
                                    .is_some_and(|n| n.is_finite() && (0.0..=1e12).contains(&n)),
                                "invalid diagnostic age"
                            );
                            out[k] = v.clone();
                        }
                    }
                }
            }
            clean.push(out);
        }
        result[group] = json!(clean);
    }
    let timers: BTreeSet<&str> = result["timers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["timer"].as_str())
        .collect();
    ensure!(
        timers == TIMERS.iter().map(|(n, _)| *n).collect(),
        "incomplete timers"
    );
    let checks: BTreeSet<&str> = result["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["check"].as_str())
        .collect();
    ensure!(
        [
            "configuration",
            "containers",
            "timers",
            "info-snapshot",
            "caches"
        ]
        .iter()
        .all(|k| checks.contains(k)),
        "incomplete checks"
    );
    Ok(result)
}
fn doctor_scope(root: &Path) -> String {
    hex::encode(Sha256::digest(
        read_regular(&root.join("private/repositories.yaml"), 1 << 20)
            .unwrap_or_else(|_| b"missing-or-unreadable".to_vec()),
    ))
}
fn doctor_summary(root: &Path, data: &Path) -> Value {
    let result = (|| -> Result<Value> {
        let env = super::environment(root)?;
        let interval = env
            .get("INFO_DOCTOR_INTERVAL_SECONDS")
            .map(|x| x.parse::<u64>())
            .transpose()?
            .unwrap_or(300);
        ensure!((60..=86400).contains(&interval), "invalid doctor interval");
        let v = read_json(&data.join("info/doctor-cache.json"), 1 << 20)?;
        ensure!(
            v["schema_version"] == 1
                && v["scope"] == doctor_scope(root)
                && allowed(&v["status"], &["ok", "running", "unavailable"]),
            "doctor scope changed"
        );
        let mut out = pick(&v, &["status", "measured_at", "attempted_at"]);
        out["interval_seconds"] = json!(interval);
        out["measured_at"] = v.get("measured_at").cloned().unwrap_or(Value::Null);
        out["attempted_at"] = v.get("attempted_at").cloned().unwrap_or(Value::Null);
        let report = v.get("report").map(clean_doctor).transpose()?;
        let at = v["measured_at"]
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=1e12).contains(n));
        let valid = at.is_some() && report.is_some();
        let age = at.map(|t| (now() - t).round_ties_even().max(0.0));
        out["age_seconds"] = if valid {
            json!(age.unwrap() as u64)
        } else {
            Value::Null
        };
        out["stale"] =
            json!(!valid || at.unwrap() > now() + 5.0 || age.unwrap() > (interval * 2) as f64);
        if let Some(report) = report {
            out["report"] = report;
        }
        Ok(out)
    })();
    result.unwrap_or(json!({"status":"unavailable","stale":true}))
}

fn worker_config(c: &Value, data: &Path) -> Value {
    let mut host = c.clone();
    host["state_root"] = json!(data.join("publisher"));
    host["images_root"] = json!(data.join("images"));
    host
}
fn resource_lock(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(f.metadata()?.is_file(), "unsafe observation lock");
    fs2::FileExt::try_lock_exclusive(&f)?;
    Ok(f)
}
fn logical_usage(root: &Path, max: u64, deadline: Instant) -> Result<u64> {
    match fs::symlink_metadata(root) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    }
    let mut stack = vec![root.to_path_buf()];
    let (mut bytes, mut entries) = (0u64, 0u64);
    while let Some(dir) = stack.pop() {
        ensure!(Instant::now() < deadline, "observation time budget");
        directory(&dir)?;
        for e in fs::read_dir(dir)? {
            ensure!(Instant::now() < deadline, "observation time budget");
            entries += 1;
            ensure!(entries <= max, "observation scan budget");
            let p = e?.path();
            let m = match fs::symlink_metadata(&p) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if m.is_dir() {
                stack.push(p);
            } else {
                ensure!(m.is_file(), "unsafe resource entry");
                bytes = bytes
                    .checked_add(m.len())
                    .context("resource size overflow")?;
            }
        }
    }
    Ok(bytes)
}
fn queue_usage(state: &Path, max: u64, deadline: Instant) -> Result<Value> {
    let (mut total, mut active, mut reserved, mut bytes, mut scanned) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut locations = json!({});
    for name in ["jobs", "job-archive", "quarantine"] {
        let root = state.join(name);
        let mut count = 0u64;
        if fs::symlink_metadata(&root).is_ok() {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                directory(&dir)?;
                for e in fs::read_dir(dir)? {
                    scanned += 1;
                    ensure!(
                        scanned <= max && Instant::now() < deadline,
                        "queue observation budget"
                    );
                    let p = e?.path();
                    let m = fs::symlink_metadata(&p)?;
                    if m.is_dir() {
                        stack.push(p);
                    } else {
                        count += 1;
                        bytes = bytes.checked_add(m.len()).context("queue size overflow")?;
                        reserved = reserved
                            .checked_add(m.len().max(65536))
                            .context("queue reservation overflow")?;
                    }
                }
            }
        }
        locations[name] = json!(count);
        total += count;
        if name == "jobs" {
            active = count;
        }
    }
    Ok(
        json!({"active_records":active,"total_records":total,"reserved_bytes":reserved,"bytes":bytes,"locations":locations}),
    )
}
fn collect_resources(
    c: &Value,
    keys: &[String],
    data: &Path,
    root: &Path,
    force: bool,
) -> Result<Value> {
    let env = super::environment(root)?;
    let interval = env
        .get("INFO_RESOURCE_INTERVAL_SECONDS")
        .map(|x| x.parse::<u64>())
        .transpose()?
        .unwrap_or(900);
    ensure!(
        (60..=86400).contains(&interval),
        "invalid resource interval"
    );
    let budget = limits(c)?;
    let mut sorted = keys.to_vec();
    sorted.sort();
    let scope = digest(&json!([sorted, budget]))?;
    let previous =
        read_json(&data.join("info/resource-cache.json"), 1 << 20).unwrap_or(Value::Null);
    let mut cache = json!({"schema_version":1,"scope":scope,"status":"never"});
    if previous["scope"] == scope {
        if allowed(&previous["status"], &["ok", "busy", "unavailable"]) {
            cache["status"] = previous["status"].clone();
        }
        if finite(&previous["attempted_at"]) {
            cache["attempted_at"] = previous["attempted_at"].clone();
        }
        if let Ok(sample) = clean_resource_sample(&previous["sample"], keys) {
            cache["sample"] = sample;
        }
    }
    if !force
        && cache["status"] != "busy"
        && cache["attempted_at"]
            .as_f64()
            .is_some_and(|t| now() >= t && now() - t < interval as f64)
    {
        return Ok(cache);
    }
    let result = (|| -> Result<Value> {
        let state = data.join("publisher");
        let _worker = resource_lock(&state.join("worker.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let max = budget["scan_entries"].as_u64().unwrap();
        let git = state.join("git");
        let total = logical_usage(&git, max, deadline)?;
        let mut repos = json!({});
        for key in keys {
            repos[key] = json!(logical_usage(&git.join(key), max, deadline)?);
        }
        let _queue = resource_lock(&state.join("queue.lock"))?;
        let sample = json!({"measured_at":now(),"git_bytes":total,"repositories":repos,"queue":queue_usage(&state,max,deadline)?});
        clean_resource_sample(&sample, keys)
    })();
    match result {
        Ok(sample) => {
            cache["status"] = json!("ok");
            cache["attempted_at"] = json!(now());
            cache["sample"] = sample;
        }
        Err(e) => {
            let busy = e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock);
            cache["status"] = json!(if busy { "busy" } else { "unavailable" });
            if !busy {
                cache["attempted_at"] = json!(now());
            }
        }
    }
    Ok(cache)
}
fn collect_audits(c: &Value, keys: &[String], data: &Path, force: bool) -> Result<Value> {
    use std::os::unix::fs::MetadataExt;
    let mut sorted = keys.to_vec();
    sorted.sort();
    let scope = digest(&json!(["audit-cache-v1", sorted]))?;
    let previous = read_json(&data.join("info/audit-cache.json"), 1 << 20).unwrap_or(Value::Null);
    let mut cache = json!({"schema_version":1,"scope":scope,"repositories":{}});
    if previous["scope"] == scope {
        for key in keys {
            let v = &previous["repositories"][key];
            let mut row = json!({});
            if allowed(&v["status"], &["ok", "busy", "unavailable"]) {
                row["status"] = v["status"].clone();
            }
            if finite(&v["attempted_at"]) {
                row["attempted_at"] = v["attempted_at"].clone();
            }
            if let Ok(sample) = clean_audit_sample(&v["sample"]) {
                row["sample"] = sample;
            }
            cache["repositories"][key] = row;
        }
    }
    let mut due: Vec<String> = keys
        .iter()
        .filter(|k| {
            force
                || cache["repositories"][*k]["attempted_at"]
                    .as_f64()
                    .is_none_or(|t| now() - t >= 900.0)
        })
        .cloned()
        .collect();
    due.sort_by(|a, b| {
        cache["repositories"][a]["attempted_at"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(
                &cache["repositories"][b]["attempted_at"]
                    .as_f64()
                    .unwrap_or(0.0),
            )
    });
    if due.is_empty() {
        return Ok(cache);
    }
    let lock = resource_lock(&data.join("publisher/worker.lock"));
    if lock.as_ref().err().is_some_and(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
    }) {
        for key in due {
            cache["repositories"][&key]["status"] = json!("busy");
        }
        return Ok(cache);
    }
    let _lock = lock?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let host = worker_config(c, data);
    for key in due {
        if !cache["repositories"][&key].is_object() {
            cache["repositories"][&key] = json!({});
        }
        cache["repositories"][&key]["attempted_at"] = json!(now());
        let sample = (|| -> Result<Value> {
            ensure!(Instant::now() < deadline, "audit time budget");
            let (paths, rows) = crate::storage::maintenance::audit_scan(
                &host,
                &key,
                deadline.min(Instant::now() + Duration::from_secs(30)),
            )?;
            let plan = crate::storage::maintenance::audit_plan(&rows, None, 20, 30, 100, now())?;
            ensure!(plan["status"] == "complete", "audit plan unavailable");
            let mut sample = json!({"measured_at":now(),"invalid_records":rows.iter().filter(|r|r["invalid"]==true).count(),"candidate_records":plan["candidates"].as_array().context("audit candidates")?.len(),"candidate_bytes":plan["candidate_bytes"],"eligible_records":plan["candidates"].as_array().unwrap().len()as u64+plan["protection_counts"]["batch-limit"].as_u64().unwrap_or(0),"protection_counts":plan["protection_counts"]});
            let mut scanned = 0;
            for name in ["online", "archive", "retired"] {
                let p = &paths[name];
                let records = rows
                    .iter()
                    .filter(|r| {
                        r["locations"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|x| x == name))
                    })
                    .count();
                let (mut bytes, mut disk) = (0u64, 0u64);
                if p.exists() {
                    directory(p)?;
                    disk += fs::symlink_metadata(p)?.blocks() * 512;
                    for e in fs::read_dir(p)? {
                        scanned += 1;
                        ensure!(
                            scanned <= 120000 && Instant::now() < deadline,
                            "audit measurement budget"
                        );
                        let p = e?.path();
                        let m = fs::symlink_metadata(&p)?;
                        disk += m.blocks() * 512;
                        if m.is_file() {
                            bytes += m.len();
                        } else {
                            ensure!(
                                m.is_dir() && name != "archive",
                                "unsafe audit measurement entry"
                            );
                            for e in fs::read_dir(&p)? {
                                scanned += 1;
                                ensure!(
                                    scanned <= 120000 && Instant::now() < deadline,
                                    "audit measurement budget"
                                );
                                let m = fs::symlink_metadata(e?.path())?;
                                ensure!(m.is_file(), "unsafe audit artifact");
                                bytes += m.len();
                                disk += m.blocks() * 512;
                            }
                        }
                    }
                }
                sample[name] = json!({"records":records,"bytes":bytes,"disk_bytes":disk});
            }
            clean_audit_sample(&sample)
        })();
        match sample {
            Ok(sample) => {
                cache["repositories"][&key]["status"] = json!("ok");
                cache["repositories"][&key]["sample"] = sample;
            }
            Err(_) => cache["repositories"][&key]["status"] = json!("unavailable"),
        }
    }
    Ok(cache)
}
fn sanitize_objects(v: &Value, c: &Value, keys: &[String]) -> Result<Value> {
    let scope = object_scope(c, keys)?;
    let mut out = json!({"schema_version":1,"scope":scope,"repositories":{}});
    if v["scope"] != scope {
        return Ok(out);
    }
    for key in keys {
        let old = &v["repositories"][key];
        if !old.is_object() {
            continue;
        }
        let mut row = json!({});
        if allowed(&old["status"], &["ok", "scanning", "unavailable"]) {
            row["status"] = old["status"].clone();
        }
        if finite(&old["attempted_at"]) {
            row["attempted_at"] = old["attempted_at"].clone();
        }
        let m = &old["complete"];
        if finite(&m["started_at"])
            && finite(&m["measured_at"])
            && m["started_at"].as_f64() <= m["measured_at"].as_f64()
            && count(&m["objects"], (1 << 53) - 1)
            && count(&m["bytes"], (1 << 53) - 1)
        {
            row["complete"] = pick(m, &["objects", "bytes", "started_at", "measured_at"]);
        }
        let p = &old["progress"];
        if finite(&p["started_at"])
            && count(&p["objects"], (1 << 53) - 1)
            && count(&p["bytes"], (1 << 53) - 1)
            && p["token"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 16384)
        {
            row["progress"] = pick(p, &["started_at", "objects", "bytes", "token"]);
        }
        out["repositories"][key] = row;
    }
    Ok(out)
}
fn object_worker(c: &Value, previous: &Value) -> Result<Value> {
    let keys: Vec<String> = c["repositories"]
        .as_array()
        .context("repositories")?
        .iter()
        .map(|r| r["key"].as_str().unwrap().to_owned())
        .collect();
    let mut cache = sanitize_objects(previous, c, &keys)?;
    let client = crate::storage::Client::from_config(c)?;
    let deadline = Instant::now() + Duration::from_secs(110);
    let mut pending = keys.clone();
    pending.sort_by(|a, b| {
        cache["repositories"][a]["attempted_at"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(
                &cache["repositories"][b]["attempted_at"]
                    .as_f64()
                    .unwrap_or(0.0),
            )
    });
    let mut pages = 0;
    let mut touched = BTreeSet::new();
    let mut next = Instant::now();
    while !pending.is_empty() && pages < 100 && Instant::now() < deadline {
        let key = pending.remove(0);
        let mut row = cache["repositories"][&key].clone();
        if !row.is_object() {
            row = json!({});
        }
        if row["attempted_at"]
            .as_f64()
            .is_some_and(|at| now() - at < 900.0)
        {
            continue;
        }
        let mut progress = row["progress"].clone();
        if !progress.is_object()
            || progress["started_at"]
                .as_f64()
                .is_none_or(|at| now() - at > 86400.0)
        {
            progress = json!({"started_at":now(),"objects":0,"bytes":0});
        }
        let prefix = format!("repositories/{key}/");
        if next > Instant::now() {
            std::thread::sleep(next - Instant::now());
        }
        if Instant::now() >= deadline {
            break;
        }
        next = Instant::now() + Duration::from_secs(1);
        pages += 1;
        touched.insert(key.clone());
        let scanned = (|| -> Result<()> {
            let page = client.list(&prefix, progress["token"].as_str(), 1000)?;
            let contents = page["Contents"]
                .as_array()
                .context("object listing contents")?;
            let mut count = progress["objects"].as_u64().unwrap();
            let mut bytes = progress["bytes"].as_u64().unwrap();
            for item in contents {
                ensure!(
                    item["Key"].as_str().is_some_and(|s| s.starts_with(&prefix)),
                    "listing escaped scope"
                );
                count = count.checked_add(1).context("object count overflow")?;
                bytes = bytes
                    .checked_add(item["Size"].as_u64().context("invalid object size")?)
                    .context("object bytes overflow")?;
            }
            ensure!(count < (1 << 53) && bytes < (1 << 53), "object total range");
            let truncated = page["IsTruncated"]
                .as_bool()
                .context("missing truncation flag")?;
            if truncated {
                let token = page["NextContinuationToken"]
                    .as_str()
                    .context("missing continuation")?;
                ensure!(
                    !token.is_empty() && token.len() <= 16384 && progress["token"] != token,
                    "invalid continuation"
                );
                row["status"] = json!("scanning");
                row["progress"] = json!({"started_at":progress["started_at"],"objects":count,"bytes":bytes,"token":token});
                pending.push(key.clone());
            } else {
                row["status"] = json!("ok");
                row["attempted_at"] = json!(now());
                row["complete"] = json!({"started_at":progress["started_at"],"measured_at":now(),"objects":count,"bytes":bytes});
                row.as_object_mut().unwrap().remove("progress");
            }
            Ok(())
        })();
        if scanned.is_err() {
            row.as_object_mut().unwrap().remove("progress");
            row["status"] = json!("unavailable");
            row["attempted_at"] = json!(now());
        }
        cache["repositories"][&key] = row;
    }
    for key in touched {
        if cache["repositories"][&key]["status"] == "scanning" {
            cache["repositories"][&key]["attempted_at"] = json!(now());
        }
    }
    cache["request_pages"] = json!(pages);
    Ok(cache)
}
fn invoke_object_worker(root: &Path, previous: &Value) -> Result<Value> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut input = tempfile::tempfile()?;
    input.write_all(&serde_json::to_vec(previous)?)?;
    input.seek(SeekFrom::Start(0))?;
    let mut output = tempfile::tempfile()?;
    let mut cmd = Command::new("docker");
    cmd.args([
        "compose",
        "--project-directory",
        root.to_str().context("deployment root")?,
        "exec",
        "-T",
        "publisher",
        "open-assets",
        "object-info",
        "--worker",
    ])
    .stdin(Stdio::from(input))
    .stdout(Stdio::from(output.try_clone()?))
    .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let started = Instant::now();
    let result = (|| -> Result<Value> {
        loop {
            ensure!(
                started.elapsed() < Duration::from_secs(130) && output.metadata()?.len() <= 4 << 20,
                "object collector subprocess budget"
            );
            if let Some(s) = child.try_wait()? {
                ensure!(s.success(), "object worker failed");
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        output.seek(SeekFrom::Start(0))?;
        let mut raw = Vec::new();
        output.take((4 << 20) + 1).read_to_end(&mut raw)?;
        ensure!(raw.len() <= 4 << 20, "object cache size");
        Ok(serde_json::from_slice(&raw)?)
    })();
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    result
}
pub fn run(command: &str, args: &[String], _c: &Value) -> Result<()> {
    if command == "object-info" && args.iter().any(|a| a == "--worker") {
        let config = load(None)?;
        let mut input = Vec::new();
        std::io::stdin()
            .take((4 << 20) + 1)
            .read_to_end(&mut input)?;
        ensure!(input.len() <= 4 << 20, "object input size");
        let cache = object_worker(&config, &serde_json::from_slice(&input)?)?;
        println!("{}", cache);
        return Ok(());
    }
    crate::ops::host_only()?;
    let root = crate::ops::protected_root(&crate::ops::root_arg(args))?;
    let (config, keys) = scope(&root)?;
    let env = super::environment(&root)?;
    let data = PathBuf::from(env.get("DATA_ROOT").context("data root")?);
    let directory = data.join("info");
    fs::create_dir_all(&directory)?;
    let force = args.iter().any(|s| s == "--force");
    let (file, lock) = match command {
        "resource-info" => ("resource-cache.json", ".resources.lock"),
        "audit-info" => ("audit-cache.json", ".audits.lock"),
        "doctor-info" => ("doctor-cache.json", ".doctor.lock"),
        "object-info" => ("objects-cache.json", ".objects.lock"),
        _ => return Err(anyhow!("unknown observation command")),
    };
    let _guard = super::lock_file(&directory.join(lock), true)?;
    let path = directory.join(file);
    let cache = match command {
        "resource-info" => collect_resources(&config, &keys, &data, &root, force)?,
        "audit-info" => collect_audits(&config, &keys, &data, force)?,
        "object-info" => {
            let old = sanitize_objects(
                &read_json(&path, 4 << 20).unwrap_or(Value::Null),
                &config,
                &keys,
            )?;
            match invoke_object_worker(&root, &old) {
                Ok(v) => sanitize_objects(&v, &config, &keys)?,
                Err(_) => {
                    let mut old = old;
                    for key in &keys {
                        if !old["repositories"][key].is_object() {
                            old["repositories"][key] = json!({});
                        }
                        old["repositories"][key]["status"] = json!("unavailable");
                    }
                    super::write_snapshot(&path, old)?;
                    return Err(anyhow!("object collector unavailable"));
                }
            }
        }
        "doctor-info" => {
            let scope = doctor_scope(&root);
            let old = read_json(&path, 1 << 20).unwrap_or(Value::Null);
            let interval = env
                .get("INFO_DOCTOR_INTERVAL_SECONDS")
                .map(|x| x.parse::<u64>())
                .transpose()?
                .unwrap_or(300);
            ensure!((60..=86400).contains(&interval), "invalid doctor interval");
            if !force
                && old["scope"] == scope
                && old["status"] != "running"
                && old["attempted_at"]
                    .as_f64()
                    .is_some_and(|t| now() >= t && now() - t < interval as f64)
            {
                return Ok(());
            }
            let mut cache =
                json!({"schema_version":1,"scope":scope,"status":"running","attempted_at":now()});
            if old["scope"] == scope {
                if let Ok(report) = clean_doctor(&old["report"]) {
                    cache["report"] = report;
                    if finite(&old["measured_at"]) {
                        cache["measured_at"] = old["measured_at"].clone();
                    }
                }
            }
            super::write_snapshot(&path, cache.clone())?;
            match clean_doctor(&crate::ops::doctor(&root)) {
                Ok(report) => {
                    cache["status"] = json!("ok");
                    cache["measured_at"] = json!(now());
                    cache["report"] = report;
                }
                Err(_) => cache["status"] = json!("unavailable"),
            };
            cache
        }
        _ => unreachable!(),
    };
    super::write_snapshot(&path, cache)?;
    println!("Filtered observation cache updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_is_filtered_and_uncertainty_preserved() {
        let sha = "a".repeat(40);
        let v = json!({"schema_version":1,"key":"assets","sha":sha,"status":"failed","phase":"upload","started_at":1,"finished_at":2,"scope_digest":"scope","partial_possible":true,"error":"ClientError","secret":"never render"});
        let r = clean_restore(&v, "assets", &sha, "scope").unwrap();
        assert_eq!(r["uncertain"], true);
        assert_eq!(r["failure_reason"], "storage");
        assert!(r.get("secret").is_none());
    }
    #[test]
    fn false_verified_result_is_rejected() {
        let sha = "a".repeat(40);
        let v = json!({"schema_version":1,"key":"assets","sha":sha,"status":"complete","phase":"complete","diagnosis":"verified","started_at":1,"finished_at":2,"checks":{},"files":1,"verified_objects":1});
        assert!(clean_check(&v, "assets", &sha).is_err());
    }
}
