use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn limit(c: &Value, key: &str) -> u64 {
    c["resource_limits"][key].as_u64().unwrap_or(match key {
        "git_repository_bytes" => 2 << 30,
        "git_total_bytes" => 8 << 30,
        "git_fetch_bytes" => 512 << 20,
        "git_min_free_bytes" => 512 << 20,
        "git_timeout_seconds" => 120,
        "git_process_memory_bytes" => 512 << 20,
        "git_output_bytes" => 8 << 20,
        "scan_entries" => 200000,
        "queue_active_records" => 1000,
        "queue_total_records" => 10000,
        "queue_reserved_bytes" => 1 << 30,
        _ => 0,
    })
}
pub fn usage(root: &Path, budget: u64) -> Result<u64> {
    if !root.try_exists()? {
        return Ok(0);
    }
    let mut stack = vec![root.to_path_buf()];
    let (mut total, mut count) = (0u64, 0u64);
    while let Some(dir) = stack.pop() {
        ensure!(
            fs::symlink_metadata(&dir)?.file_type().is_dir(),
            "unsafe_resource_entry"
        );
        for item in fs::read_dir(dir)? {
            let p = item?.path();
            count += 1;
            ensure!(count <= budget, "scan_entries");
            let m = match fs::symlink_metadata(&p) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if m.is_dir() {
                stack.push(p)
            } else if m.is_file() {
                total = total.checked_add(m.len()).context("resource overflow")?
            } else {
                bail!("unsafe_resource_entry")
            }
        }
    }
    Ok(total)
}
pub fn free_bytes(path: &Path) -> Result<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut out = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), out.as_mut_ptr()) } == 0,
        "disk capacity unavailable"
    );
    let s = unsafe { out.assume_init() };
    #[cfg(target_os = "linux")]
    let blocks = s.f_bavail;
    #[cfg(not(target_os = "linux"))]
    let blocks = u64::from(s.f_bavail);
    blocks
        .checked_mul(s.f_frsize)
        .context("disk capacity overflow")
}
fn budget(c: &Value, path: &Path, initial: Option<u64>) -> Result<()> {
    let count = limit(c, "scan_entries");
    let used = usage(path, count)?;
    ensure!(
        used <= limit(c, "git_repository_bytes"),
        "git_repository_bytes"
    );
    if let Some(n) = initial {
        ensure!(
            used.saturating_sub(n) <= limit(c, "git_fetch_bytes"),
            "git_fetch_bytes"
        )
    }
    let root = Path::new(c["state_root"].as_str().context("state_root")?).join("git");
    ensure!(
        usage(&root, count)? <= limit(c, "git_total_bytes"),
        "git_total_bytes"
    );
    ensure!(
        free_bytes(path)? >= limit(c, "git_min_free_bytes"),
        "git_min_free_bytes"
    );
    Ok(())
}
pub fn run(
    c: &Value,
    path: &Path,
    args: &[String],
    input: Option<&[u8]>,
    fetch_initial: Option<u64>,
) -> Result<Vec<u8>> {
    let mut stdout = tempfile::tempfile()?;
    let mut stdin = tempfile::tempfile()?;
    if let Some(raw) = input {
        ensure!(raw.len() <= 65536, "invalid Git control input");
        stdin.write_all(raw)?;
        stdin.seek(SeekFrom::Start(0))?;
    }
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(path)
        .args(args)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::null());
    let _memory = limit(c, "git_process_memory_bytes");
    let bytes = limit(c, "git_fetch_bytes");
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            };
            #[cfg(target_os = "linux")]
            {
                let r = libc::rlimit {
                    rlim_cur: _memory,
                    rlim_max: _memory,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &r) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let r = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &r) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("Git process unavailable")?;
    let start = Instant::now();
    let mut last = Instant::now();
    let outcome = (|| -> Result<()> {
        loop {
            ensure!(
                start.elapsed() < Duration::from_secs(limit(c, "git_timeout_seconds")),
                "git_timeout_seconds"
            );
            ensure!(
                stdout.metadata()?.len() <= limit(c, "git_output_bytes"),
                "git_output_bytes"
            );
            if fetch_initial.is_some() && last.elapsed() >= Duration::from_millis(100) {
                budget(c, path, fetch_initial)?;
                last = Instant::now()
            }
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "Git command failed");
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    })();
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    outcome?;
    stdout.seek(SeekFrom::Start(0))?;
    let mut raw = Vec::new();
    stdout
        .take(limit(c, "git_output_bytes") + 1)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() as u64 <= limit(c, "git_output_bytes"),
        "git_output_bytes"
    );
    Ok(raw)
}
pub fn git(c: &Value, path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run(
        c,
        path,
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        None,
        None,
    )
}
pub fn fetch(c: &Value, r: &Value, path: &Path) -> Result<String> {
    let state = Path::new(c["state_root"].as_str().context("state_root")?);
    fs::create_dir_all(state)?;
    ensure!(
        fs::symlink_metadata(state)?.is_dir(),
        "unsafe_resource_entry"
    );
    let root = state.join("git");
    ensure!(
        path.parent() == Some(root.as_path()),
        "invalid Git cache location"
    );
    fs::create_dir_all(&root)?;
    ensure!(
        fs::symlink_metadata(&root)?.is_dir(),
        "unsafe_resource_entry"
    );
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(m.is_dir(), "unsafe_resource_entry"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            git(c, path, &["init", "--bare", "-q"])?;
        }
        Err(e) => return Err(e.into()),
    }
    budget(c, path, None)?;
    let initial = usage(path, limit(c, "scan_entries"))?;
    let temporary = format!("refs/open-assets/fetch-{}", uuid::Uuid::new_v4().simple());
    let reference = format!(
        "+refs/heads/{}:{}",
        r["branch"].as_str().context("branch")?,
        temporary
    );
    let args = [
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
        "-c",
        "pack.threads=1",
        "-c",
        "core.bigFileThreshold=1m",
        "-c",
        "pack.windowMemory=16m",
        "-c",
        "pack.deltaCacheSize=16m",
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--no-tags",
        "--no-write-fetch-head",
        "--no-recurse-submodules",
        r["remote"].as_str().context("remote")?,
        &reference,
    ];
    let outcome = (|| -> Result<String> {
        run(
            c,
            path,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            None,
            Some(initial),
        )?;
        let raw = git(c, path, &["rev-parse", &temporary])?;
        let sha = std::str::from_utf8(&raw)?.trim().to_string();
        ensure!(super::valid_sha(&sha), "invalid fetched commit");
        budget(c, path, Some(initial))?;
        run(
            c,
            path,
            &["update-ref".into(), "--stdin".into()],
            Some(format!("update refs/heads/source {sha}\ndelete {temporary} {sha}\n").as_bytes()),
            None,
        )?;
        Ok(sha)
    })();
    if outcome.is_err() {
        let _ = git(c, path, &["update-ref", "-d", &temporary]);
    }
    outcome
}
pub fn remote_head(c: &Value, r: &Value, path: &Path) -> Result<String> {
    let reference = format!("refs/heads/{}", r["branch"].as_str().context("branch")?);
    let raw = git(
        c,
        path,
        &[
            "ls-remote",
            "--exit-code",
            "--refs",
            r["remote"].as_str().context("remote")?,
            &reference,
        ],
    )?;
    let s = std::str::from_utf8(&raw)?;
    let p = s.trim_end().split('\t').collect::<Vec<_>>();
    ensure!(
        p.len() == 2 && p[1] == reference && super::valid_sha(p[0]),
        "invalid remote advertisement"
    );
    Ok(p[0].into())
}
#[derive(Clone, Debug)]
pub struct Blob {
    pub oid: String,
    pub relative: String,
    pub original: String,
    pub size: u64,
}
pub fn normalized_source(value: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in Path::new(value).components() {
        match part {
            std::path::Component::Normal(p) => parts.push(p.to_str().context("source encoding")?),
            std::path::Component::CurDir => {}
            _ => bail!("invalid source directory"),
        }
    }
    ensure!(!parts.is_empty(), "invalid source directory");
    Ok(parts.join("/"))
}
pub fn plan(c: &Value, r: &Value, path: &Path, sha: &str) -> Result<Vec<Blob>> {
    plan_cached(c, r, path, sha, &std::collections::BTreeMap::new())
}
pub fn plan_cached(
    c: &Value,
    r: &Value,
    path: &Path,
    sha: &str,
    known: &std::collections::BTreeMap<String, u64>,
) -> Result<Vec<Blob>> {
    ensure!(super::valid_sha(sha), "invalid commit");
    let source = normalized_source(r["source_directory"].as_str().context("source_directory")?)?;
    let source = source.as_str();
    let raw = if known.is_empty() {
        git(c, path, &["ls-tree", "-rlz", sha, "--", source])?
    } else {
        let raw = git(c, path, &["ls-tree", "-rz", sha, "--", source])?;
        let entries = raw
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .collect::<Vec<_>>();
        ensure!(
            entries.len() <= r["max_files"].as_u64().unwrap_or(10000) as usize,
            "publication limit exceeded"
        );
        let mut sizes = known.clone();
        let mut unknown = std::collections::BTreeSet::new();
        for entry in &entries {
            let at = entry
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid tree metadata")?;
            let fields = std::str::from_utf8(&entry[..at])?
                .split_whitespace()
                .collect::<Vec<_>>();
            ensure!(
                fields.len() == 3
                    && ["100644", "100755"].contains(&fields[0])
                    && fields[1] == "blob"
                    && super::valid_sha(fields[2]),
                "unsupported Git entry"
            );
            if !sizes.contains_key(fields[2]) {
                unknown.insert(fields[2].to_string());
            }
        }
        let unknown = unknown.into_iter().collect::<Vec<_>>();
        for batch in unknown.chunks(1024) {
            let input = (batch.join("\n") + "\n").into_bytes();
            let response = run(
                c,
                path,
                &["cat-file".into(), "--batch-check".into()],
                Some(&input),
                None,
            )?;
            let lines = std::str::from_utf8(&response)?.lines().collect::<Vec<_>>();
            ensure!(lines.len() == batch.len(), "Git size batch mismatch");
            for (oid, line) in batch.iter().zip(lines) {
                let p = line.split_whitespace().collect::<Vec<_>>();
                ensure!(
                    p.len() == 3 && p[0] == oid && p[1] == "blob",
                    "Git size identity mismatch"
                );
                sizes.insert(oid.clone(), p[2].parse::<u64>()?);
            }
        }
        let mut rebuilt = Vec::new();
        for entry in entries {
            let at = entry
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid tree metadata")?;
            let fields = std::str::from_utf8(&entry[..at])?
                .split_whitespace()
                .collect::<Vec<_>>();
            rebuilt.extend_from_slice(&entry[..at]);
            rebuilt.extend_from_slice(format!(" {}", sizes[fields[2]]).as_bytes());
            rebuilt.push(b'\t');
            rebuilt.extend_from_slice(&entry[at + 1..]);
            rebuilt.push(0);
        }
        rebuilt
    };
    let (mut out, mut total) = (Vec::new(), 0u64);
    for entry in raw.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let at = entry
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid tree metadata")?;
        let fields = std::str::from_utf8(&entry[..at])?
            .split_whitespace()
            .collect::<Vec<_>>();
        ensure!(
            fields.len() == 4
                && ["100644", "100755"].contains(&fields[0])
                && fields[1] == "blob"
                && super::valid_sha(fields[2]),
            "unsupported Git entry"
        );
        let name = std::str::from_utf8(&entry[at + 1..])?;
        let relative = name
            .strip_prefix(&format!("{source}/"))
            .context("invalid source path")?;
        super::snapshot::relative(relative)?;
        let size = fields[3].parse::<u64>()?;
        ensure!(
            size <= r["max_file_bytes"].as_u64().unwrap_or(104857600),
            "file limit exceeded"
        );
        total = total.checked_add(size).context("publication overflow")?;
        ensure!(
            total <= r["max_bytes"].as_u64().unwrap_or(1073741824)
                && out.len() < (r["max_files"].as_u64().unwrap_or(10000) as usize),
            "publication limit exceeded"
        );
        let ext = Path::new(relative)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "svg" {
            ensure!(size <= 2 << 20, "SVG size limit exceeded")
        }
        if relative.starts_with("images/") {
            ensure!(
                ["png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "svg"].contains(&ext.as_str()),
                "image format not allowed"
            );
            if r["mount_path"] == "/" {
                let name = relative.split('/').nth(1).unwrap_or("");
                for other in c["repositories"].as_array().context("repositories")? {
                    let mount = other["mount_path"]
                        .as_str()
                        .unwrap_or("/")
                        .trim_matches('/');
                    ensure!(mount.is_empty() || name != mount, "reserved URL prefix")
                }
            }
        }
        out.push(Blob {
            oid: fields[2].into(),
            relative: relative.into(),
            original: name.into(),
            size,
        });
    }
    ensure!(!out.is_empty(), "empty publication");
    ensure!(
        out.iter().any(|b| b.relative.starts_with("images/")),
        "missing images"
    );
    Ok(out)
}
pub fn blob(c: &Value, g: &Path, oid: &str, out: &Path, size: u64) -> Result<()> {
    ensure!(super::valid_sha(oid), "invalid blob");
    let target = File::options().write(true).create_new(true).open(out)?;
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(g)
        .args(["cat-file", "blob", oid])
        .stdin(Stdio::null())
        .stdout(Stdio::from(target.try_clone()?))
        .stderr(Stdio::null());
    let _memory = limit(c, "git_process_memory_bytes");
    let bytes = limit(c, "git_fetch_bytes");
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            #[cfg(target_os = "linux")]
            {
                let r = libc::rlimit {
                    rlim_cur: _memory,
                    rlim_max: _memory,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &r) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let r = libc::rlimit {
                rlim_cur: bytes,
                rlim_max: bytes,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &r) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    let result = (|| -> Result<()> {
        loop {
            ensure!(
                start.elapsed() < Duration::from_secs(limit(c, "git_timeout_seconds")),
                "git_timeout_seconds"
            );
            ensure!(target.metadata()?.len() <= size, "Git blob size mismatch");
            if let Some(s) = child.try_wait()? {
                ensure!(s.success(), "Git blob read failed");
                break;
            }
            thread::sleep(Duration::from_millis(2))
        }
        ensure!(target.metadata()?.len() == size, "Git blob size mismatch");
        target.sync_all()?;
        Ok(())
    })();
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_refuses_symlinks() {
        let t = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/tmp", t.path().join("bad")).unwrap();
        assert!(usage(t.path(), 100).is_err());
    }
    #[test]
    fn bounded_output() {
        let t = tempfile::tempdir().unwrap();
        let c = serde_json::json!({"resource_limits":{"git_output_bytes":2}});
        assert!(git(&c, t.path(), &["--version"]).is_err());
    }
}

pub struct Batch {
    child: std::process::Child,
    input: Option<std::process::ChildStdin>,
    output: std::process::ChildStdout,
    started: Instant,
    timeout: Duration,
}
impl Batch {
    pub fn new(c: &Value, g: &Path) -> Result<Self> {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(g)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let _memory = limit(c, "git_process_memory_bytes");
        let bytes = limit(c, "git_fetch_bytes");
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    let r = libc::rlimit {
                        rlim_cur: _memory,
                        rlim_max: _memory,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &r) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                let r = libc::rlimit {
                    rlim_cur: bytes,
                    rlim_max: bytes,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &r) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let input = child.stdin.take().context("Git stdin")?;
        let output = child.stdout.take().context("Git stdout")?;
        Ok(Self {
            child,
            input: Some(input),
            output,
            started: Instant::now(),
            timeout: Duration::from_secs(limit(c, "git_timeout_seconds")),
        })
    }
    fn read(&mut self, out: &mut [u8]) -> Result<usize> {
        use std::os::fd::AsRawFd;
        loop {
            let remaining = self
                .timeout
                .checked_sub(self.started.elapsed())
                .context("git_timeout_seconds")?;
            let mut descriptor = libc::pollfd {
                fd: self.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let n =
                unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis().min(1000) as i32) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if n == 0 {
                continue;
            }
            return Ok(self.output.read(out)?);
        }
    }
    pub fn finish(&mut self) -> Result<()> {
        drop(self.input.take());
        loop {
            ensure!(self.started.elapsed() < self.timeout, "git_timeout_seconds");
            if let Some(status) = self.child.try_wait()? {
                ensure!(status.success(), "Git batch process failed");
                return Ok(());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    pub fn write(&mut self, oid: &str, size: u64, target: &Path) -> Result<String> {
        use sha2::{Digest, Sha256};
        ensure!(super::valid_sha(oid), "invalid Git object");
        let input = self.input.as_mut().context("closed Git batch")?;
        input.write_all(format!("{oid}\n").as_bytes())?;
        input.flush()?;
        let mut header = Vec::new();
        loop {
            let mut one = [0];
            ensure!(self.read(&mut one)? == 1, "short Git header");
            if one[0] == b'\n' {
                break;
            }
            header.push(one[0]);
            ensure!(header.len() <= 128, "invalid Git header")
        }
        ensure!(
            header == format!("{oid} blob {size}").as_bytes(),
            "Git batch identity mismatch"
        );
        let mut f = File::options().write(true).create_new(true).open(target)?;
        let mut remaining = size;
        let mut hash = Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut prefix = Vec::new();
        while remaining > 0 {
            let max = remaining.min(buffer.len() as u64) as usize;
            let got = self.read(&mut buffer[..max])?;
            ensure!(got > 0, "short Git blob");
            const LFS: &[u8] = b"version https://git-lfs.github.com/spec/v1";
            if prefix.len() < LFS.len() {
                let n = (LFS.len() - prefix.len()).min(got);
                prefix.extend_from_slice(&buffer[..n]);
                ensure!(prefix.as_slice() != LFS, "LFS unsupported");
            }
            hash.update(&buffer[..got]);
            f.write_all(&buffer[..got])?;
            remaining -= got as u64;
        }
        let mut end = [0];
        ensure!(
            self.read(&mut end)? == 1 && end[0] == b'\n',
            "Git batch delimiter mismatch"
        );
        f.sync_all()?;
        Ok(hex::encode(hash.finalize()))
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    #[test]
    fn stalled_git_process_group_is_terminated() {
        let t = tempfile::tempdir().unwrap();
        let c = serde_json::json!({"resource_limits":{"git_timeout_seconds":1}});
        let start = Instant::now();
        let error = git(&c, t.path(), &["-c", "alias.pause=!sleep 10", "pause"]).unwrap_err();
        assert!(error.to_string().contains("git_timeout_seconds"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}

#[cfg(test)]
mod source_and_cache_regression {
    use super::*;
    #[test]
    fn legacy_source_directory_spellings_normalize_without_escaping() {
        for (raw, expected) in [
            ("public/", "public"),
            ("./public", "public"),
            ("public/./images", "public/images"),
            ("public//images/", "public/images"),
        ] {
            assert_eq!(normalized_source(raw).unwrap(), expected);
        }
        for raw in ["", ".", "/public", "public/../other"] {
            assert!(normalized_source(raw).is_err());
        }
    }
    #[test]
    fn dangling_cache_symlink_is_refused_before_git_init() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        fs::create_dir_all(state.join("git")).unwrap();
        let external = t.path().join("external");
        fs::create_dir(&external).unwrap();
        let outside = external.join("must-not-exist");
        let cache = state.join("git/images");
        std::os::unix::fs::symlink(&outside, &cache).unwrap();
        let c = serde_json::json!({"state_root":state,"resource_limits":{"git_min_free_bytes":1}});
        let r = serde_json::json!({"remote":"unused","branch":"main"});
        assert!(fetch(&c, &r, &cache)
            .unwrap_err()
            .to_string()
            .contains("unsafe_resource_entry"));
        assert!(!outside.exists());
        assert!(fs::read_dir(&external).unwrap().next().is_none());
    }
}
