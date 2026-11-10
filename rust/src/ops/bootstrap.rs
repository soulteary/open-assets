use super::*;
use flate2::read::GzDecoder;
use rand::RngCore;
use std::io::Cursor;
const ROLES: [&str; 7] = ["oss", "img", "storage", "git", "auth", "traefik", "info"];
fn dns_label(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}
pub fn build_domains(node: &str, base: &str) -> Result<Value> {
    ensure!(
        dns_label(node, 48)
            && base.split('.').count() >= 2
            && base.split('.').all(|s| dns_label(s, 63)),
        "invalid deployment DNS names"
    );
    let mut result = json!({});
    for role in ROLES {
        let domain = format!(
            "{role}-{node}.{}{base}",
            if ["storage", "git", "auth", "traefik", "info"].contains(&role) {
                "console."
            } else {
                ""
            }
        );
        ensure!(domain.len() <= 253, "generated hostname exceeds DNS limit");
        result[role] = json!(domain);
    }
    Ok(result)
}
pub fn render_env(template: &str, node: &str, base: &str) -> Result<String> {
    let domains = build_domains(node, base)?;
    let values = BTreeMap::from([
        ("NODE_NAME", node),
        ("PUBLIC_BASE", base),
        (
            "STORAGE_DOMAIN",
            domains["storage"].as_str().context("storage")?,
        ),
        ("GIT_DOMAIN", domains["git"].as_str().context("git")?),
    ]);
    let mut found = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for line in template.lines() {
        if let Some((k, _)) = line.split_once('=') {
            if let Some(v) = values.get(k) {
                found.insert(k);
                out.push(format!("{k}={v}"));
                continue;
            }
        }
        out.push(line.into());
    }
    ensure!(found.len() == values.len(), "missing naming fields");
    Ok(out.join("\n") + "\n")
}
pub fn check_naming(env_path: &Path, domains_path: &Path) -> Result<()> {
    let text = fs::read_to_string(env_path)?;
    let env: BTreeMap<&str, &str> = text
        .lines()
        .filter(|s| !s.starts_with('#'))
        .filter_map(|s| s.split_once('='))
        .collect();
    let domains = build_domains(
        env.get("NODE_NAME").context("node name")?,
        env.get("PUBLIC_BASE").context("public base")?,
    )?;
    ensure!(
        info::read_json_bounded(domains_path, 16384)? == domains,
        "naming metadata differs from runtime domains"
    );
    for (role, key) in [("storage", "STORAGE_DOMAIN"), ("git", "GIT_DOMAIN")] {
        ensure!(
            env.get(key).copied() == domains[role].as_str(),
            "Compose domain differs from runtime domain"
        );
    }
    Ok(())
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
pub fn unused(app: &Path, data: &Path) -> Result<()> {
    ensure!(
        !exists(&app.join(".env")) && !exists(&app.join("private")),
        "existing private configuration; use maintenance commands"
    );
    if exists(data) {
        ensure!(
            fs::symlink_metadata(data)?.is_dir() && fs::read_dir(data)?.next().is_none(),
            "existing application data; unused deployment required"
        );
    }
    Ok(())
}
fn write_new(path: &Path, value: &Value) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    f.write_all(&common::python_json(value, false)?)?;
    f.sync_all()?;
    common::sync_directory(path.parent().context("marker parent")?)
}
pub fn prepare(app: &Path, data: &Path) -> Result<PathBuf> {
    unused(app, data)?;
    let private = app.join("private");
    fs::create_dir(&private)?;
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
    let marker = private.join(".preparation.json");
    write_new(
        &marker,
        &json!({"schema_version":1,"status":"running","started_at":common::now()}),
    )?;
    Ok(marker)
}
pub fn initialize(private: &Path) -> Result<PathBuf> {
    let marker = private.join(".initialization.json");
    ensure!(
        !exists(&marker) && !exists(&private.join("oc-cli")),
        "initialization already started; inspect state before recovery"
    );
    let value = info::read_json_bounded(&private.join(".preparation.json"), 4096)?;
    ensure!(
        value["schema_version"] == 1 && value["status"] == "complete",
        "fresh completed preparation required"
    );
    write_new(
        &marker,
        &json!({"schema_version":1,"status":"running","started_at":common::now()}),
    )?;
    Ok(marker)
}
pub fn complete(marker: &Path) -> Result<()> {
    ensure!(
        info::read_json_bounded(marker, 4096)?["status"] == "running",
        "preparation marker is not running"
    );
    let tmp = marker.with_extension("complete.tmp");
    write_new(
        &tmp,
        &json!({"schema_version":1,"status":"complete","completed_at":common::now()}),
    )?;
    fs::rename(tmp, marker)?;
    common::sync_directory(marker.parent().context("marker parent")?)
}
pub fn guard_command(args: &[String]) -> Result<()> {
    let op = args.first().context("guard operation")?;
    let app = root_arg(args);
    let data = flag(args, "--data")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/asset-stack"));
    match op.as_str() {
        "unused" => unused(&app, &data),
        "prepare" => {
            println!("{}", prepare(&app, &data)?.display());
            Ok(())
        }
        "initialize" => {
            println!("{}", initialize(&app.join("private"))?.display());
            Ok(())
        }
        "complete" => complete(Path::new(
            flag(args, "--marker").context("marker required")?,
        )),
        _ => bail!("unknown guard operation"),
    }
}
fn archive_path_safe(path: &Path) -> bool {
    !path.is_absolute()
        && !path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
}
fn resolved_inside(destination: &Path, relative: &Path) -> Result<()> {
    let destination = destination.canonicalize()?;
    let mut value = destination.clone();
    for c in relative.components() {
        match c {
            Component::Normal(name) => {
                value.push(name);
                if fs::symlink_metadata(&value).is_ok() {
                    value = value.canonicalize()?;
                }
            }
            Component::ParentDir => {
                ensure!(value.pop(), "archive link escapes destination");
            }
            Component::CurDir => {}
            _ => bail!("absolute archive link"),
        }
    }
    ensure!(
        value.starts_with(&destination),
        "archive link escapes destination"
    );
    Ok(())
}
pub fn verify_backup(archive: &Path, dest: &Path) -> Result<()> {
    let checksum = fs::read_to_string(PathBuf::from(format!("{}.sha256", archive.display())))?
        .split_whitespace()
        .next()
        .context("checksum")?
        .to_owned();
    ensure!(
        checksum.len() == 64
            && checksum
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid snapshot checksum"
    );
    let mut source = fs::File::open(archive)?;
    let mut digest = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    ensure!(
        hex::encode(digest.finalize()) == checksum,
        "snapshot checksum mismatch"
    );
    fs::create_dir(dest)?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o700))?;
    let mut tar = tar::Archive::new(GzDecoder::new(fs::File::open(archive)?));
    tar.set_preserve_permissions(false);
    tar.set_preserve_ownerships(false);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        ensure!(archive_path_safe(&path), "unsafe archive path");
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir() || kind.is_symlink() || kind.is_hard_link(),
            "unsupported archive entry"
        );
        if let Some(target) = entry.link_name()? {
            ensure!(!target.is_absolute(), "absolute archive link");
            let mut level = if kind.is_symlink() {
                path.parent().map(|p| p.components().count()).unwrap_or(0)
            } else {
                0
            };
            for c in target.components() {
                match c {
                    Component::ParentDir => {
                        ensure!(level > 0, "archive link escapes destination");
                        level -= 1;
                    }
                    Component::Normal(_) => level += 1,
                    Component::CurDir => {}
                    _ => bail!("archive link escapes destination"),
                }
            }
        }
        ensure!(entry.unpack_in(dest)?, "archive extraction refused");
    }
    for entry in walkdir::WalkDir::new(dest).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            let target = fs::read_link(entry.path())?;
            let parent = entry
                .path()
                .parent()
                .context("archive symlink parent")?
                .strip_prefix(dest)?;
            resolved_inside(dest, &parent.join(target))?;
        }
    }
    Ok(())
}
pub fn configure_edge(args: &[String]) -> Result<()> {
    let domains = info::read_json_bounded(
        Path::new(flag(args, "--domains").context("domains required")?),
        16384,
    )?;
    let ca = fs::read_to_string(flag(args, "--ca").context("CA required")?)?;
    let output = Path::new(flag(args, "--output").context("output required")?);
    ensure!(
        matches!(
            output.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        ),
        "output must end in yaml or yml"
    );
    for role in ROLES {
        ensure!(
            domains[role].as_str().is_some_and(super::hostname),
            "invalid configured hostname"
        );
    }
    let public_policy = if let Some(path) = flag(args, "--public-access-config") {
        crate::public_access::Policy::from_config(
            &json!({"public_access":common::read_json(Path::new(path),65_536)?}),
        )?
    } else {
        crate::public_access::Policy::default()
    };
    let mut services = json!({});
    for (name, url) in [
        ("images", "http://nginx:8080"),
        ("git", "http://gitea:3000"),
        ("auth", "http://172.29.19.3:8080"),
        ("storage", "https://oc:9090"),
        ("objects", "https://otterio:9000"),
        ("info", "http://info:8082"),
        ("navigation", "http://nginx:8081"),
    ] {
        services[name] = json!({"loadBalancer":{"servers":[{"url":url}]}});
    }
    let _references = args.iter().any(|a| a == "--reference-releases");
    // Both legacy and reference manifests are resolved by the guarded Reader;
    // Nginx rejects forbidden methods and referers before reaching it.
    services["release-objects"] = json!({"loadBalancer":{"servers":[{"url":"http://nginx:8084"}]}});
    services["public-redirect"] = json!({"loadBalancer":{"servers":[{"url":"http://nginx:8087"}]}});
    services["storage"]["loadBalancer"]["serversTransport"] = json!("console-tls");
    services["objects"]["loadBalancer"]["serversTransport"] = json!("storage-tls");
    let mut routers = json!({});
    for (key, service) in [
        ("img", "images"),
        ("oss", "objects"),
        ("git", "git"),
        ("auth", "auth"),
        ("storage", "storage"),
        ("traefik", "api@internal"),
        ("info", "info"),
    ] {
        let mut rule = format!("Host(`{}`)", domains[key].as_str().context("domain")?);
        if key == "oss" {
            rule.push_str(" && PathPrefix(`/attachments/repositories/`)");
        }
        let mut r = json!({"rule":rule,"entryPoints":["websecure"],"service":if key=="oss"{"release-objects"}else{service},"tls":{"certResolver":"le"}});
        if matches!(key, "img" | "oss") {
            r["middlewares"] = json!(["public-assets", "public-qps"]);
        }
        if ["git", "storage", "traefik", "info"].contains(&key) {
            r["middlewares"] = json!(["console-auth", "console-gzip"]);
        }
        routers[key] = r;
    }
    routers["public-http"] = json!({"rule":format!("Host(`{}`) || Host(`{}`)",domains["img"].as_str().unwrap(),domains["oss"].as_str().unwrap()),"entryPoints":["web"],"service":"public-redirect","priority":100,"middlewares":["public-http-headers","public-qps"]});
    let management = ["git", "storage", "traefik", "info"]
        .map(|k| format!("Host(`{}`)", domains[k].as_str().unwrap_or("")))
        .join(" || ");
    for (key, path, service, priority, auth) in [
        (
            "session-exchange",
            "/_session_exchange",
            "auth",
            1000,
            false,
        ),
        ("auth-return", "/__auth_return", "navigation", 900, true),
    ] {
        let mut r = json!({"rule":format!("({management}) && Path(`{path}`)"),"entryPoints":["websecure"],"service":service,"priority":priority,"tls":{"certResolver":"le"}});
        if auth {
            r["middlewares"] = json!(["console-auth", "console-gzip"]);
        }
        routers[key] = r;
    }
    let mut config = json!({"http":{"routers":routers,"services":services,"middlewares":{"public-assets":{"headers":{"customResponseHeaders":{"Content-Security-Policy":crate::storage::CSP,"X-Content-Type-Options":"nosniff"}}},"console-gzip":{"compress":{"encodings":["gzip"],"minResponseBodyBytes":1024,"includedContentTypes":["text/html","text/plain","text/css","text/javascript","application/javascript","application/json","application/xml","text/xml","image/svg+xml"]}},"console-auth":{"forwardAuth":{"address":"http://172.29.19.3:8080/_auth","trustForwardHeader":false,"maxResponseBodySize":1048576}}},"serversTransports":{"console-tls":{"serverName":domains["storage"],"rootCAs":[ca]},"storage-tls":{"serverName":"storage.internal","rootCAs":[ca]}}}});
    config["http"]["middlewares"]["public-assets"]["headers"]["customResponseHeaders"]
        ["Strict-Transport-Security"] = json!("");
    config["http"]["middlewares"]["public-assets"]["headers"]["customResponseHeaders"]
        ["Set-Cookie"] = json!("");
    config["http"]["middlewares"]["public-assets"]["headers"]["stsSeconds"] = json!(0);
    config["http"]["middlewares"]["public-http-headers"] = json!({"headers":{"customResponseHeaders":{"Strict-Transport-Security":"","Set-Cookie":""},"customRequestHeaders":{"Authorization":"","Cookie":""}}});
    // Default source criterion is the authenticated socket peer, not client XFF.
    config["http"]["middlewares"]["public-qps"] =
        json!({"rateLimit":{"average":public_policy.qps,"period":"1s","burst":public_policy.qps}});
    let old = fs::symlink_metadata(output).ok();
    if let Some(m) = &old {
        ensure!(m.is_file(), "route output must be a regular file");
        common::read_regular(output, 4 * 1024 * 1024)?;
    }
    private_write(output, &serde_json::to_vec_pretty(&config)?, None)?;
    if let Some(m) = old {
        use std::os::unix::fs::MetadataExt;
        chown(output, m.uid(), m.gid())?;
        fs::set_permissions(output, m.permissions())?;
    } else {
        fs::set_permissions(output, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}
pub fn ensure_certs_maker(
    app: &Path,
    env: &BTreeMap<String, String>,
    archive: Option<&Path>,
) -> Result<PathBuf> {
    ensure!(cfg!(target_os = "linux"), "deployment supports Linux only");
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => bail!("unsupported Certs Maker platform"),
    };
    let version = env
        .get("CERTS_MAKER_VERSION")
        .context("certificate tool version")?;
    ensure!(
        version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
        "invalid certificate version"
    );
    let expected = env
        .get(&format!(
            "CERTS_MAKER_SHA256_{}",
            architecture.to_uppercase()
        ))
        .context("certificate tool checksum")?;
    let tools = app.join("private/tools");
    fs::create_dir_all(&tools)?;
    fs::set_permissions(&tools, fs::Permissions::from_mode(0o700))?;
    let binary = tools.join("certs-maker");
    let receipt = tools.join("certs-maker.json");
    if binary.exists() && receipt.exists() {
        let r = info::read_json_bounded(&receipt, 16384)?;
        if r["version"] == *version && r["archive_sha256"] == *expected {
            let bytes = common::read_regular(&binary, 64 * 1024 * 1024)?;
            ensure!(
                r["binary_sha256"] == hex::encode(Sha256::digest(&bytes)),
                "installed certificate tool digest mismatch"
            );
            return Ok(binary);
        }
    }
    let bytes = if let Some(path) = archive {
        common::read_regular(path, 32 * 1024 * 1024)?
    } else {
        let url=format!("https://github.com/soulteary/certs-maker/releases/download/v{version}/certs-maker_{version}_linux_{architecture}.tar.gz");
        let mut response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?
            .get(url)
            .send()?
            .error_for_status()?;
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        bytes
    };
    ensure!(
        bytes.len() <= 32 * 1024 * 1024 && hex::encode(Sha256::digest(&bytes)) == *expected,
        "official certificate tool archive checksum mismatch"
    );
    let mut tar = tar::Archive::new(GzDecoder::new(Cursor::new(bytes)));
    let mut data = None;
    for e in tar.entries()? {
        let e = e?;
        if e.path()?.as_ref() == Path::new("certs-maker") && e.header().entry_type().is_file() {
            ensure!(
                e.size() <= 64 * 1024 * 1024,
                "certificate tool binary too large"
            );
            let mut buf = Vec::new();
            e.take(64 * 1024 * 1024 + 1).read_to_end(&mut buf)?;
            ensure!(buf.len() <= 64 * 1024 * 1024, "binary budget");
            data = Some(buf);
            break;
        }
    }
    let data = data.context("expected regular certificate tool binary missing")?;
    private_write(&binary, &data, None)?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
    private_write(
        &receipt,
        &serde_json::to_vec(
            &json!({"version":version,"archive_sha256":expected,"binary_sha256":hex::encode(Sha256::digest(&data))}),
        )?,
        None,
    )?;
    Ok(binary)
}
pub fn generate_certificates(output: &Path, domains: &Value, binary: &Path) -> Result<()> {
    ensure!(
        !output.exists() || fs::read_dir(output)?.next().is_none(),
        "output must be absent or empty; never generate over live certificates"
    );
    fs::create_dir_all(output)?;
    fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    let mut roots = Vec::new();
    for (role, names, uid) in [
        ("storage", "storage.internal", 20002u32),
        (
            "console",
            domains["storage"].as_str().context("storage domain")?,
            20004,
        ),
    ] {
        ensure!(
            super::hostname(names) || names == "storage.internal",
            "invalid certificate identity"
        );
        let work = output.join("issuers").join(role);
        fs::create_dir_all(&work)?;
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        command(
            binary.to_str().context("tool encoding")?,
            &[
                &format!("--CERT_DNS={names}"),
                "--FOR_FIREFOX=ON",
                "--EXPIRE_DAYS=3650",
                "--CUSTOM_FILE_NAME=server",
                "--CERT_O=Open Assets",
                "--DIR=ssl",
            ],
            Some(&work),
        )?;
        let generated = work.join("ssl");
        let ca = generated.join("server.rootCA.pem");
        let cert = generated.join("server.pem.crt");
        let key = generated.join("server.pem.key");
        for hostname in names.split(',') {
            command(
                "openssl",
                &[
                    "verify",
                    "-CAfile",
                    ca.to_str().context("CA path")?,
                    "-verify_hostname",
                    hostname,
                    cert.to_str().context("certificate path")?,
                ],
                None,
            )?;
        }
        let public = command(
            "openssl",
            &[
                "x509",
                "-in",
                cert.to_str().context("cert")?,
                "-noout",
                "-pubkey",
            ],
            None,
        )?;
        let private = command(
            "openssl",
            &["pkey", "-in", key.to_str().context("key")?, "-pubout"],
            None,
        )?;
        ensure!(public == private, "certificate/key mismatch");
        for path in [&ca, &cert] {
            let dates = command(
                "openssl",
                &[
                    "x509",
                    "-in",
                    path.to_str().context("certificate path")?,
                    "-noout",
                    "-dates",
                ],
                None,
            )?;
            let dates: BTreeMap<&str, &str> =
                dates.lines().filter_map(|l| l.split_once('=')).collect();
            let parse = |s: &str| -> Result<i64> {
                Ok(
                    chrono::NaiveDateTime::parse_from_str(s, "%b %e %H:%M:%S %Y GMT")?
                        .and_utc()
                        .timestamp(),
                )
            };
            let start = parse(dates.get("notBefore").context("certificate start")?)?;
            let end = parse(dates.get("notAfter").context("certificate end")?)?;
            ensure!(
                end - start == 3650 * 86400 && end as f64 > common::now() + 3649.0 * 86400.0,
                "unexpected certificate validity"
            );
        }
        let dest = output.join(role);
        fs::create_dir(&dest)?;
        chown(&dest, uid, uid)?;
        fs::set_permissions(
            &dest,
            fs::Permissions::from_mode(if role == "storage" { 0o750 } else { 0o700 }),
        )?;
        let (certname, keyname) = if role == "storage" {
            ("public.crt", "private.key")
        } else {
            ("console.crt", "console.key")
        };
        for (src, name) in [(&cert, certname), (&key, keyname)] {
            fs::copy(src, dest.join(name))?;
            chown(&dest.join(name), uid, uid)?;
            fs::set_permissions(
                dest.join(name),
                fs::Permissions::from_mode(if role == "storage" { 0o640 } else { 0o600 }),
            )?;
        }
        fs::copy(&ca, work.join("root.crt"))?;
        fs::copy(generated.join("server.rootCA.key"), work.join("root.key"))?;
        for e in walkdir::WalkDir::new(&work).follow_links(false) {
            let e = e?;
            ensure!(!e.file_type().is_symlink(), "linked generated certificate");
            fs::set_permissions(
                e.path(),
                fs::Permissions::from_mode(if e.file_type().is_dir() { 0o700 } else { 0o600 }),
            )?;
        }
        roots.push(fs::read(&ca)?);
    }
    let bundle = roots.join(&b'\n');
    private_write(&output.join("ca.crt"), &bundle, None)?;
    for (role, uid) in [("storage", 20002), ("console", 20004)] {
        let path = output.join(role).join("ca.crt");
        private_write(&path, &bundle, Some(uid))?;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if role == "storage" { 0o640 } else { 0o600 }),
        )?;
    }
    let cas = output.join("storage/CAs");
    fs::create_dir(&cas)?;
    chown(&cas, 20002, 20002)?;
    fs::set_permissions(cas, fs::Permissions::from_mode(0o750))?;
    fs::set_permissions(output, fs::Permissions::from_mode(0o711))?;
    Ok(())
}
pub fn internal_certs(args: &[String]) -> Result<()> {
    host_only()?;
    let root = protected_root(&root_arg(args))?;
    let env = info::environment(&root)?;
    let domains = info::read_json_bounded(
        Path::new(flag(args, "--domains").context("domains required")?),
        16384,
    )?;
    let binary = ensure_certs_maker(&root, &env, flag(args, "--archive").map(Path::new))?;
    generate_certificates(
        Path::new(flag(args, "--output").context("output required")?),
        &domains,
        &binary,
    )?;
    println!("Two verified server certificates and issuing CAs generated (3650 days)");
    Ok(())
}
fn secret(private: &Path, name: &str, value: Option<&str>) -> Result<String> {
    let path = private.join(name);
    if !exists(&path) {
        let value = value.map(str::to_owned).unwrap_or_else(|| {
            let mut bytes = [0; 24];
            rand::thread_rng().fill_bytes(&mut bytes);
            hex::encode(bytes)
        });
        private_write(&path, value.as_bytes(), None)?;
    }
    Ok(String::from_utf8(common::read_regular(&path, 16384)?)?
        .trim()
        .into())
}
pub fn prepare_private(args: &[String]) -> Result<()> {
    host_only()?;
    unsafe {
        libc::umask(0o077);
    };
    let app = protected_root(&root_arg(args))?;
    let data = flag(args, "--data")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/asset-stack"));
    unused(&app, &data)?;
    let base = std::env::var("PUBLIC_BASE").context("PUBLIC_BASE required")?;
    let node = std::env::var("NODE_NAME").unwrap_or_else(|_| "node-name".into());
    let domains = build_domains(&node, &base)?;
    let marker = prepare(&app, &data)?;
    let private = app.join("private");
    for path in ["storage", "publisher", "tls", "generated"] {
        fs::create_dir_all(private.join(path))?;
    }
    for (path, uid) in [
        ("publisher", 20010),
        ("publisher/manifests", 20010),
        ("publisher/object-retention", 20010),
        ("images", 20010),
        ("info-refresh", 20010),
        ("otterio", 20002),
        ("gitea/data", 20001),
        ("gitea/config", 20001),
        ("console", 20004),
        ("archives", 20004),
    ] {
        let p = data.join(path);
        fs::create_dir_all(&p)?;
        chown(&p, uid, uid)?;
        fs::set_permissions(p, fs::Permissions::from_mode(0o750))?;
    }
    let guard = data.join("publisher/object-retention/compaction.lock");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&guard)?;
    chown(&guard, 20010, 20010)?;
    fs::set_permissions(guard, fs::Permissions::from_mode(0o644))?;
    for (name, value) in [
        ("storage/root_user", Some("assets-root")),
        ("storage/root_password", None),
        ("publisher/publisher_access", Some("assets-publisher")),
        ("publisher/publisher_secret", None),
        ("webhook_secret", None),
        ("admin_password", None),
        ("stargate_password", None),
        ("console_user", Some("assets-operator")),
        ("console_password", None),
    ] {
        secret(&private, name, value)?;
    }
    fs::copy(
        private.join("webhook_secret"),
        private.join("publisher/webhook_secret"),
    )?;
    command(
        "ssh-keygen",
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            private
                .join("publisher/deploy_key")
                .to_str()
                .context("deploy key")?,
        ],
        None,
    )?;
    let template = fs::read_to_string(app.join(".env.example"))?;
    let settings: BTreeMap<String, String> = template
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.into(), v.into())))
        .collect();
    let binary = ensure_certs_maker(&app, &settings, flag(args, "--archive").map(Path::new))?;
    let tls = private.join("tls");
    generate_certificates(&tls, &domains, &binary)?;
    fs::copy(tls.join("ca.crt"), private.join("publisher/ca.crt"))?;
    private_write(
        &app.join(".env"),
        render_env(&template, &node, &base)?.as_bytes(),
        None,
    )?;
    let password = secret(&private, "stargate_password", None)?;
    let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST)?;
    let session = secret(&private, "session_secret", None)?;
    let stargate=format!("AUTH_HOST={}\nPORT=8080\nPASSWORDS='bcrypt:{hash}'\nCOOKIE_SECURE=true\nSESSION_EXCHANGE_REDIRECT_PATH=/__auth_return\nCALLBACK_ALLOWED_HOSTS={}\nSESSION_EXCHANGE_SECRET={session}\nTRUSTED_PROXIES=172.29.19.2\nWARDEN_ENABLED=false\nHERALD_ENABLED=false\nSESSION_STORAGE_ENABLED=false\n",domains["auth"].as_str().context("auth domain")?,["storage","traefik","git","info"].map(|r|domains[r].as_str().unwrap_or("")).join(","));
    private_write(&private.join("stargate.env"), stargate.as_bytes(), None)?;
    let mut c: Value =
        serde_yaml::from_slice(&fs::read(app.join("config/repositories.example.yaml"))?)?;
    c["endpoint"] = json!("https://storage.internal:9000");
    private_write(
        &private.join("repositories.yaml"),
        serde_yaml::to_string(&c)?.as_bytes(),
        Some(20010),
    )?;
    private_write(
        &private.join("domains.json"),
        &serde_json::to_vec(&domains)?,
        None,
    )?;
    fs::set_permissions(&private, fs::Permissions::from_mode(0o711))?;
    for p in [private.join("publisher"), private.join("generated")] {
        fs::set_permissions(&p, fs::Permissions::from_mode(0o750))?;
        chown(&p, 20010, 20010)?;
    }
    for e in fs::read_dir(private.join("publisher"))? {
        chown(&e?.path(), 20010, 20010)?;
    }
    for p in [
        private.join("webhook_secret"),
        private.join("repositories.yaml"),
    ] {
        chown(&p, 20010, 20010)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640))?;
    }
    let storage = private.join("storage");
    chown(&storage, 20002, 20002)?;
    fs::set_permissions(&storage, fs::Permissions::from_mode(0o750))?;
    for e in fs::read_dir(storage)? {
        let p = e?.path();
        chown(&p, 20002, 20002)?;
        fs::set_permissions(p, fs::Permissions::from_mode(0o640))?;
    }
    complete(&marker)?;
    println!(
        "Private credentials, certificates and configuration prepared; existing Traefik unchanged"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_contract() {
        let d = build_domains("toh1", "soulteary.com").unwrap();
        assert_eq!(d["img"], "img-toh1.soulteary.com");
        assert_eq!(d["git"], "git-toh1.console.soulteary.com");
        for (node, base) in [
            ("UPPER", "example.com"),
            ("ok", "https://example.com"),
            ("-bad", "example.com"),
        ] {
            assert!(build_domains(node, base).is_err());
        }
    }
    #[test]
    fn public_edge_policy_is_scoped_and_uses_safe_peer_rate_limit() {
        let d = tempfile::tempdir().unwrap();
        let domains = d.path().join("domains.json");
        let ca = d.path().join("ca.crt");
        let policy = d.path().join("public-access.json");
        let output = d.path().join("routes.yaml");
        fs::write(
            &domains,
            serde_json::to_vec(&build_domains("demo", "example.com").unwrap()).unwrap(),
        )
        .unwrap();
        fs::write(&ca, "fixture CA path").unwrap();
        fs::write(
            &policy,
            br#"{"qps":7,"hotlink_enabled":true,"allowed_referers":["example.com"]}"#,
        )
        .unwrap();
        let args = vec![
            "--domains".into(),
            domains.to_str().unwrap().into(),
            "--ca".into(),
            ca.to_str().unwrap().into(),
            "--output".into(),
            output.to_str().unwrap().into(),
            "--public-access-config".into(),
            policy.to_str().unwrap().into(),
        ];
        configure_edge(&args).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        let http = &v["http"];
        assert_eq!(
            http["routers"]["public-http"]["entryPoints"],
            json!(["web"])
        );
        assert_eq!(http["routers"]["public-http"]["service"], "public-redirect");
        assert_eq!(
            http["services"]["public-redirect"]["loadBalancer"]["servers"][0]["url"],
            "http://nginx:8087"
        );
        assert_eq!(
            http["services"]["release-objects"]["loadBalancer"]["servers"][0]["url"],
            "http://nginx:8084"
        );
        assert_eq!(
            http["middlewares"]["public-qps"]["rateLimit"],
            json!({"average":7,"period":"1s","burst":7})
        );
        for role in ["img", "oss"] {
            assert_eq!(
                http["routers"][role]["middlewares"],
                json!(["public-assets", "public-qps"])
            );
        }
        assert_eq!(
            http["middlewares"]["public-assets"]["headers"]["customResponseHeaders"]
                ["Strict-Transport-Security"],
            ""
        );
        assert_eq!(
            http["routers"]["info"]["middlewares"],
            json!(["console-auth", "console-gzip"])
        );
        fs::write(&policy, br#"{"qps":0}"#).unwrap();
        let before = fs::read(&output).unwrap();
        assert!(configure_edge(&args).is_err());
        assert_eq!(fs::read(&output).unwrap(), before);
    }
    #[test]
    fn failed_initialization_never_reinitializes() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("app");
        let data = d.path().join("data");
        fs::create_dir(&app).unwrap();
        let p = prepare(&app, &data).unwrap();
        assert!(prepare(&app, &data).is_err());
        assert!(initialize(&app.join("private")).is_err());
        complete(&p).unwrap();
        let marker = initialize(&app.join("private")).unwrap();
        assert!(initialize(&app.join("private")).is_err());
        assert_eq!(
            info::read_json_bounded(&marker, 4096).unwrap()["status"],
            "running"
        );
    }
    #[test]
    fn archive_paths() {
        assert!(archive_path_safe(Path::new("app/private/config")));
        assert!(!archive_path_safe(Path::new("../../etc")));
        assert!(!archive_path_safe(Path::new("/etc")));
    }
}
#[cfg(test)]
mod migration_tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    fn archive(root: &Path, malicious: bool) -> PathBuf {
        let p = root.join("snapshot.tar.gz");
        let gz = GzEncoder::new(fs::File::create(&p).unwrap(), Compression::default());
        let mut b = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o600);
        h.set_cksum();
        b.append_data(&mut h, "app/data.txt", &b"safe"[..]).unwrap();
        if malicious {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_mode(0o777);
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_cksum();
            b.append_link(&mut h, "app/escape", "../../outside")
                .unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        let bytes = fs::read(&p).unwrap();
        fs::write(
            PathBuf::from(format!("{}.sha256", p.display())),
            format!("{} snapshot.tar.gz\n", hex::encode(Sha256::digest(bytes))),
        )
        .unwrap();
        p
    }
    #[test]
    fn backup_digest_and_exclusive_destination() {
        let root = tempfile::tempdir().unwrap();
        let archive = archive(root.path(), false);
        let dest = root.path().join("drill");
        verify_backup(&archive, &dest).unwrap();
        assert_eq!(fs::read(dest.join("app/data.txt")).unwrap(), b"safe");
        assert!(verify_backup(&archive, &dest).is_err());
        let invalid = root.path().join("invalid");
        fs::write(
            PathBuf::from(format!("{}.sha256", archive.display())),
            "0".repeat(64),
        )
        .unwrap();
        assert!(verify_backup(&archive, &invalid).is_err());
        assert!(!invalid.exists());
    }
    #[test]
    fn backup_cannot_link_outside_drill() {
        let root = tempfile::tempdir().unwrap();
        let archive = archive(root.path(), true);
        let dest = root.path().join("drill");
        assert!(verify_backup(&archive, &dest).is_err());
        assert!(!root.path().join("outside").exists());
    }
}
