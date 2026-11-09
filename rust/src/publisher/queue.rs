use super::{hex_string, valid_key, valid_sha};
use crate::common::{atomic, now, read_json, repo};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};
pub struct Lock(File);
impl Lock {
    pub fn acquire(c: &Value, name: &str, blocking: bool) -> Result<Self> {
        ensure!(["queue", "worker"].contains(&name), "invalid lock");
        let root = Path::new(c["state_root"].as_str().context("state_root")?);
        fs::create_dir_all(root)?;
        ensure!(
            fs::symlink_metadata(root)?.is_dir(),
            "unsafe state directory"
        );
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(root.join(format!("{name}.lock")))?;
        ensure!(file.metadata()?.is_file(), "unsafe lock");
        if blocking {
            fs2::FileExt::lock_exclusive(&file)?
        } else {
            fs2::FileExt::try_lock_exclusive(&file)?
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
pub fn identity(key: &str, sha: &str, kind: &str) -> String {
    hex::encode(Sha256::digest(format!("{key}:{sha}:{kind}").as_bytes()))
}
pub fn read_job(path: &Path) -> Result<Value> {
    let j = read_json(path, 65536)?;
    let key = j["key"].as_str().context("job key")?;
    let sha = j["sha"].as_str().context("job SHA")?;
    let kind = j["kind"].as_str().context("job kind")?;
    let created = j["created"].as_f64().context("job created")?;
    let id = identity(key, sha, kind);
    ensure!(
        valid_key(key)
            && valid_sha(sha)
            && ["auto", "manual"].contains(&kind)
            && [
                "pending",
                "running",
                "images_prepared",
                "published",
                "failed",
                "superseded"
            ]
            .contains(&j["status"].as_str().unwrap_or(""))
            && created.is_finite()
            && (0. ..=1e12).contains(&created)
            && j["delivery"]
                .as_str()
                .is_some_and(|s| s.chars().count() <= 128)
            && j["config"].as_str().is_some_and(|s| !s.is_empty())
            && j["id"] == id
            && path.file_name().and_then(|s| s.to_str()) == Some(&format!("{id}.json")),
        "invalid job structure"
    );
    Ok(j)
}
fn admission(c: &Value) -> Result<()> {
    let root = Path::new(c["state_root"].as_str().context("state_root")?);
    let (mut active, mut total, mut reserved, mut count) = (0u64, 0u64, 0u64, 0u64);
    for name in ["jobs", "job-archive", "quarantine"] {
        let p = root.join(name);
        if !p.try_exists()? {
            continue;
        }
        ensure!(fs::symlink_metadata(&p)?.is_dir(), "unsafe queue entry");
        let mut stack = vec![p];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir)? {
                count += 1;
                ensure!(
                    count <= super::git::limit(c, "scan_entries"),
                    "scan_entries"
                );
                let p = entry?.path();
                let m = fs::symlink_metadata(&p)?;
                if m.is_dir() {
                    stack.push(p)
                } else {
                    total += 1;
                    active += u64::from(name == "jobs");
                    reserved = reserved
                        .checked_add(m.len().max(65536))
                        .context("queue accounting overflow")?;
                }
            }
        }
    }
    ensure!(
        active < super::git::limit(c, "queue_active_records"),
        "queue_active_records"
    );
    ensure!(
        total < super::git::limit(c, "queue_total_records"),
        "queue_total_records"
    );
    ensure!(
        reserved + 65536 <= super::git::limit(c, "queue_reserved_bytes"),
        "queue_reserved_bytes"
    );
    Ok(())
}
pub fn enqueue(c: &Value, key: &str, sha: &str, delivery: &str, kind: &str) -> Result<String> {
    repo(c, key)?;
    ensure!(
        valid_sha(sha) && ["auto", "manual"].contains(&kind),
        "invalid commit"
    );
    let _lock = Lock::acquire(c, "queue", true)?;
    let id = identity(key, sha, kind);
    let state = Path::new(c["state_root"].as_str().context("state_root")?);
    let p = state.join("jobs").join(format!("{id}.json"));
    let archived = state
        .join("job-archive")
        .join(&id[..2])
        .join(format!("{id}.json"));
    if fs::symlink_metadata(&p).is_ok() || fs::symlink_metadata(archived).is_ok() {
        return Ok(id);
    }
    if let Err(e) = admission(c) {
        super::record_limit(c, &e.to_string(), key, "queue");
        return Err(e);
    };
    atomic(
        &p,
        &json!({"id":id,"key":key,"sha":sha,"delivery":delivery.chars().take(128).collect::<String>(),"kind":kind,"status":"pending","created":now(),"config":c["revision"]}),
    )?;
    Ok(id)
}
pub fn jobs(c: &Value) -> Result<Vec<PathBuf>> {
    let p = Path::new(c["state_root"].as_str().context("state_root")?).join("jobs");
    fs::create_dir_all(&p)?;
    ensure!(fs::symlink_metadata(&p)?.is_dir(), "unsafe jobs directory");
    let mut out = Vec::new();
    for entry in fs::read_dir(p)? {
        let p = entry?.path();
        if p.extension().is_some_and(|s| s == "json") {
            out.push(p)
        }
    }
    ensure!(
        out.len() as u64 <= super::git::limit(c, "scan_entries"),
        "scan_entries"
    );
    out.sort();
    Ok(out)
}
pub fn quarantine(c: &Value, p: &Path) -> Result<()> {
    let state = Path::new(c["state_root"].as_str().context("state_root")?);
    let folder = state
        .join("quarantine")
        .join(uuid::Uuid::new_v4().simple().to_string());
    fs::create_dir_all(&folder)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    fs::rename(p, folder.join("record"))?;
    atomic(
        &folder.join("reason.json"),
        &json!({"original_name":p.file_name().and_then(|s|s.to_str()),"quarantined_at":now(),"error":"InvalidData"}),
    )?;
    super::snapshot::sync_dir(p.parent().context("job parent")?)?;
    Ok(())
}
pub fn retry(c: &Value, id: &str) -> Result<()> {
    ensure!(hex_string(id, 64), "invalid job ID");
    let _lock = Lock::acquire(c, "worker", true)?;
    let p = Path::new(c["state_root"].as_str().context("state_root")?)
        .join("jobs")
        .join(format!("{id}.json"));
    let mut j = read_job(&p)?;
    ensure!(j["status"] == "failed", "only failed jobs can be retried");
    j["status"] = "pending".into();
    j.as_object_mut().context("job")?.remove("resource_limit");
    atomic(&p, &j)
}
#[cfg(target_os = "linux")]
pub struct Wakeup(i32);
#[cfg(target_os = "linux")]
impl Wakeup {
    pub fn new(path: &Path) -> Self {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Self(-1);
        }
        let p = match CString::new(path.as_os_str().as_bytes()) {
            Ok(v) => v,
            Err(_) => {
                unsafe { libc::close(fd) };
                return Self(-1);
            }
        };
        if unsafe { libc::inotify_add_watch(fd, p.as_ptr(), 0x188) } < 0 {
            unsafe { libc::close(fd) };
            Self(-1)
        } else {
            Self(fd)
        }
    }
    pub fn wait(&self) {
        if self.0 < 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            return;
        }
        let mut p = libc::pollfd {
            fd: self.0,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut p, 1, 2000);
            let mut buf = [0u8; 65536];
            for _ in 0..8 {
                if libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) <= 0 {
                    break;
                }
            }
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for Wakeup {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}
#[cfg(not(target_os = "linux"))]
pub struct Wakeup;
#[cfg(not(target_os = "linux"))]
impl Wakeup {
    pub fn new(_: &Path) -> Self {
        Self
    }
    pub fn wait(&self) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_compatible_job_id() {
        assert_eq!(
            identity("images", &"a".repeat(40), "auto"),
            hex::encode(Sha256::digest(format!("images:{}:auto", "a".repeat(40))))
        );
    }
    #[test]
    fn corrupted_queue_rejected() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(format!("{}.json", "a".repeat(64)));
        fs::write(&p, b"{}").unwrap();
        assert!(read_job(&p).is_err());
    }
}
