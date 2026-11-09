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
    fs::set_permissions(p, fs::Permissions::from_mode(0o444))?;
    File::open(p)?.sync_all()?;
    Ok(
        json!({"size":item["size"],"sha256":item["sha256"],"fingerprint":fingerprint(p)?,"verified_at":verified_at}),
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
pub fn verify(
    root: &Path,
    files: &[Value],
    proofs: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
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
    let mut actual = BTreeSet::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(dir) = dirs.pop() {
        for e in fs::read_dir(dir)? {
            let p = e?.path();
            count += 1;
            ensure!(count <= 200000, "snapshot scan budget exceeded");
            let m = fs::symlink_metadata(&p)?;
            if m.is_dir() {
                dirs.push(p);
                continue;
            }
            let name = p
                .strip_prefix(root)?
                .to_str()
                .context("invalid path encoding")?;
            let item = expected.get(name).context("unexpected version entry")?;
            ensure!(
                m.is_file() && m.len() == item["size"].as_u64().context("manifest size")?,
                "version size mismatch"
            );
            if let Some(proofs) = proofs {
                ensure!(
                    matches(&p, item, proofs.get(name).unwrap_or(&Value::Null), false),
                    "verified version identity changed"
                )
            } else {
                ensure!(
                    hash_file(&p, m.len())? == item["sha256"].as_str().unwrap_or(""),
                    "version bytes mismatch"
                )
            };
            actual.insert(name.to_string());
        }
    }
    ensure!(actual.len() == expected.len(), "missing version file");
    Ok(())
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
pub fn sources(c: &Value, r: &Value) -> Result<BTreeMap<String, Source>> {
    let mut result = BTreeMap::new();
    let key = r["key"].as_str().context("repository key")?;
    let state = Path::new(c["state_root"].as_str().context("state_root")?);
    let manifest = read_json(&state.join("published").join(key), 8 << 20)?;
    let seal = read_json(
        &state.join("incremental").join(format!("{key}.json")),
        8 << 20,
    )?;
    let sha = manifest["sha"].as_str().context("manifest SHA")?;
    ensure!(
        super::valid_sha(sha)
            && manifest["key"] == key
            && seal["sha"] == sha
            && seal["version"] == 3
            && seal["config"] == c["revision"],
        "cache identity mismatch"
    );
    let digest = json!({"files":manifest["files"],"object_sources":manifest.get("object_sources").cloned().unwrap_or(json!({}))});
    let digest = hex::encode(Sha256::digest(crate::common::python_json(&digest, true)?));
    ensure!(seal["manifest"] == digest, "cache manifest mismatch");
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
    let mut proofs = BTreeMap::new();
    for item in files {
        let name = item["path"].as_str().context("manifest path")?;
        relative(name)?;
        let rec = &seal["records"][name];
        proofs.insert(name.to_string(), rec["immutable"].clone());
    }
    verify(&full, files, Some(&proofs))?;
    for item in files {
        let name = item["path"].as_str().context("manifest path")?;
        let rec = &seal["records"][name];
        let oid = rec["oid"].as_str().unwrap_or("");
        let etag = rec["etag"].as_str().unwrap_or("");
        if !super::valid_sha(oid) || !valid_etag(etag) || rec["modified"].as_u64().is_none() {
            continue;
        }
        let object=manifest["object_sources"].get(name).cloned().unwrap_or(json!({"key":format!("repositories/{key}/releases/{sha}/{name}"),"content_type":item["content_type"]}));
        let objectkey = object["key"].as_str().context("object key")?;
        ensure!(
            objectkey.starts_with(&format!("repositories/{key}/releases/")),
            "cross-repository object reference"
        );
        result.entry(oid.to_string()).or_insert(Source {
            item: item.clone(),
            path: full.join(name),
            oid: oid.into(),
            object,
            etag: rec["etag"].clone(),
            modified: rec["modified"].clone(),
            proof: rec["immutable"].clone(),
        });
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
