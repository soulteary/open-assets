use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct ExitCode(pub i32);
impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command exited with status {}", self.0)
    }
}
impl std::error::Error for ExitCode {}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn valid_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        && s.bytes().any(|c| c != b'0')
}

pub fn relative(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('\\')
        && !s.contains('\0')
        && !s.starts_with('/')
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

fn parent_directory(path: &Path, create: bool) -> Result<(File, std::ffi::CString)> {
    // macOS exposes these root-owned system aliases; normalize only these exact
    // prefixes, never an arbitrary deployment-state symlink.
    #[cfg(target_os = "macos")]
    let owned = if let Ok(rest) = path.strip_prefix("/var") {
        Path::new("/private/var").join(rest)
    } else if let Ok(rest) = path.strip_prefix("/tmp") {
        Path::new("/private/tmp").join(rest)
    } else {
        path.to_path_buf()
    };
    #[cfg(target_os = "macos")]
    let path = owned.as_path();
    let parent = path.parent().context("missing record parent")?;
    let name = std::ffi::CString::new(path.file_name().context("missing record name")?.as_bytes())?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(if parent.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        })?;
    for part in parent.components() {
        let component = match part {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(c) => c,
            _ => anyhow::bail!("unsafe record parent"),
        };
        let component = std::ffi::CString::new(component.as_bytes())?;
        if create {
            let made = unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o700) };
            if made < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(e.into());
                }
            } else {
                directory.sync_all()?;
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok((directory, name))
}

pub fn read_regular(path: &Path, max: u64) -> Result<Vec<u8>> {
    let (parent, name) = parent_directory(path, false)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= max,
        "invalid record type or size"
    );
    let mut data = Vec::new();
    file.take(max.checked_add(1).context("invalid record limit")?)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= max, "record exceeds size limit");
    Ok(data)
}

pub fn read_json(path: &Path, max: u64) -> Result<Value> {
    Ok(serde_json::from_slice(&read_regular(path, max)?)?)
}

pub fn sync_directory(path: &Path) -> Result<()> {
    let (parent, _) = parent_directory(&path.join(".sync-sentinel"), false)?;
    parent.sync_all()?;
    Ok(())
}

pub fn atomic(path: &Path, value: &Value) -> Result<()> {
    let (parent, name) = parent_directory(path, true)?;
    let mut existing = std::mem::MaybeUninit::<libc::stat>::uninit();
    let status = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            existing.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status == 0 {
        let existing = unsafe { existing.assume_init() };
        ensure!(
            existing.st_mode & libc::S_IFMT == libc::S_IFREG,
            "unsafe record destination"
        );
    } else {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e.into());
        }
    }
    let temporary = std::ffi::CString::new(format!(".tmp{}", uuid::Uuid::new_v4().simple()))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<()> {
        file.write_all(&json_bytes(value, false, false)?)?;
        file.sync_all()?;
        let status = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
            )
        };
        if status < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        parent.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

pub fn python_json(value: &Value, sort_keys: bool) -> Result<Vec<u8>> {
    json_bytes(value, sort_keys, true)
}

fn json_bytes(value: &Value, sorted: bool, ascii: bool) -> Result<Vec<u8>> {
    fn string(s: &str, ascii: bool, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if (c as u32) < 32 || (ascii && (c as u32) >= 127) => {
                    let mut buf = [0u16; 2];
                    for u in c.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{:04x}", u));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    fn number(n: &serde_json::Number) -> String {
        if n.is_i64() || n.is_u64() {
            return n.to_string();
        }
        let f = n.as_f64().unwrap_or(0.0);
        let mut s = if f != 0.0 && (f.abs() < 1e-4 || f.abs() >= 1e16) {
            format!("{:e}", f)
        } else {
            n.to_string()
        };
        if let Some((mantissa, exponent)) = s.split_once('e') {
            if let Ok(e) = exponent.parse::<i32>() {
                s = format!(
                    "{}e{}{:02}",
                    mantissa.trim_end_matches(".0"),
                    if e < 0 { "-" } else { "+" },
                    e.abs()
                );
            }
        } else if !s.contains('.') {
            s.push_str(".0");
        }
        s
    }
    fn emit(v: &Value, sorted: bool, ascii: bool, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&number(n)),
            Value::String(s) => string(s, ascii, out),
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    emit(v, sorted, ascii, out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                let mut keys: Vec<_> = o.keys().collect();
                if sorted {
                    keys.sort();
                }
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    string(k, ascii, out);
                    out.push_str(": ");
                    emit(&o[*k], sorted, ascii, out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    emit(value, sorted, ascii, &mut out);
    Ok(out.into_bytes())
}

pub fn digest(value: &Value) -> Result<String> {
    Ok(hex::encode(Sha256::digest(python_json(value, true)?)))
}

pub fn repo<'a>(config: &'a Value, key: &str) -> Result<&'a Value> {
    let r = config["repositories"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["key"] == key))
        .context("unknown repository key")?;
    ensure!(
        r["enabled"].as_bool().unwrap_or(false),
        "repository disabled"
    );
    Ok(r)
}

pub fn load(path: Option<&Path>) -> Result<Value> {
    let default =
        std::env::var("OPEN_ASSETS_CONFIG").unwrap_or_else(|_| "/config/repositories.yaml".into());
    let bytes = read_regular(path.unwrap_or_else(|| Path::new(&default)), 8 * 1024 * 1024)?;
    let mut c: Value = serde_yaml::from_slice(&bytes).context("invalid configuration")?;
    validate(&c)?;
    c["revision"] = Value::String(hex::encode(Sha256::digest(bytes)));
    Ok(c)
}

pub fn limits(c: &Value) -> Result<Value> {
    let mut result = json!({"git_repository_bytes":2147483648u64,"git_total_bytes":8589934592u64,"git_fetch_bytes":536870912u64,"git_min_free_bytes":536870912u64,"git_timeout_seconds":120,"git_process_memory_bytes":536870912u64,"git_output_bytes":8388608,"scan_entries":200000,"queue_active_records":1000,"queue_total_records":10000,"queue_reserved_bytes":1073741824});
    if let Some(supplied) = c.get("resource_limits") {
        let map = supplied.as_object().context("invalid resource_limits")?;
        for (key, value) in map {
            ensure!(
                result.get(key).is_some() && value.as_u64().is_some_and(|n| n > 0),
                "invalid resource limit"
            );
            result[key] = value.clone();
        }
    }
    ensure!(
        result["queue_active_records"].as_u64() <= result["queue_total_records"].as_u64(),
        "active queue exceeds total"
    );
    Ok(result)
}

fn retention(c: &Value, r: &Value, objects: bool) -> Result<()> {
    let name = if objects {
        "object_retention"
    } else {
        "retention"
    };
    let mut result = if objects {
        json!({"enabled":false,"exclusive_writer":false,"keep_last":10,"keep_days":90,"min_age_hours":24,"pinned_shas":[],"max_delete_versions":2,"max_delete_objects":100,"max_delete_bytes":268435456})
    } else {
        json!({"enabled":false,"keep_last":5,"keep_days":30,"min_age_hours":24,"rollback_versions":2,"pinned_shas":[],"archive_jobs_after_days":30,"max_delete_versions":10,"max_delete_bytes":268435456,"max_archive_jobs":100})
    };
    for source in [c.get(name), r.get(name)].into_iter().flatten() {
        for (k, v) in source.as_object().context("invalid retention policy")? {
            ensure!(result.get(k).is_some(), "unknown retention option");
            result[k] = v.clone();
        }
    }
    ensure!(result["enabled"].is_boolean(), "invalid retention flag");
    if objects {
        ensure!(
            result["exclusive_writer"].is_boolean(),
            "invalid writer flag"
        );
    }
    for (k, low, high) in [
        ("keep_last", 1, 10000),
        ("keep_days", 0, 36500),
        ("min_age_hours", 1, 876000),
        ("rollback_versions", 1, 100),
        ("archive_jobs_after_days", 1, 36500),
        ("max_delete_versions", 1, if objects { 10 } else { 100 }),
        ("max_delete_objects", 1, 1000),
        ("max_delete_bytes", 1, 10737418240u64),
        ("max_archive_jobs", 1, 1000),
    ] {
        if let Some(v) = result.get(k) {
            ensure!(
                v.as_u64().is_some_and(|n| (low..=high).contains(&n)),
                "invalid retention limit"
            );
        }
    }
    let pins = result["pinned_shas"]
        .as_array()
        .context("invalid pinned versions")?;
    ensure!(
        pins.len() <= 1000 && pins.iter().all(|v| v.as_str().is_some_and(valid_sha)),
        "invalid pinned versions"
    );
    Ok(())
}

pub fn validate(c: &Value) -> Result<()> {
    ensure!(
        c["schema_version"].as_u64() == Some(1),
        "unsupported schema"
    );
    limits(c)?;
    if let Some(p) = c.get("publication") {
        let p = p.as_object().context("invalid publication settings")?;
        ensure!(
            p.keys()
                .all(|s| s == "object_workers" || s == "reference_releases"),
            "unknown publication setting"
        );
        if let Some(v) = p.get("object_workers") {
            ensure!(
                v.as_u64().is_some_and(|n| (1..=8).contains(&n)),
                "invalid worker limit"
            );
        }
        if let Some(v) = p.get("reference_releases") {
            ensure!(v.is_boolean(), "invalid reference flag");
        }
    }
    let key = Regex::new(r"^[a-z][a-z0-9-]{0,47}$")?;
    let mount = Regex::new(r"^/(?:[a-z0-9][a-z0-9-]*/)?$")?;
    let branch = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/-]*$")?;
    let mut keys = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    let mut mounts = Vec::<String>::new();
    for r in c["repositories"]
        .as_array()
        .context("repositories must be an array")?
    {
        let k = r["key"].as_str().context("missing repository key")?;
        let id = r["repository_id"]
            .as_u64()
            .context("invalid repository ID")?;
        ensure!(
            key.is_match(k) && keys.insert(k) && id > 0 && ids.insert(id),
            "invalid or duplicate repository"
        );
        for flag in ["enabled", "serve"] {
            if let Some(v) = r.get(flag) {
                ensure!(v.is_boolean(), "repository flags must be booleans");
            }
        }
        let source = r["source_directory"]
            .as_str()
            .context("missing source directory")?;
        ensure!(
            !source.is_empty()
                && !Path::new(source).is_absolute()
                && source != "."
                && !Path::new(source)
                    .components()
                    .any(|p| matches!(p, Component::ParentDir)),
            "invalid source directory"
        );
        let m = r["mount_path"].as_str().context("missing mount")?;
        ensure!(
            mount.is_match(m)
                && !mounts
                    .iter()
                    .any(|x| x == m
                        || (m != "/" && x != "/" && (m.starts_with(x) || x.starts_with(m)))),
            "mount collision"
        );
        mounts.push(m.into());
        ensure!(
            r["remote"]
                .as_str()
                .is_some_and(|s| s.starts_with("ssh://git@")),
            "SSH remote required"
        );
        ensure!(
            r["branch"]
                .as_str()
                .is_some_and(|s| branch.is_match(s) && !s.contains("..")),
            "invalid branch"
        );
        for field in ["max_files", "max_bytes", "max_file_bytes"] {
            if let Some(v) = r.get(field) {
                ensure!(
                    v.as_u64().is_some_and(|n| n > 0),
                    "invalid repository budget"
                );
            }
        }
        retention(c, r, false)?;
        retention(c, r, true)?;
    }
    for field in ["state_root", "images_root", "bucket", "endpoint"] {
        ensure!(
            c[field].as_str().is_some_and(|s| !s.is_empty()),
            "missing configuration field"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_digest_bytes() {
        let value = json!({"z":[1,"中文","😀",null],"a":true});
        assert_eq!(
            String::from_utf8(python_json(&value, true).unwrap()).unwrap(),
            r#"{"a": true, "z": [1, "\u4e2d\u6587", "\ud83d\ude00", null]}"#
        );
        assert_eq!(
            String::from_utf8(python_json(&json!([1e-7, 1e16, 1.0]), false).unwrap()).unwrap(),
            "[1e-07, 1e+16, 1.0]"
        );
    }
    #[test]
    fn durable_replace_and_refuse_symlink_read() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("record");
        atomic(&p, &json!({"ok":true})).unwrap();
        assert_eq!(read_json(&p, 1024).unwrap()["ok"], true);
        std::os::unix::fs::symlink(&p, t.path().join("link")).unwrap();
        assert!(read_json(&t.path().join("link"), 1024).is_err());
        assert!(read_json(&p, 1).is_err());
    }
    #[test]
    fn ancestor_symlinks_cannot_escape_protected_record_io() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        fs::create_dir(&state).unwrap();
        std::os::unix::fs::symlink(outside.path(), state.join("object-restores")).unwrap();
        let path = state.join("object-restores/images/report.json");
        assert!(atomic(&path, &json!({"status":"complete"})).is_err());
        assert!(!outside.path().join("images").exists());
        fs::write(outside.path().join("secret.json"), b"{}").unwrap();
        assert!(read_json(&state.join("object-restores/secret.json"), 1024).is_err());
    }
    #[test]
    fn configuration_rejects_wrong_types_and_overlap() {
        let mut c: Value =
            serde_yaml::from_str(include_str!("../../config/repositories.example.yaml")).unwrap();
        validate(&c).unwrap();
        c["resource_limits"]["git_timeout_seconds"] = json!(true);
        assert!(validate(&c).is_err());
        c["resource_limits"]["git_timeout_seconds"] = json!(120);
        c["repositories"][0]["enabled"] = json!("true");
        assert!(validate(&c).is_err());
    }
}
