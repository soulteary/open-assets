use crate::common::{atomic, now, read_json};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
    time::Instant,
};
pub const CSP:&str="sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
pub fn relative(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('/')
            && !name.ends_with('/')
            && !name.contains("//")
            && !name.contains('\0'),
        "unsafe path"
    );
    ensure!(name.split('/').all(|s| !s.starts_with('.')), "unsafe path");
    for part in Path::new(name).components() {
        match part {
            Component::Normal(p) => ensure!(!p.to_string_lossy().starts_with('.'), "unsafe path"),
            _ => bail!("unsafe path"),
        }
    }
    ensure!(
        Path::new(name).to_string_lossy() == name,
        "noncanonical path"
    );
    Ok(())
}
pub fn sync_dir(p: &Path) -> Result<()> {
    ensure!(fs::symlink_metadata(p)?.is_dir(), "unsafe directory");
    File::open(p)?.sync_all()?;
    Ok(())
}
pub fn hash_file(p: &Path, max: u64) -> Result<String> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)?;
    ensure!(
        f.metadata()?.is_file() && f.metadata()?.len() <= max,
        "unsafe file"
    );
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    let mut n = 0u64;
    loop {
        let got = f.read(&mut buf)?;
        if got == 0 {
            break;
        }
        n += got as u64;
        ensure!(n <= max, "file size mismatch");
        h.update(&buf[..got]);
    }
    ensure!(n == max, "file size mismatch");
    Ok(hex::encode(h.finalize()))
}
pub fn fingerprint(p: &Path) -> Result<Value> {
    identity(&fs::symlink_metadata(p)?)
}
pub fn identity(m: &fs::Metadata) -> Result<Value> {
    ensure!(m.is_file(), "unsafe immutable file");
    Ok(json!([
        m.dev(),
        m.ino(),
        m.size(),
        m.mtime() as i128 * 1000000000 + m.mtime_nsec() as i128,
        m.ctime() as i128 * 1000000000 + m.ctime_nsec() as i128,
        m.mode() & 0o7777
    ]))
}
pub fn matches(p: &Path, item: &Value, proof: &Value, expired: bool) -> bool {
    let at = proof["verified_at"].as_f64().unwrap_or(f64::NEG_INFINITY);
    let age = now() - at;
    proof.is_object()
        && proof["sha256"] == item["sha256"]
        && proof["size"] == item["size"]
        && age >= 0.
        && (expired || age <= 3600.)
        && proof["fingerprint"][5]
            .as_u64()
            .map(|v| v & 0o222 == 0)
            .unwrap_or(false)
        && fingerprint(p)
            .map(|v| v == proof["fingerprint"])
            .unwrap_or(false)
}
pub fn proof(p: &Path, item: &Value, verified_at: f64) -> Result<Value> {
    // Caller has just fully hashed producer-owned bytes. Avoid an unnecessary
    // chmod on an already read-only inode: it would invalidate every alias.
    let file = open_readonly_file(p)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && Some(before.len()) == item["size"].as_u64(),
        "unsafe immutable file"
    );
    let changed = before.mode() & 0o222 != 0;
    if changed {
        file.set_permissions(fs::Permissions::from_mode(0o444))?;
    }
    let after = file.metadata()?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.nlink() == after.nlink()
            && after.mode() & 0o222 == 0
            && (!changed || after.mode() & 0o7777 == 0o444)
            && (changed || identity(&before)? == identity(&after)?),
        "immutable source changed while sealing"
    );
    file.sync_all()?;
    let fingerprint = identity(&after)?;
    ensure!(
        identity(&file.metadata()?)? == fingerprint
            && identity(&open_readonly_file(p)?.metadata()?)? == fingerprint,
        "immutable source changed while sealing"
    );
    Ok(
        json!({"size":item["size"],"sha256":item["sha256"],"fingerprint":fingerprint,"verified_at":verified_at}),
    )
}
pub fn safe_link(source: &Path, dest: &Path, item: &Value, old: &Value) -> Result<Value> {
    safe_link_required(source, dest, item, old, false)
}
pub fn safe_link_required(
    source: &Path,
    dest: &Path,
    item: &Value,
    old: &Value,
    require_link: bool,
) -> Result<Value> {
    ensure!(
        matches(source, item, old, false),
        "immutable source identity changed"
    );
    let before = fs::symlink_metadata(source)?;
    ensure!(
        identity(&before)? == old["fingerprint"],
        "immutable source identity changed"
    );
    if let Err(error) = fs::hard_link(source, dest) {
        if require_link
            || !error.raw_os_error().is_some_and(|code| {
                [
                    libc::EXDEV,
                    libc::EPERM,
                    libc::EACCES,
                    libc::ENOTSUP,
                    libc::EMLINK,
                ]
                .contains(&code)
            })
        {
            return Err(error.into());
        }
        let mut source_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)?;
        ensure!(source_file.metadata()?.is_file(), "unsafe immutable source");
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dest)?;
        let size = item["size"].as_u64().context("immutable size")?;
        let got = std::io::copy(&mut source_file.by_ref().take(size + 1), &mut target)?;
        ensure!(got == size, "immutable fallback size mismatch");
        target.sync_all()?;
        ensure!(
            hash_file(dest, size)? == item["sha256"].as_str().context("immutable SHA")?
                && matches(source, item, old, false),
            "immutable fallback differs"
        );
        return proof(dest, item, now());
    }
    let after = fs::symlink_metadata(source)?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.size() == after.size()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.mode() == after.mode()
            && before.nlink() + 1 == after.nlink(),
        "immutable source changed while linking"
    );
    let mut out = old.clone();
    out["fingerprint"] = fingerprint(source)?;
    Ok(out)
}
/// Inspect the complete logical tree without trusting any byte proof. Missing,
/// unlisted, linked, nonregular or noncanonical entries invalidate the context.
/// Size/byte eligibility is decided per file by the caller.
pub(crate) fn inspect_tree(root: &Path, files: &[Value]) -> Result<BTreeMap<String, fs::Metadata>> {
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "version root is not a directory"
    );
    let mut expected = BTreeMap::new();
    for item in files {
        let name = item["path"].as_str().context("invalid manifest path")?;
        relative(name)?;
        ensure!(
            item["size"].as_u64().is_some()
                && super::hex_string(item["sha256"].as_str().unwrap_or(""), 64)
                && expected.insert(name, item).is_none(),
            "invalid manifest entry"
        );
    }
    let mut actual = BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(dir) = dirs.pop() {
        ensure!(
            fs::symlink_metadata(&dir)?.is_dir(),
            "unsafe version directory"
        );
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            count += 1;
            ensure!(count <= 200000, "snapshot scan budget exceeded");
            let info = fs::symlink_metadata(&path)?;
            if info.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("invalid path encoding")?;
            ensure!(
                expected.contains_key(name) && info.is_file(),
                "unexpected version entry"
            );
            actual.insert(name.to_string(), info);
        }
    }
    ensure!(actual.len() == expected.len(), "missing version file");
    Ok(actual)
}
pub fn verify(
    root: &Path,
    files: &[Value],
    proofs: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let actual = inspect_tree(root, files)?;
    for item in files {
        let name = item["path"].as_str().context("manifest path")?;
        let info = &actual[name];
        ensure!(
            info.len() == item["size"].as_u64().context("manifest size")?,
            "version size mismatch"
        );
        let path = root.join(name);
        if let Some(proofs) = proofs {
            ensure!(
                matches(&path, item, proofs.get(name).unwrap_or(&Value::Null), false),
                "verified version identity changed"
            )
        } else {
            ensure!(
                hash_file(&path, info.len())? == item["sha256"].as_str().unwrap_or(""),
                "version bytes mismatch"
            )
        }
    }
    Ok(())
}
/// Open through descriptor-relative, no-follow directory components. A recovered
/// proof must never authenticate a file reached through a substituted parent link.
fn open_readonly_file(path: &Path) -> Result<File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    #[cfg(target_os = "macos")]
    let normalized = if let Ok(rest) = path.strip_prefix("/var") {
        Path::new("/private/var").join(rest)
    } else if let Ok(rest) = path.strip_prefix("/tmp") {
        Path::new("/private/tmp").join(rest)
    } else {
        path.to_path_buf()
    };
    #[cfg(target_os = "macos")]
    let path = normalized.as_path();
    let parent = path.parent().context("proof parent")?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(if parent.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        })?;
    for part in parent.components() {
        let part = match part {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(p) => p,
            _ => bail!("unsafe proof parent"),
        };
        let name = CString::new(part.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    let name = CString::new(path.file_name().context("proof filename")?.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
/// A fresh proof is created only after the entire expected body is hashed. This
/// does not chmod, copy, link, rewrite or otherwise change the serving inode.
pub(crate) fn revalidate_proof(
    path: &Path,
    item: &Value,
    record: &Value,
    deadline: Option<Instant>,
) -> Result<Value> {
    let size = item["size"].as_u64().context("proof size")?;
    let expected = item["sha256"].as_str().context("proof SHA256")?;
    ensure!(super::hex_string(expected, 64), "invalid proof digest");
    ensure!(source_record_valid(record), "invalid source receipt");
    let mut file = open_readonly_file(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() == size && before.mode() & 0o222 == 0,
        "unqualified immutable source"
    );
    let fingerprint = identity(&before)?;
    let mut hash = Sha256::new();
    let mut git_hash = sha1::Sha1::new();
    git_hash.update(format!("blob {size}\0").as_bytes());
    let mut etag_hash = md5::Context::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        ensure!(
            deadline.is_none_or(|at| Instant::now() < at),
            "proof verification deadline exceeded"
        );
        let maximum = (size.saturating_sub(bytes) + 1).min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..maximum])?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(bytes <= size, "proof size mismatch");
        hash.update(&buffer[..count]);
        git_hash.update(&buffer[..count]);
        etag_hash.consume(&buffer[..count]);
    }
    ensure!(
        hex::encode(git_hash.finalize()) == record["oid"].as_str().unwrap_or("")
            && format!("\"{:x}\"", etag_hash.finalize()) == record["etag"].as_str().unwrap_or(""),
        "proof Git/object receipt mismatch"
    );
    ensure!(
        bytes == size && hex::encode(hash.finalize()) == expected,
        "proof bytes mismatch"
    );
    file.sync_all()?;
    ensure!(
        identity(&file.metadata()?)? == fingerprint
            && identity(&open_readonly_file(path)?.metadata()?)? == fingerprint,
        "proof source changed during verification"
    );
    Ok(json!({"size":size,"sha256":expected,"fingerprint":fingerprint,"verified_at":now()}))
}
pub fn sync_tree(root: &Path) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    let mut count = 0;
    while let Some(d) = stack.pop() {
        ensure!(
            fs::symlink_metadata(&d)?.is_dir(),
            "unsafe snapshot directory"
        );
        for e in fs::read_dir(&d)? {
            count += 1;
            ensure!(count <= 200000, "snapshot sync entry budget exceeded");
            let p = e?.path();
            let m = fs::symlink_metadata(&p)?;
            if m.is_dir() {
                stack.push(p)
            } else {
                ensure!(m.is_file(), "unsafe snapshot entry");
                File::open(p)?.sync_all()?;
            }
        }
        dirs.push(d);
    }
    for d in dirs.into_iter().rev() {
        sync_dir(&d)?
    }
    Ok(())
}
pub fn switch(base: &Path, target: &str) -> Result<()> {
    ensure!(
        regex::Regex::new(r"^(?:images-[a-f0-9]{40}|[a-f0-9]{40}/images)$")?.is_match(target),
        "invalid activation target"
    );
    let tmp = base.join(format!(".current-{}", uuid::Uuid::new_v4().simple()));
    std::os::unix::fs::symlink(format!("releases/{target}"), &tmp)?;
    if let Err(e) = fs::rename(&tmp, base.join("current")) {
        let _ = fs::remove_file(tmp);
        return Err(e.into());
    }
    sync_dir(base)
}
pub fn image_target(manifest: &Value) -> Result<String> {
    let sha = manifest["sha"].as_str().context("manifest SHA")?;
    ensure!(super::valid_sha(sha), "invalid image version");
    Ok(if manifest["image_layout"] == "snapshot-v1" {
        format!("{sha}/images")
    } else {
        format!("images-{sha}")
    })
}
pub fn images(files: &[Value]) -> Result<Vec<Value>> {
    let out = files
        .iter()
        .filter_map(|i| {
            i["path"]
                .as_str()
                .and_then(|p| p.strip_prefix("images/"))
                .map(|p| {
                    let mut v = i.clone();
                    v["path"] = p.into();
                    v
                })
        })
        .collect::<Vec<_>>();
    ensure!(!out.is_empty(), "missing images");
    Ok(out)
}
pub fn activate(
    base: &Path,
    target: &str,
    files: &[Value],
    proofs: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    verify(&base.join("releases").join(target), files, proofs)?;
    let current = base.join("current");
    let previous = match fs::symlink_metadata(&current) {
        Ok(m) => {
            ensure!(m.file_type().is_symlink(), "current is not a symlink");
            Some(fs::read_link(&current)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let result = (|| -> Result<()> {
        switch(base, target)?;
        ensure!(
            fs::read_link(&current)? == Path::new(&format!("releases/{target}")),
            "activation target changed"
        );
        verify(&fs::canonicalize(&current)?, files, proofs)
    })();
    if result.is_err() {
        if let Some(previous) = previous {
            let tmp = base.join(format!(".restore-{}", uuid::Uuid::new_v4().simple()));
            std::os::unix::fs::symlink(previous, &tmp)?;
            fs::rename(tmp, &current)?;
        } else {
            let _ = fs::remove_file(&current);
        }
        sync_dir(base)?;
    }
    result
}
pub fn validate_svg(path: &Path) -> Result<()> {
    use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
    let raw = crate::common::read_regular(path, 2 << 20)?;
    let text = std::str::from_utf8(&raw)?.trim_start_matches('\u{feff}');
    ensure!(
        text.chars().all(|c| matches!(c, '\t' | '\n' | '\r')
            || (c as u32 >= 0x20 && c != '\u{fffe}' && c != '\u{ffff}')),
        "invalid XML character"
    );
    ensure!(
        !regex::Regex::new(r"(?i)<!\s*(?:DOCTYPE|ENTITY)\b")?.is_match(text),
        "SVG declarations unsupported"
    );
    let data_image = regex::Regex::new(r"^data:image/(?:png|jpeg|gif|webp|avif);base64,")?;
    let mut reader = NsReader::from_str(text);
    reader.config_mut().check_end_names = true;
    let (mut count, mut depth) = (0u64, 0u64);
    let mut root = false;
    loop {
        let (ns, event) = reader.read_resolved_event()?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                ensure!(depth > 0 || !root, "multiple SVG roots");
                count += 1;
                depth += 1;
                ensure!(
                    depth <= 128 && count <= 20000,
                    "SVG complexity limit exceeded"
                );
                ensure!(
                    matches!(ns,ResolveResult::Bound(n) if n.as_ref()==b"http://www.w3.org/2000/svg"),
                    "non-SVG elements unsupported"
                );
                let local =
                    String::from_utf8(e.local_name().as_ref().to_vec())?.to_ascii_lowercase();
                if !root {
                    ensure!(
                        e.local_name().as_ref() == b"svg",
                        "SVG root namespace required"
                    );
                    root = true;
                }
                ensure!(
                    ![
                        "script",
                        "foreignobject",
                        "iframe",
                        "object",
                        "embed",
                        "audio",
                        "video"
                    ]
                    .contains(&local.as_str()),
                    "active SVG element unsupported"
                );
                for a in e.attributes() {
                    let a = a?;
                    let name = String::from_utf8(a.key.local_name().as_ref().to_vec())?
                        .to_ascii_lowercase();
                    let value = a.decode_and_unescape_value(reader.decoder())?;
                    ensure!(!name.starts_with("on"), "SVG event handlers unsupported");
                    if name == "href" {
                        ensure!(
                            value.starts_with('#') || data_image.is_match(&value),
                            "external SVG reference unsupported"
                        );
                    }
                    if name == "attributename" {
                        ensure!(
                            !value.to_ascii_lowercase().starts_with("on"),
                            "SVG event animation unsupported"
                        );
                    }
                }
                if empty {
                    depth -= 1
                }
            }
            Event::End(_) => {
                ensure!(depth > 0, "invalid SVG XML");
                depth -= 1
            }
            Event::PI(_) | Event::DocType(_) => bail!("SVG processing instructions unsupported"),
            Event::Decl(_) => ensure!(!root, "misplaced XML declaration"),
            Event::GeneralRef(e) => {
                let n = e.decode()?;
                ensure!(
                    ["amp", "lt", "gt", "quot", "apos"].contains(&n.as_ref())
                        || n.strip_prefix("#x")
                            .is_some_and(|s| u32::from_str_radix(s, 16).is_ok())
                        || n.strip_prefix('#')
                            .is_some_and(|s| s.parse::<u32>().is_ok()),
                    "invalid XML entity"
                )
            }
            Event::Text(e) if depth == 0 => {
                ensure!(e.decode()?.trim().is_empty(), "text outside SVG root")
            }
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(root && depth == 0, "SVG root required");
    Ok(())
}
#[derive(Clone)]
pub struct Source {
    pub item: Value,
    pub path: PathBuf,
    pub oid: String,
    pub object: Value,
    pub etag: Value,
    pub modified: Value,
    pub proof: Value,
}
fn valid_etag(value: &str) -> bool {
    value.len() == 34
        && value.starts_with('"')
        && value.ends_with('"')
        && super::hex_string(&value[1..33], 32)
}
pub(crate) fn source_record_valid(record: &Value) -> bool {
    super::valid_sha(record["oid"].as_str().unwrap_or(""))
        && valid_etag(record["etag"].as_str().unwrap_or(""))
        && record["modified"].as_u64().is_some()
}
pub fn sources(c: &Value, r: &Value) -> Result<BTreeMap<String, Source>> {
    source_context(c, r, false)
}
pub(crate) fn sources_reverified(c: &Value, r: &Value) -> Result<BTreeMap<String, Source>> {
    source_context(c, r, true)
}
fn source_context(c: &Value, r: &Value, force_bytes: bool) -> Result<BTreeMap<String, Source>> {
    let mut result: BTreeMap<String, Source> = BTreeMap::new();
    let key = r["key"].as_str().context("repository key")?;
    let state = Path::new(c["state_root"].as_str().context("state_root")?);
    let manifest_path = state.join("published").join(key);
    let seal_path = state.join("incremental").join(format!("{key}.json"));
    let manifest = read_json(&manifest_path, 8 << 20)?;
    let original_seal = read_json(&seal_path, 8 << 20)?;
    let mut seal = original_seal.clone();
    let sha = manifest["sha"].as_str().context("manifest SHA")?;
    ensure!(
        super::valid_sha(sha)
            && manifest["key"] == key
            && seal["sha"] == sha
            && seal["version"] == 3
            && seal["config"] == c["revision"]
            && seal["records"].is_object(),
        "cache identity mismatch"
    );
    let digest = crate::common::digest(
        &json!({"files":manifest["files"],"object_sources":manifest.get("object_sources").cloned().unwrap_or(json!({}))}),
    )?;
    ensure!(seal["manifest"] == digest, "cache manifest mismatch");
    let objects = crate::storage::validate_manifest(&manifest)?;
    let base = Path::new(c["images_root"].as_str().context("images_root")?)
        .join("repos")
        .join(key);
    ensure!(
        fs::read_link(base.join("current"))?
            == Path::new(&format!("releases/{}", image_target(&manifest)?)),
        "cache current mismatch"
    );
    let full = base.join("releases").join(sha);
    let files = manifest["files"].as_array().context("manifest files")?;
    ensure!(
        files.len() <= r["max_files"].as_u64().unwrap_or(10000) as usize,
        "cache file budget"
    );
    let mut total = 0u64;
    for item in files {
        let size = item["size"].as_u64().context("cache size")?;
        ensure!(
            size <= r["max_file_bytes"].as_u64().unwrap_or(104857600),
            "cache file budget"
        );
        total = total.checked_add(size).context("cache byte overflow")?;
    }
    ensure!(
        total <= r["max_bytes"].as_u64().unwrap_or(1073741824),
        "cache byte budget"
    );
    let tree = inspect_tree(&full, files)?;
    let mut verified = BTreeMap::<(u64, u64, String, String), Value>::new();
    if !force_bytes {
        for item in files {
            let name = item["path"].as_str().context("manifest path")?;
            let record = &seal["records"][name];
            let proof = &record["immutable"];
            if source_record_valid(record) && matches(&full.join(name), item, proof, false) {
                let info = &tree[name];
                verified.insert(
                    (
                        info.dev(),
                        info.ino(),
                        record["oid"].as_str().unwrap().into(),
                        record["etag"].as_str().unwrap().into(),
                    ),
                    proof.clone(),
                );
            }
        }
    }
    let mut changed = false;
    let mut admitted = 0usize;
    for item in files {
        let name = item["path"].as_str().context("manifest path")?;
        let rec = seal["records"][name].clone();
        let oid = rec["oid"].as_str().unwrap_or("");
        if !source_record_valid(&rec) {
            continue;
        }
        let path = full.join(name);
        let info = &tree[name];
        if info.len() != item["size"].as_u64().context("manifest size")? || info.mode() & 0o222 != 0
        {
            continue;
        }
        let id = (
            info.dev(),
            info.ino(),
            oid.to_owned(),
            rec["etag"].as_str().unwrap().to_owned(),
        );
        let proof =
            if let Some(proof) = verified.get(&id).filter(|p| matches(&path, item, p, false)) {
                proof.clone()
            } else {
                match revalidate_proof(&path, item, &rec, None) {
                    Ok(proof) => {
                        verified.insert(id, proof.clone());
                        proof
                    }
                    Err(_) => continue,
                }
            };
        admitted += 1;
        let short = Value::Array(
            proof["fingerprint"]
                .as_array()
                .context("proof fingerprint")?[..5]
                .to_vec(),
        );
        if rec["immutable"] != proof || rec["fingerprint"] != short {
            let row = seal["records"]
                .get_mut(name)
                .context("cache record disappeared")?;
            row["immutable"] = proof.clone();
            row["fingerprint"] = Value::Array(
                proof["fingerprint"]
                    .as_array()
                    .context("proof fingerprint")?[..5]
                    .to_vec(),
            );
            changed = true;
        }
        let source = Source {
            item: item.clone(),
            path,
            oid: oid.into(),
            object: objects[name].clone(),
            etag: rec["etag"].clone(),
            modified: rec["modified"].clone(),
            proof,
        };
        if let Some(existing) = result.get(oid) {
            ensure!(
                existing.item["size"] == item["size"] && existing.item["sha256"] == item["sha256"],
                "conflicting Git source provenance"
            );
        } else {
            result.insert(oid.into(), source);
        }
    }
    ensure!(
        !force_bytes || admitted == files.len(),
        "incomplete source receipt revalidation"
    );
    if changed {
        ensure!(
            read_json(&manifest_path, 8 << 20)? == manifest
                && read_json(&seal_path, 8 << 20)? == original_seal
                && fs::read_link(base.join("current"))?
                    == Path::new(&format!("releases/{}", image_target(&manifest)?)),
            "cache baseline changed during verification"
        );
        atomic(&seal_path, &seal)?;
    }
    Ok(result)
}
#[expect(
    clippy::too_many_arguments,
    reason = "One atomic V3 seal binds the exact plan, Git OIDs, immutable proofs and object receipts to a configuration and commit."
)]
pub fn save(
    c: &Value,
    key: &str,
    sha: &str,
    files: &[Value],
    oids: &BTreeMap<String, String>,
    proofs: &BTreeMap<String, Value>,
    etags: &BTreeMap<String, Value>,
    sources: &Value,
) -> Result<()> {
    let full = Path::new(c["images_root"].as_str().context("images_root")?)
        .join("repos")
        .join(key)
        .join("releases")
        .join(sha);
    let mut records = serde_json::Map::new();
    for item in files {
        let name = item["path"].as_str().context("manifest path")?;
        let receipt = etags.get(name).unwrap_or(&Value::Null);
        let etag = receipt["etag"].as_str().unwrap_or("");
        if !valid_etag(etag) || receipt["modified"].as_u64().is_none() {
            continue;
        }
        let mut p = proofs.get(name).cloned().unwrap_or(Value::Null);
        ensure!(p.is_object(), "missing immutable proof");
        p["fingerprint"] = fingerprint(&full.join(name))?;
        let fp = p["fingerprint"].as_array().context("fingerprint")?[..5].to_vec();
        records.insert(name.into(),json!({"oid":oids[name],"etag":receipt["etag"],"modified":receipt["modified"],"fingerprint":fp,"immutable":p}));
    }
    let digest = json!({"files":files,"object_sources":sources});
    let digest = hex::encode(Sha256::digest(crate::common::python_json(&digest, true)?));
    atomic(
        &Path::new(c["state_root"].as_str().context("state_root")?)
            .join("incremental")
            .join(format!("{key}.json")),
        &json!({"version":3,"sha":sha,"config":c["revision"],"manifest":digest,"records":records}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_rejections() {
        for p in ["", "/a", "a/../b", "a//b", ".hidden/a", "a/./b", "a/"] {
            assert!(relative(p).is_err(), "{p}");
        }
        assert!(relative("images/2026/image.svg").is_ok());
    }
    #[test]
    fn rejects_symlink_tree() {
        let t = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/passwd", t.path().join("x")).unwrap();
        assert!(verify(
            t.path(),
            &[json!({"path":"x","size":1,"sha256":"0".repeat(64)})],
            None
        )
        .is_err());
    }
}

/// New bytes were already fsynced by export; only directory entries need syncing.
pub fn sync_directories(root: &Path, selected: Option<&BTreeSet<PathBuf>>) -> Result<()> {
    let mut paths = Vec::new();
    if let Some(selected) = selected {
        for rel in selected {
            ensure!(
                !rel.is_absolute()
                    && rel
                        .components()
                        .all(|p| matches!(p, Component::Normal(_) | Component::CurDir)),
                "unsafe sync directory"
            );
            paths.push(root.join(rel));
        }
    } else {
        let mut stack = vec![root.to_path_buf()];
        let mut count = 0;
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir)? {
                count += 1;
                ensure!(count <= 200000, "snapshot sync entry budget");
                let p = entry?.path();
                let m = fs::symlink_metadata(&p)?;
                if m.is_dir() {
                    stack.push(p)
                } else {
                    ensure!(m.is_file(), "unsafe snapshot entry")
                }
            }
            paths.push(dir)
        }
    }
    let mut levels: BTreeMap<usize, Vec<PathBuf>> = BTreeMap::new();
    for path in paths {
        ensure!(
            fs::symlink_metadata(&path)?.is_dir(),
            "unsafe sync directory"
        );
        levels
            .entry(path.strip_prefix(root)?.components().count())
            .or_default()
            .push(path);
    }
    for (_, paths) in levels.into_iter().rev() {
        for batch in paths.chunks(32) {
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|scope| -> Result<()> {
                let mut handles = Vec::new();
                for _ in 0..4 {
                    handles.push(scope.spawn(|| -> Result<()> {
                        loop {
                            let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(p) = batch.get(n) else { break };
                            sync_dir(p)?;
                        }
                        Ok(())
                    }));
                }
                let mut error = None;
                for handle in handles {
                    match handle.join() {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error.get_or_insert(e);
                        }
                        Err(_) => {
                            error.get_or_insert(anyhow::anyhow!("directory sync worker failed"));
                        }
                    }
                }
                if let Some(e) = error {
                    Err(e)
                } else {
                    Ok(())
                }
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod svg_regression {
    use super::*;
    fn check(raw: &[u8]) -> Result<()> {
        let t = tempfile::tempdir()?;
        let p = t.path().join("image.svg");
        fs::write(&p, raw)?;
        validate_svg(&p)
    }
    #[test]
    fn static_svg_matches_python_policy() {
        check(br##"<?xml version="1.0" encoding="UTF-8"?><svg xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="g"/></defs><style>rect{fill:url(#g)}</style><rect width="10" height="10"/><use href="#g"/></svg>"##).unwrap();
        let many = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>",
            "<g/>".repeat(1000)
        );
        check(many.as_bytes()).unwrap();
    }
    #[test]
    fn active_elements_and_event_attributes_refused() {
        for content in [
            "<script>alert(1)</script>",
            "<foreignObject/>",
            "<g onload=\"alert(1)\"/>",
            "<use href=\"https://example.org/x\"/>",
            "<image href=\"data:image/svg+xml;base64,PHN2Zy8+\"/>",
            "<set attributeName=\"onload\"/>",
            "<g xmlns=\"http://www.w3.org/1999/xhtml\"/>",
        ] {
            let text = format!("<svg xmlns=\"http://www.w3.org/2000/svg\">{content}</svg>");
            assert!(check(text.as_bytes()).is_err(), "{content}");
        }
    }
    #[test]
    fn malformed_declarations_and_entities_refused() {
        for text in ["<svg>","<html/>","<!DOCTYPE svg [<!ENTITY x \"text\">]><svg xmlns=\"http://www.w3.org/2000/svg\">&x;</svg>","<?xml-stylesheet href=\"x\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>","<svg xmlns=\"http://www.w3.org/2000/svg\">&unknown;</svg>","<svg xmlns=\"http://www.w3.org/2000/svg\"/><svg xmlns=\"http://www.w3.org/2000/svg\"/>","<SVG xmlns=\"http://www.w3.org/2000/svg\"/>"]{assert!(check(text.as_bytes()).is_err(),"{text}");}
    }
    #[test]
    fn budgets_are_enforced() {
        let deep = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}{}</svg>",
            "<g>".repeat(129),
            "</g>".repeat(129)
        );
        assert!(check(deep.as_bytes()).is_err());
        let many = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>",
            "<g/>".repeat(20001)
        );
        assert!(check(many.as_bytes()).is_err());
        assert!(check(&vec![b' '; (2 << 20) + 1]).is_err());
    }
}

#[cfg(test)]
mod proof_reuse_regression {
    use super::*;
    use crate::publisher::proof_reuse_fixtures::Fixture;
    #[test]
    fn mixed_fresh_and_expired_sources_rehash_only_when_needed_without_copying() {
        let f = Fixture::new(3, false);
        f.age(&[1], 3700.);
        let before = f.seal();
        let data = f
            .files
            .iter()
            .map(|i| {
                let path = f.full.join(i["path"].as_str().unwrap());
                (
                    fingerprint(&path).unwrap(),
                    fs::metadata(path).unwrap().nlink(),
                )
            })
            .collect::<Vec<_>>();
        let started = now();
        let sources = sources(&f.c, f.repository()).unwrap();
        assert_eq!(sources.len(), 3);
        let after = f.seal();
        assert_eq!(
            after["records"]["images/0000.png"]["immutable"],
            before["records"]["images/0000.png"]["immutable"]
        );
        assert!(
            after["records"]["images/0001.png"]["immutable"]["verified_at"]
                .as_f64()
                .unwrap()
                >= started
        );
        for (i, item) in f.files.iter().enumerate() {
            let path = f.full.join(item["path"].as_str().unwrap());
            assert_eq!(fingerprint(&path).unwrap(), data[i].0);
            assert_eq!(fs::metadata(&path).unwrap().nlink(), data[i].1);
            let source = &sources[&f.oids[item["path"].as_str().unwrap()]];
            assert_eq!(
                source.object,
                f.manifest["object_sources"][item["path"].as_str().unwrap()]
            );
        }
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
    fn one_bad_hash_does_not_admit_bad_source_or_discard_unrelated_sources() {
        let f = Fixture::new(3, false);
        f.corrupt(1);
        let sources = sources(&f.c, f.repository()).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(!sources.contains_key(&f.oids["images/0001.png"]));
        assert!(sources.contains_key(&f.oids["images/0000.png"]));
        assert!(sources.contains_key(&f.oids["images/0002.png"]));
        assert_ne!(
            hash_file(
                &f.full.join("images/0001.png"),
                f.files[1]["size"].as_u64().unwrap()
            )
            .unwrap(),
            f.files[1]["sha256"].as_str().unwrap()
        );
    }
    #[test]
    fn expired_receipts_must_match_git_oid_and_object_md5_for_identical_size() {
        for field in ["oid", "etag"] {
            let f = Fixture::new(3, false);
            f.age(&[1], 3700.);
            let mut seal = f.seal();
            seal["records"]["images/0001.png"][field] = if field == "oid" {
                json!("b".repeat(40))
            } else {
                json!(format!("\"{}\"", "0".repeat(32)))
            };
            atomic(&f.seal_path, &seal).unwrap();
            let path = f.full.join("images/0001.png");
            let before = fingerprint(&path).unwrap();
            let admitted = sources(&f.c, f.repository()).unwrap();
            assert_eq!(admitted.len(), 2);
            assert_eq!(
                f.seal()["records"]["images/0001.png"],
                seal["records"]["images/0001.png"]
            );
            assert_eq!(fingerprint(&path).unwrap(), before);
            assert!(sources_reverified(&f.c, f.repository()).is_err());
        }
    }
    #[test]
    fn expired_wrong_alias_receipt_cannot_borrow_fresh_inode_proof() {
        for field in ["oid", "etag"] {
            let f = Fixture::new(2, true);
            f.age(&[1], 3700.);
            let mut seal = f.seal();
            seal["records"]["images/0001.png"][field] = if field == "oid" {
                json!("b".repeat(40))
            } else {
                json!(format!("\"{}\"", "0".repeat(32)))
            };
            atomic(&f.seal_path, &seal).unwrap();
            let admitted = sources(&f.c, f.repository()).unwrap();
            assert_eq!(admitted.len(), 1);
            assert!(admitted.contains_key(&f.oids["images/0000.png"]));
            assert_eq!(
                f.seal()["records"]["images/0001.png"],
                seal["records"]["images/0001.png"]
            );
        }
    }
    #[test]
    fn own_ctime_change_requires_full_bytes_but_does_not_recopy_data() {
        let f = Fixture::new(2, false);
        let file = f.full.join("images/0000.png");
        let before = f.seal();
        let at = now();
        let extra = f._temporary.path().join("interrupted-link");
        fs::hard_link(&file, &extra).unwrap();
        assert!(!matches(
            &file,
            &f.files[0],
            &before["records"]["images/0000.png"]["immutable"],
            false
        ));
        let id = fingerprint(&file).unwrap();
        let sources = sources(&f.c, f.repository()).unwrap();
        assert_eq!(sources.len(), 2);
        let seal = f.seal();
        assert!(
            seal["records"]["images/0000.png"]["immutable"]["verified_at"]
                .as_f64()
                .unwrap()
                >= at
        );
        assert_eq!(fingerprint(&file).unwrap(), id);
        assert!(extra.exists());
    }
    #[test]
    fn repeated_readonly_sealing_does_not_change_ctime_or_break_alias_proof() {
        let f = Fixture::new(2, true);
        let p = f.full.join("images/0000.png");
        let before = fingerprint(&p).unwrap();
        let first = proof(&p, &f.files[0], now()).unwrap();
        let second = proof(&f.full.join("images/0001.png"), &f.files[1], now()).unwrap();
        assert_eq!(fingerprint(&p).unwrap(), before);
        assert!(matches(&p, &f.files[0], &first, false));
        assert!(matches(&p, &f.files[0], &second, false));
    }
    #[test]
    fn writable_or_escaped_source_is_never_requalified() {
        let f = Fixture::new(2, false);
        let p = f.full.join("images/0001.png");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let admitted = sources(&f.c, f.repository()).unwrap();
        assert_eq!(admitted.len(), 1);
        assert!(revalidate_proof(
            &p,
            &f.files[1],
            &f.seal()["records"]["images/0001.png"],
            None
        )
        .is_err());
        let alias = f._temporary.path().join("linked-parent");
        std::os::unix::fs::symlink(&f.full, &alias).unwrap();
        assert!(revalidate_proof(
            &alias.join("images/0000.png"),
            &f.files[0],
            &f.seal()["records"]["images/0000.png"],
            None
        )
        .is_err());
    }
}
