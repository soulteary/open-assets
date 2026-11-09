use super::*;
use std::os::unix::fs::MetadataExt;
const LIMITS: [(&str, u64); 4] = [
    ("intent.json", 8192),
    ("result.json", 8192),
    ("source.json", 16384),
    ("check.json", 16384),
];
const GROUPS: [&str; 3] = ["restorations", "restoration_checks", "restoration_closures"];
fn valid_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn real_directory(path: &Path, create: bool) -> Result<()> {
    if create && !path.exists() {
        fs::create_dir(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        common::sync_directory(path.parent().context("directory parent")?)?;
    }
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(m.is_dir(), "unsafe evidence directory"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn digest_file(path: &Path, limit: u64) -> Result<String> {
    real_directory(
        path.parent()
            .context("record parent")?
            .parent()
            .context("record ancestor")?,
        false,
    )?;
    real_directory(path.parent().unwrap(), false)?;
    match common::read_regular(path, limit) {
        Ok(raw) => Ok(hex::encode(Sha256::digest(raw))),
        Err(e) => {
            if !path.try_exists()? && fs::symlink_metadata(path).is_err() {
                Ok("missing".into())
            } else {
                Err(e)
            }
        }
    }
}
fn artifacts_validate(
    artifacts: &BTreeMap<String, Vec<u8>>,
    key: &str,
    id: &str,
) -> Result<(Value, Value)> {
    let intent: Value = serde_json::from_slice(
        artifacts
            .get("intent.json")
            .context("missing audit intent")?,
    )?;
    let result: Value = serde_json::from_slice(
        artifacts
            .get("result.json")
            .context("missing audit result")?,
    )?;
    let record = info::reports::clean_closure(&intent, Some(&result), key, id)?;
    ensure!(record["status"] != "running", "unfinished audit");
    for (name, field) in [
        ("source.json", "source_report_digest"),
        ("check.json", "check_digest"),
    ] {
        if artifacts.contains_key(name) || result[field].as_str().is_some_and(|s| !s.is_empty()) {
            ensure!(
                artifacts
                    .get(name)
                    .is_some_and(|r| hex::encode(Sha256::digest(r)) == result[field]),
                "audit snapshot digest mismatch"
            );
        }
    }
    if let Some(raw) = artifacts.get("source.json") {
        let source: Value = serde_json::from_slice(raw)?;
        let clean = info::reports::clean_restore(
            &source,
            key,
            intent["sha"].as_str().context("audit SHA")?,
            intent["scope_digest"].as_str().context("audit scope")?,
        )?;
        ensure!(
            source["scope_digest"] == intent["scope_digest"]
                && clean["uncertain"] == result["source_uncertain"]
                && source["status"] == result["source_status"],
            "audit source binding mismatch"
        );
    }
    if let Some(raw) = artifacts.get("check.json") {
        let check: Value = serde_json::from_slice(raw)?;
        info::reports::clean_check(&check, key, intent["sha"].as_str().context("audit SHA")?)?;
        ensure!(
            check["source_report_digest"] == result["source_report_digest"],
            "audit check binding mismatch"
        );
    }
    if result["status"] == "closed" {
        info::reports::verify_closure_snapshots(
            artifacts.get("source.json").context("source snapshot")?,
            artifacts.get("check.json").context("check snapshot")?,
            &intent,
            &result,
        )?;
    }
    Ok((intent, result))
}
fn context(
    root: &Path,
    data: &Path,
    snapshot: &Value,
    evidence: &Value,
    key: &str,
    id: &str,
) -> Result<(Value, BTreeMap<String, Vec<u8>>)> {
    ensure!(valid_id(id), "invalid closure audit ID");
    let mut rows = BTreeMap::new();
    for group in GROUPS {
        ensure!(
            snapshot[group]["status"] == "ok",
            "current evidence unavailable"
        );
        let row = snapshot[group]["repositories"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["key"] == key))
            .context("current evidence missing")?;
        ensure!(
            row["result_status"] != "unavailable",
            "current evidence unavailable"
        );
        rows.insert(group, row);
    }
    ensure!(
        evidence["restoration_closures"][key]["audit_id"] == id
            && rows["restoration_closures"]["attempt"]["status"] == "refused",
        "only latest online closure refusal can be acknowledged"
    );
    let state = data.join("publisher");
    let base = state.join("restore-closures").join(key).join(id);
    for p in [
        &state,
        base.parent().unwrap().parent().unwrap(),
        base.parent().unwrap(),
        &base,
    ] {
        real_directory(p, false)?;
    }
    let mut artifacts = BTreeMap::new();
    for (name, limit) in LIMITS {
        let path = base.join(name);
        match common::read_regular(&path, limit) {
            Ok(raw) => {
                artifacts.insert(name.into(), raw);
            }
            Err(e) => {
                if matches!(name, "intent.json" | "result.json")
                    || path.try_exists()?
                    || fs::symlink_metadata(&path).is_ok()
                {
                    return Err(e);
                }
            }
        }
    }
    let (intent, result) = artifacts_validate(&artifacts, key, id)?;
    ensure!(result["status"] == "refused", "closure is not refused");
    let sha = intent["sha"].as_str().context("audit SHA")?;
    let digest = hex::encode(Sha256::digest(common::read_regular(
        &root.join("private/repositories.yaml"),
        1024 * 1024,
    )?));
    let host = json!({"state_root":state,"images_root":data.join("images")});
    for p in [
        data.join("images"),
        data.join("images/repos"),
        data.join("images/repos").join(key),
        state.join("published"),
    ] {
        real_directory(&p, false)?;
    }
    let mut hashes = json!({});
    for (name, raw) in &artifacts {
        hashes[name] = json!(hex::encode(Sha256::digest(raw)));
    }
    let mut latest = json!({});
    for group in GROUPS {
        latest[group] = evidence[group].get(key).cloned().unwrap_or(Value::Null);
    }
    let binding = json!({"key":key,"audit_id":id,"sha":sha,"config_digest":digest,"artifacts":hashes,"source":digest_file(&state.join("object-restores").join(key).join(format!("{sha}.json")),16384)?,"check":digest_file(&state.join("restore-checks").join(key).join(format!("{sha}.json")),16384)?,"manifest":digest_file(&state.join("manifests").join(key).join(format!("{sha}.json")),4194304)?,"references":info::reports::reference_fingerprint(&host,key)?,"latest":latest});
    Ok((binding, artifacts))
}
fn location(data: &Path, key: &str) -> Result<PathBuf> {
    let base = data.join("closure-acknowledgments");
    for p in [data.to_path_buf(), base.clone(), base.join(key)] {
        real_directory(&p, false)?;
        if p.exists() {
            let m = fs::symlink_metadata(&p)?;
            ensure!(
                m.uid() == unsafe { libc::getuid() } && m.mode() & 0o022 == 0,
                "unsafe acknowledgment parent ownership or permissions"
            );
        }
    }
    Ok(base.join(key))
}
fn read_record(base: &Path, pointer: &Value) -> Result<Value> {
    ensure!(
        pointer["schema_version"] == 1 && pointer["ack_id"].as_str().is_some_and(valid_id),
        "invalid acknowledgment pointer"
    );
    let id = pointer["ack_id"].as_str().unwrap();
    let folder = base.join(id);
    real_directory(&folder, false)?;
    let raw = common::read_regular(&folder.join("ack.json"), 16384)?;
    ensure!(
        hex::encode(Sha256::digest(&raw)) == pointer["record_digest"],
        "acknowledgment integrity mismatch"
    );
    let value: Value = serde_json::from_slice(&raw)?;
    ensure!(
        value["schema_version"] == 1 && value["status"] == "recorded" && value["ack_id"] == id,
        "invalid acknowledgment record"
    );
    let reason = value["reason"].as_str().context("acknowledgment reason")?;
    ensure!(
        (1..=256).contains(&reason.trim().chars().count())
            && !reason.chars().any(|c| c < ' ' || c == '\u{7f}'),
        "invalid acknowledgment reason"
    );
    ensure!(
        value["acknowledged_at"]
            .as_f64()
            .is_some_and(|t| t.is_finite() && t > 0.0 && t <= common::now() + 5.0),
        "invalid acknowledgment time"
    );
    let artifacts = value["binding"]["artifacts"]
        .as_object()
        .context("invalid acknowledgment binding")?;
    for (name, digest) in artifacts {
        let limit = LIMITS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, l)| *l)
            .context("invalid artifact name")?;
        ensure!(
            hex::encode(Sha256::digest(common::read_regular(
                &folder.join(name),
                limit
            )?)) == *digest,
            "acknowledgment snapshot mismatch"
        );
    }
    ensure!(
        artifacts.contains_key("intent.json")
            && artifacts.contains_key("result.json")
            && value["execution_uid"].as_u64().is_some(),
        "incomplete acknowledgment"
    );
    Ok(value)
}
pub fn summary(root: &Path, data: &Path, snapshot: &Value, evidence: &Value, row: &Value) -> Value {
    let result = (|| -> Result<Value> {
        let key = row["key"].as_str().context("key")?;
        let base = location(data, key)?;
        let pointer = base.join("latest.json");
        if !pointer.try_exists()? && fs::symlink_metadata(&pointer).is_err() {
            return Ok(json!({"status":"none"}));
        }
        let value = read_record(&base, &info::read_json_bounded(&pointer, 8192)?)?;
        let current = &evidence["restoration_closures"][key];
        if current["audit_id"] != value["binding"]["audit_id"] {
            return Ok(json!({"status":"invalid","reason":"new-attempt"}));
        }
        let (binding, _) = context(
            root,
            data,
            snapshot,
            evidence,
            key,
            current["audit_id"].as_str().context("audit ID")?,
        )?;
        if binding != value["binding"] {
            return Ok(json!({"status":"invalid","reason":"evidence-changed"}));
        }
        Ok(json!({"status":"valid","acknowledged_at":value["acknowledged_at"]}))
    })();
    result.unwrap_or(json!({"status":"unavailable","reason":"unreadable-evidence"}))
}
fn write_once(path: &Path, raw: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    f.write_all(raw)?;
    f.sync_all()?;
    common::sync_directory(path.parent().context("evidence parent")?)
}
pub fn run(root: &Path, args: &[String]) -> Result<()> {
    host_only()?;
    ensure!(args.len() >= 2, "key and audit ID required");
    let key = &args[0];
    let id = &args[1];
    let reason = flag(args, "--reason").context("bounded acknowledgment reason required")?;
    ensure!(
        regex::Regex::new(r"^[a-z][a-z0-9-]{0,47}$")?.is_match(key) && valid_id(id),
        "invalid acknowledgment identity"
    );
    ensure!(
        (1..=256).contains(&reason.trim().chars().count())
            && !reason.chars().any(|c| c < ' ' || c == '\u{7f}'),
        "bounded acknowledgment reason required"
    );
    let confirm = flag(args, "--confirm");
    if let Some(t) = confirm {
        ensure!(
            t.len() == 64
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid evidence token"
        );
    }
    let env = info::environment(root)?;
    let data = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?);
    let c = config(root)?;
    ensure!(
        c["state_root"] == "/state" && c["images_root"] == "/images",
        "unsupported state mapping"
    );
    common::repo(&c, key)?;
    let state = data.join("publisher");
    real_directory(&state, false)?;
    let path = state.join("worker.lock");
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "invalid worker lock"
    );
    let _lock = match info::lock_file(&path, true) {
        Ok(f) => f,
        Err(_) => {
            println!("{}", json!({"status":"busy","key":key,"audit_id":id}));
            return Err(common::ExitCode(2).into());
        }
    };
    let (snapshot, evidence) = info::reports::observations(root, &data)?;
    let (binding, artifacts) = context(root, &data, &snapshot, &evidence, key, id)?;
    let token = common::digest(&binding)?;
    if confirm.is_none() {
        println!(
            "{}",
            json!({"status":"preview","key":key,"audit_id":id,"evidence_token":token,"hint":"Review this refusal, then repeat the command with --confirm evidence_token; this does not close restoration."})
        );
        return Ok(());
    }
    let refusal = |ack: Option<&str>| {
        let mut v = json!({"status":"refused","reason":"evidence-changed","hint":"Evidence changed; inspect again and obtain a new preview token.","key":key,"audit_id":id});
        if let Some(ack) = ack {
            v["ack_id"] = json!(ack);
        }
        v
    };
    if confirm != Some(token.as_str()) {
        println!("{}", refusal(None));
        return Err(common::ExitCode(1).into());
    }
    let base = location(&data, key)?;
    real_directory(base.parent().context("acknowledgment parent")?, true)?;
    real_directory(&base, true)?;
    let pointer = base.join("latest.json");
    if pointer.try_exists()? || fs::symlink_metadata(&pointer).is_ok() {
        let prior = read_record(&base, &info::read_json_bounded(&pointer, 8192)?)?;
        if prior["binding"] == binding && prior["reason"] == reason.trim() {
            println!(
                "{}",
                json!({"status":"acknowledged","key":key,"audit_id":id,"ack_id":prior["ack_id"],"reused":true})
            );
            return Ok(());
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut count = 0;
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "latest.json" {
            continue;
        }
        ensure!(
            name.to_str().is_some_and(valid_id) && entry.file_type()?.is_dir(),
            "unexpected acknowledgment entry"
        );
        count += 1;
        ensure!(
            count < 1000 && std::time::Instant::now() <= deadline,
            "acknowledgment history limit"
        );
    }
    let ack_id = uuid::Uuid::new_v4().simple().to_string();
    let folder = base.join(&ack_id);
    fs::create_dir(&folder)?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    common::sync_directory(&base)?;
    for (name, raw) in &artifacts {
        write_once(&folder.join(name), raw)?;
    }
    let value = json!({"schema_version":1,"status":"recorded","ack_id":ack_id,"acknowledged_at":common::now(),"execution_uid":unsafe{libc::getuid()},"reason":reason.trim(),"binding":binding});
    write_once(
        &folder.join("ack.json"),
        &common::python_json(&value, true)?,
    )?;
    let (snapshot, evidence) = info::reports::observations(root, &data)?;
    let (current, _) = context(root, &data, &snapshot, &evidence, key, id)?;
    if current != binding {
        println!("{}", refusal(Some(&ack_id)));
        return Err(common::ExitCode(1).into());
    }
    common::atomic(
        &pointer,
        &json!({"schema_version":1,"ack_id":ack_id,"record_digest":hex::encode(Sha256::digest(common::read_regular(&folder.join("ack.json"),16384)?))}),
    )?;
    println!(
        "{}",
        json!({"status":"acknowledged","key":key,"audit_id":id,"ack_id":ack_id,"reused":false})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledgment_identity() {
        assert!(valid_id(&"a".repeat(32)));
        assert!(!valid_id("../../latest"));
    }
    #[test]
    fn immutable_evidence_cannot_be_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("source.json");
        write_once(&p, b"old").unwrap();
        assert!(write_once(&p, b"new").is_err());
        assert_eq!(fs::read(p).unwrap(), b"old");
    }
}
