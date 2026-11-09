//! Destructive fixtures for an explicitly empty, disposable Linux Docker host.
//! No public DNS, production credentials, or Python interpreter are used.
use anyhow::{ensure, Context, Result};
use base64::Engine;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
const ROOT: &str = "/app/open-assets";
#[derive(Debug)]
struct FixtureExit {
    i32_code: i32,
    body: Vec<u8>,
}
impl std::fmt::Display for FixtureExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fixture command failed ({})", self.i32_code)
    }
}
impl std::error::Error for FixtureExit {}
fn command(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut output = tempfile::tempfile()?;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(ROOT)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::null());
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = child.spawn()?;
    let started = Instant::now();
    let result = (|| -> Result<Vec<u8>> {
        let status = loop {
            ensure!(
                started.elapsed() < Duration::from_secs(900) && output.metadata()?.len() <= 8 << 20,
                "fixture process limit"
            );
            if let Some(s) = child.try_wait()? {
                break s;
            }
            thread::sleep(Duration::from_millis(20));
        };
        output.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        output.take((8 << 20) + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 8 << 20, "fixture output limit");
        if !status.success() {
            return Err(FixtureExit {
                i32_code: status.code().unwrap_or(1),
                body: bytes,
            }
            .into());
        }
        Ok(bytes)
    })();
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    result
}
fn text(program: &str, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(command(program, args)?)?
        .trim()
        .to_owned())
}
fn compose(args: &[&str]) -> Result<Vec<u8>> {
    let mut full = vec!["compose"];
    full.extend_from_slice(args);
    command("docker", &full)
}
fn wait(mut check: impl FnMut() -> Result<bool>, seconds: u64) -> Result<()> {
    let at = Instant::now();
    while at.elapsed() < Duration::from_secs(seconds) {
        if check().unwrap_or(false) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("fixture readiness deadline")
}
fn binary() -> String {
    std::env::var("OPEN_ASSETS_BINARY")
        .unwrap_or_else(|_| format!("{ROOT}/target/debug/open-assets"))
}
fn app(args: &[&str]) -> Result<Vec<u8>> {
    command(&binary(), args)
}
fn worker(args: &[&str]) -> Result<Vec<u8>> {
    let mut a = vec!["exec", "-T", "publisher", "open-assets"];
    a.extend_from_slice(args);
    let started = Instant::now();
    loop {
        match compose(&a) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let busy = e.downcast_ref::<FixtureExit>().is_some_and(|e| {
                    e.i32_code == 2
                        && serde_json::from_slice::<Value>(&e.body)
                            .is_ok_and(|v| v["status"] == "busy")
                });
                if !busy || started.elapsed() > Duration::from_secs(30) {
                    return Err(e);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
fn read(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn api(client: &Client, password: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let u = format!("http://127.0.0.1:13000/api/v1/{path}");
    let req = if let Some(body) = body {
        client.post(u).json(&body)
    } else {
        client.get(u)
    };
    let r = req.basic_auth("content", Some(password)).send()?;
    ensure!(r.status().is_success(), "fixture API refused");
    Ok(r.json()?)
}
fn wait_job(sha: &str, published: bool) -> Result<Value> {
    let mut found = None;
    wait(
        || {
            for e in fs::read_dir("/data/asset-stack/publisher/jobs")? {
                let p = e?.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let v = read(p)?;
                if v["key"] == "main-images"
                    && v["sha"] == sha
                    && matches!(
                        v["status"].as_str(),
                        Some("published" | "failed" | "superseded")
                    )
                {
                    found = Some(v);
                    return Ok(true);
                }
            }
            Ok(false)
        },
        120,
    )?;
    let found = found.context("missing fixture job")?;
    ensure!(
        found["status"] == if published { "published" } else { "failed" },
        "unexpected publication outcome"
    );
    Ok(found)
}
fn git(path: &Path, args: &[&str]) -> Result<String> {
    let mut full = vec!["-C", path.to_str().context("fixture Git path")?];
    full.extend_from_slice(args);
    text("git", &full)
}
fn commit_push(path: &Path, message: &str) -> Result<String> {
    git(path, &["add", "public"])?;
    git(
        path,
        &["-c", "commit.gpgsign=false", "commit", "-qm", message],
    )?;
    let sha = git(path, &["rev-parse", "HEAD"])?;
    git(path, &["push", "origin", "HEAD:refs/heads/main"])?;
    Ok(sha)
}
fn check_image(client: &Client, path: &str, body: &[u8], mime: &str) -> Result<()> {
    let url = if path.starts_with("https://") {
        path.to_owned()
    } else {
        format!("http://127.0.0.1:18081/{path}")
    };
    let r = client.get(&url).send()?;
    ensure!(
        r.status() == 200
            && r.headers()["content-type"].to_str()?.split(';').next() == Some(mime)
            && r.headers()["x-content-type-options"] == "nosniff"
            && r.headers()["content-security-policy"]
                .to_str()?
                .contains("sandbox"),
        "image response policy mismatch"
    );
    let etag = r.headers()["etag"].to_str()?.to_owned();
    ensure!(&r.bytes()?[..] == body, "image bytes mismatch");
    let r = client.head(&url).send()?;
    ensure!(
        r.status() == 200
            && r.headers()["content-length"].to_str()?.parse::<usize>()? == body.len(),
        "image HEAD mismatch"
    );
    let r = client.get(&url).header("Range", "bytes=0-7").send()?;
    ensure!(
        r.status() == 206 && r.bytes()?[..] == body[..8],
        "image range mismatch"
    );
    ensure!(
        client
            .get(&url)
            .header("If-None-Match", etag)
            .send()?
            .status()
            == 304,
        "image conditional GET mismatch"
    );
    Ok(())
}
fn fixture_s3(
    client: &Client,
    method: reqwest::Method,
    key: &str,
    etag: Option<&str>,
) -> Result<reqwest::blocking::Response> {
    use sha2::{Digest, Sha256};
    ensure!(
        key.starts_with("repositories/main-images/releases/") && key.ends_with("/readme.txt"),
        "fixture object scope"
    );
    let access = fs::read_to_string(format!("{ROOT}/private/object-cleaner/cleaner_access"))?;
    let secret = fs::read_to_string(format!("{ROOT}/private/object-cleaner/cleaner_secret"))?;
    let url = reqwest::Url::parse(&format!("https://storage.internal:19000/attachments/{key}"))?;
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(etag) = etag {
        headers.insert("if-match", etag.parse()?);
    }
    let signed = open_assets::storage::signing_headers(
        method.as_str(),
        &url,
        &headers,
        &hex::encode(Sha256::digest(b"")),
        access.trim(),
        secret.trim(),
        &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
    )?;
    Ok(client.request(method, url).headers(signed).send()?)
}
fn storage_fixture_client() -> Result<Client> {
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .resolve("storage.internal", "127.0.0.1:19000".parse()?);
    for ca in
        reqwest::Certificate::from_pem_bundle(&fs::read(format!("{ROOT}/private/tls/ca.crt"))?)?
    {
        builder = builder.add_root_certificate(ca);
    }
    Ok(builder.build()?)
}
fn edge_fixture(client: &Client) -> Result<(Client, Value)> {
    let p = PathBuf::from(format!("{ROOT}/private/e2e/edge"));
    fs::create_dir_all(p.join("dynamic"))?;
    for dir in [&p, &p.join("dynamic")] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    }
    let domains = read(format!("{ROOT}/private/domains.json"))?;
    let sans = domains
        .as_object()
        .context("domains")?
        .values()
        .map(|v| format!("DNS:{}", v.as_str().unwrap()))
        .collect::<Vec<_>>()
        .join(",");
    fs::write(
        p.join("extensions"),
        format!("subjectAltName={sans}\nextendedKeyUsage=serverAuth\n"),
    )?;
    let x = |name: &str| p.join(name).to_string_lossy().into_owned();
    command(
        "openssl",
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=Rust Fresh Stack",
            "-keyout",
            &x("edge.key"),
            "-out",
            &x("edge.csr"),
        ],
    )?;
    command(
        "openssl",
        &[
            "x509",
            "-req",
            "-in",
            &x("edge.csr"),
            "-CA",
            &format!("{ROOT}/private/tls/ca.crt"),
            "-CAkey",
            &format!("{ROOT}/private/tls/issuers/storage/root.key"),
            "-CAcreateserial",
            "-days",
            "2",
            "-extfile",
            &x("extensions"),
            "-out",
            &x("edge.crt"),
        ],
    )?;
    open_assets::ops::chown(&p.join("edge.key"), 20003, 20003)?;
    fs::set_permissions(p.join("edge.key"), fs::Permissions::from_mode(0o600))?;
    app(&[
        "configure-edge",
        "--reference-releases",
        "--domains",
        "private/domains.json",
        "--ca",
        "private/tls/ca.crt",
        "--output",
        &x("dynamic/routes.yaml"),
    ])?;
    let mut routes: Value = serde_yaml::from_slice(&fs::read(p.join("dynamic/routes.yaml"))?)?;
    for router in routes["http"]["routers"]
        .as_object_mut()
        .context("routers")?
        .values_mut()
    {
        router["tls"] = json!({});
    }
    fs::write(p.join("dynamic/routes.yaml"), serde_json::to_vec(&routes)?)?;
    fs::write(
        p.join("dynamic/tls.yaml"),
        serde_json::to_vec(
            &json!({"tls":{"certificates":[{"certFile":"/etc/traefik/edge.crt","keyFile":"/etc/traefik/edge.key"}]}}),
        )?,
    )?;
    fs::write(
        p.join("static.yaml"),
        serde_json::to_vec(
            &json!({"entryPoints":{"websecure":{"address":":8443"}},"providers":{"file":{"directory":"/etc/traefik/dynamic"}},"api":{"dashboard":true},"global":{"checkNewVersion":false,"sendAnonymousUsage":false},"log":{"level":"ERROR"}}),
        )?,
    )?;
    for file in [
        "dynamic/routes.yaml",
        "dynamic/tls.yaml",
        "static.yaml",
        "edge.crt",
    ] {
        fs::set_permissions(p.join(file), fs::Permissions::from_mode(0o644))?;
    }
    let env = open_assets::info::environment(Path::new(ROOT))?;
    let image = env.get("TRAEFIK_IMAGE").context("gateway image")?;
    let config = json!({"name":"open-assets-rust-e2e-edge","services":{"gateway":{"image":image,"user":"20003:20003","read_only":true,"cap_drop":["ALL"],"command":["--configFile=/etc/traefik/static.yaml"],"ports":["127.0.0.1:443:8443"],"volumes":[format!("{}:/etc/traefik:ro",p.display())],"networks":{"edge":{},"auth":{"ipv4_address":"172.29.19.2"}}}},"networks":{"edge":{"external":true,"name":"open-assets-edge"},"auth":{"external":true,"name":"open-assets-auth"}}});
    fs::write(p.join("compose.yaml"), serde_json::to_vec(&config)?)?;
    command("docker", &["compose", "-f", &x("compose.yaml"), "up", "-d"])?;
    let mut hosts = fs::OpenOptions::new().append(true).open("/etc/hosts")?;
    writeln!(
        hosts,
        "\n127.0.0.1 {}",
        domains
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    )?;
    let pem = fs::read(format!("{ROOT}/private/tls/ca.crt"))?;
    let mut builder = Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&pem)?)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10));
    for v in domains.as_object().unwrap().values() {
        builder = builder.resolve(v.as_str().unwrap(), "127.0.0.1:443".parse()?);
    }
    let secured = builder.build()?;
    wait(
        || {
            Ok(secured
                .get(format!(
                    "https://{}/healthz",
                    domains["auth"].as_str().unwrap()
                ))
                .send()?
                .status()
                == 200)
        },
        45,
    )?;
    for role in ["git", "info", "storage", "traefik"] {
        let response = secured
            .get(format!(
                "https://{}/",
                domains[role].as_str().context("management domain")?
            ))
            .send()?;
        ensure!(
            response.status().is_redirection(),
            "anonymous management access was not redirected"
        );
        let location = response.headers()["location"].to_str()?;
        let target = reqwest::Url::parse(location)?;
        ensure!(
            target.scheme() == "https"
                && target.host_str() == domains["auth"].as_str()
                && target.username().is_empty()
                && target.password().is_none(),
            "management redirect escaped authentication entry"
        );
    }
    let _ = client;
    Ok((secured, domains))
}
fn run() -> Result<()> {
    ensure!(
        std::env::var("OPEN_ASSETS_FRESH_TEST").as_deref() == Ok("1")
            && cfg!(target_os = "linux")
            && unsafe { libc::geteuid() } == 0,
        "disposable Linux root opt-in required"
    );
    ensure!(
        Path::new(ROOT).canonicalize()? == Path::new(ROOT)
            && !Path::new(&format!("{ROOT}/private")).exists()
            && !Path::new("/data/asset-stack").exists(),
        "refusing existing deployment state"
    );
    ensure!(
        text("docker", &["ps", "-aq"])?.is_empty(),
        "refusing nonempty Docker daemon"
    );
    std::env::set_current_dir(ROOT)?;
    std::env::set_var(
        "COMPOSE_FILE",
        "compose.yaml:compose.rust.yaml:private/e2e-ports.yaml",
    );
    std::env::set_var("NODE_NAME", "acceptance-node");
    std::env::set_var("PUBLIC_BASE", "acceptance.invalid");
    println!("Rust fresh stack: protected preparation");
    app(&["prepare-private", "--root", ROOT])?;
    app(&[
        "deployment-config",
        "--env",
        ".env",
        "--domains",
        "private/domains.json",
    ])?;
    fs::write(
        "private/e2e-ports.yaml",
        serde_json::to_vec(
            &json!({"services":{"nginx":{"ports":["127.0.0.1:18081:8080"]},"info":{"ports":["127.0.0.1:18082:8082"]},"object-reader":{"ports":["127.0.0.1:18083:8083"]},"webhook":{"ports":["127.0.0.1:19080:9000"]}}}),
        )?,
    )?;
    compose(&["config", "--quiet"])?;
    compose(&["pull", "otterio", "oc", "stargate", "gitea", "nginx"])?;
    compose(&["build", "publisher"])?;
    compose(&[
        "up",
        "-d",
        "--no-build",
        "otterio",
        "gitea",
        "stargate",
        "oc",
    ])?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    wait(
        || {
            Ok(client
                .get("http://127.0.0.1:13000/api/healthz")
                .send()?
                .status()
                == 200
                && String::from_utf8(compose(&["logs", "otterio"])?)?
                    .contains("IAM initialization complete"))
        },
        120,
    )?;
    println!("Rust fresh stack: initialize identities and signed hook");
    app(&["initialize-services", "--root", ROOT])?;
    let mut config: Value = serde_yaml::from_slice(&fs::read("private/repositories.yaml")?)?;
    config["publication"]["reference_releases"] = json!(true);
    fs::write("private/repositories.yaml", serde_yaml::to_string(&config)?)?;
    compose(&[
        "run",
        "--rm",
        "--no-deps",
        "-v",
        "/app/open-assets/private/generated:/generated",
        "publisher",
        "render",
        "/generated",
    ])?;
    compose(&[
        "up",
        "-d",
        "--no-build",
        "nginx",
        "webhook",
        "publisher",
        "object-reader",
        "info",
    ])?;
    wait(
        || {
            Ok(client
                .get("http://127.0.0.1:19080/healthz")
                .send()?
                .status()
                == 200
                && client
                    .get("http://127.0.0.1:18082/healthz")
                    .send()?
                    .status()
                    == 200
                && read("/data/asset-stack/publisher/worker-heartbeat.json")?["updated_at"]
                    .as_f64()
                    .is_some_and(|t| open_assets::common::now() - t < 30.0))
        },
        60,
    )?;
    fs::create_dir_all("private/e2e/client/public/images/2026/10/10")?;
    let fixture = Path::new(ROOT).join("private/e2e");
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700))?;
    let identity = fixture.join("client-key");
    command(
        "ssh-keygen",
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            identity.to_str().unwrap(),
        ],
    )?;
    let password = fs::read_to_string("private/admin_password")?;
    api(
        &client,
        password.trim(),
        "user/keys",
        Some(
            json!({"title":"rust-e2e-client","key":fs::read_to_string(identity.with_extension("pub"))?}),
        ),
    )?;
    let known = fixture.join("known_hosts");
    fs::write(
        &known,
        command("ssh-keyscan", &["-p", "2222", "-t", "ed25519", "127.0.0.1"])?,
    )?;
    std::env::set_var("GIT_SSH_COMMAND",format!("ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={}",identity.display(),known.display()));
    let repo = fixture.join("client");
    git(&repo, &["init", "-b", "main", "-q"])?;
    git(&repo, &["config", "user.name", "Rust Acceptance"])?;
    git(
        &repo,
        &["config", "user.email", "acceptance@localhost.invalid"],
    )?;
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "ssh://git@127.0.0.1:2222/content/images.git",
        ],
    )?;
    let png=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=")?;
    fs::write(repo.join("public/images/fixed.png"), &png)?;
    let svg=br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>"#;
    fs::write(repo.join("public/images/2026/10/10/image.svg"), svg)?;
    fs::write(repo.join("public/readme.txt"), b"original attachment")?;
    println!("Rust fresh stack: signed SSH publication");
    let first = commit_push(&repo, "first Rust publication")?;
    let first_job = wait_job(&first, true)?;
    ensure!(
        first_job["status"] == "published",
        "first publication failed"
    );
    let state = Path::new("/data/asset-stack/publisher");
    let images = Path::new("/data/asset-stack/images/repos/main-images");
    let old_inode = fs::metadata(images.join("current/fixed.png"))?.ino();
    check_image(&client, "fixed.png", &png, "image/png")?;
    check_image(&client, "2026/10/10/image.svg", svg, "image/svg+xml")?;
    let updated=br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8"/></svg>"#;
    fs::write(repo.join("public/images/2026/10/10/image.svg"), updated)?;
    let second = commit_push(&repo, "incremental Rust publication")?;
    let job = wait_job(&second, true)?;
    ensure!(
        fs::metadata(images.join("current/fixed.png"))?.ino() == old_inode,
        "unchanged bytes were duplicated"
    );
    ensure!(
        job["incremental"]["object_transfers"]["copied"]
            .as_u64()
            .unwrap_or(0)
            == 0
            && job["incremental"]["object_transfers"]["uploaded"] == 1,
        "incremental object reuse failed"
    );
    check_image(&client, "2026/10/10/image.svg", updated, "image/svg+xml")?;
    worker(&["rollback-images", "main-images", &first])?;
    check_image(&client, "2026/10/10/image.svg", svg, "image/svg+xml")?;
    worker(&["rollback-images", "main-images", &second])?;
    check_image(&client, "2026/10/10/image.svg", updated, "image/svg+xml")?;
    fs::write(
        repo.join("public/images/rejected.svg"),
        br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#,
    )?;
    let bad = commit_push(&repo, "unsafe SVG must fail")?;
    wait_job(&bad, false)?;
    ensure!(
        read(state.join("published/main-images"))?["sha"] == second,
        "unsafe publication changed current version"
    );
    check_image(&client, "2026/10/10/image.svg", updated, "image/svg+xml")?;
    app(&["operator", "--root", ROOT, "objects", "setup"])?;
    let storage_client = storage_fixture_client()?;
    let first_manifest = read(
        state
            .join("manifests/main-images")
            .join(format!("{first}.json")),
    )?;
    let missing_key = first_manifest["object_sources"]["readme.txt"]["key"]
        .as_str()
        .context("fixture source object")?;
    ensure!(
        missing_key == format!("repositories/main-images/releases/{first}/readme.txt"),
        "refusing unexpected fixture deletion target"
    );
    let original_object = fixture_s3(&storage_client, reqwest::Method::GET, missing_key, None)?;
    ensure!(
        original_object.status() == 200,
        "fixture object missing before deletion"
    );
    let object_etag = original_object.headers()["etag"].to_str()?.to_owned();
    ensure!(
        &original_object.bytes()?[..] == b"original attachment",
        "fixture object bytes before deletion"
    );
    ensure!(
        fixture_s3(
            &storage_client,
            reqwest::Method::DELETE,
            missing_key,
            Some(&object_etag)
        )?
        .status()
        .is_success(),
        "fixture deletion failed"
    );
    ensure!(
        fixture_s3(&storage_client, reqwest::Method::HEAD, missing_key, None)?.status() == 404,
        "fixture object was not removed"
    );
    let restored: Value =
        serde_json::from_slice(&worker(&["restore-objects", "main-images", &first])?)?;
    ensure!(
        restored["status"] == "complete"
            && restored["uploaded_objects"]
                .as_u64()
                .is_some_and(|n| n >= 1),
        "missing object was not restored"
    );
    let object = fixture_s3(&storage_client, reqwest::Method::GET, missing_key, None)?;
    ensure!(
        object.status() == 200
            && object.headers()["content-type"]
                .to_str()?
                .starts_with("text/plain")
            && &object.bytes()?[..] == b"original attachment",
        "restored object metadata or bytes mismatch"
    );
    let full = images.join("releases").join(&first);
    let saved = images.join(".e2e-saved-local");
    fs::rename(&full, &saved)?;
    let rebuilt: Value = serde_json::from_slice(&worker(&[
        "restore-objects",
        "main-images",
        &first,
        "--with-local",
    ])?)?;
    ensure!(
        rebuilt["local_full_rebuilt"] == true
            && rebuilt["local_verified"] == true
            && fs::read(full.join("images/fixed.png"))? == png,
        "missing local snapshot was not rebuilt"
    );
    ensure!(
        read(state.join("published/main-images"))?["sha"] == second,
        "restore without rollback changed publication"
    );
    worker(&["deduplicate-local", "--apply"])?;
    let checked: Value =
        serde_json::from_slice(&worker(&["check-restore", "main-images", &first])?)?;
    ensure!(
        checked["diagnosis"] == "verified",
        "restoration verification failed"
    );
    let preview: Value =
        serde_json::from_slice(&compose(&["run", "--rm", "--no-deps", "object-cleaner"])?)?;
    ensure!(
        preview["status"] == "complete" || preview["status"] == "disabled",
        "cleanup preview unavailable"
    );
    ensure!(
        read(state.join("published/main-images"))?["sha"] == second,
        "maintenance changed publication"
    );
    app(&["telemetry", "--root", ROOT])?;
    wait(
        || {
            Ok(client
                .get("http://127.0.0.1:18082/api/status")
                .send()?
                .status()
                == 200)
        },
        20,
    )?;
    println!("Rust fresh stack: HTTPS and management authentication");
    let (secured, domains) = edge_fixture(&client)?;
    let image = secured
        .get(format!(
            "https://{}/2026/10/10/image.svg",
            domains["img"].as_str().unwrap()
        ))
        .send()?;
    ensure!(
        image.status() == 200 && &image.bytes()?[..] == updated,
        "HTTPS image route mismatch"
    );
    for (path, body, mime) in [
        ("images/fixed.png", png.as_slice(), "image/png"),
        (
            "images/2026/10/10/image.svg",
            updated.as_slice(),
            "image/svg+xml",
        ),
    ] {
        check_image(
            &secured,
            &format!(
                "https://{}/attachments/repositories/main-images/releases/{second}/{path}",
                domains["oss"].as_str().unwrap()
            ),
            body,
            mime,
        )?;
    }
    for role in ["publisher", "webhook", "info", "object-reader"] {
        compose(&["exec", "-T", role, "open-assets", "health", role])?;
        ensure!(
            compose(&[
                "exec",
                "-T",
                role,
                "sh",
                "-c",
                "command -v python || command -v python3"
            ])
            .is_err(),
            "Python interpreter present in Rust runtime"
        );
    }
    ensure!(
        fs::metadata(images.join(format!("releases/{first}/images/fixed.png")))?.ino()
            == fs::metadata(images.join(format!("releases/{second}/images/fixed.png")))?.ino(),
        "retained snapshots duplicated image bytes"
    );
    println!("RUST_FRESH_STACK_OK: SSH push, reference reuse, immutable inode, SVG rejection, rollback, restore/check, PNG/SVG headers, authenticated HTTPS routes, Info and Python-free runtimes");
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!(
            "Rust fresh-stack acceptance failed: {error}; private fixtures are never uploaded."
        );
        for role in ["publication", "webhook"] {
            let p = Path::new("/data/asset-stack/publisher/private-diagnostics")
                .join(format!("{role}.json"));
            if let Ok(v) = read(p) {
                let mut safe = json!({});
                for k in ["schema_version", "phase", "code", "at"] {
                    if let Some(value) = v.get(k) {
                        safe[k] = value.clone();
                    }
                }
                eprintln!("Private diagnostic category {role}: {safe}");
            }
        }
        std::process::exit(1);
    }
}
