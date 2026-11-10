use super::*;
use rand::RngCore;
use std::os::unix::fs::MetadataExt;
struct Manager {
    root: PathBuf,
    private: PathBuf,
    env: BTreeMap<String, String>,
    client: reqwest::blocking::Client,
    password: String,
}
impl Manager {
    fn new(root: &Path) -> Result<Self> {
        host_only()?;
        let root = protected_root(root)?;
        let private = root.join("private");
        let password = String::from_utf8(common::read_regular(
            &private.join("admin_password"),
            16384,
        )?)?
        .trim()
        .into();
        Ok(Self {
            env: info::environment(&root)?,
            root,
            private,
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            password,
        })
    }
    fn api(
        &self,
        path: &str,
        value: Option<&Value>,
        method: Option<reqwest::Method>,
        missing: bool,
    ) -> Result<Value> {
        let url = format!("http://127.0.0.1:13000/api/v1/{path}");
        let method = method.unwrap_or(if value.is_some() {
            reqwest::Method::POST
        } else {
            reqwest::Method::GET
        });
        let mut r = self
            .client
            .request(method, url)
            .basic_auth("content", Some(&self.password));
        if let Some(v) = value {
            r = r.json(v);
        }
        let mut response = r
            .send()
            .map_err(|_| anyhow::anyhow!("Gitea request unavailable"))?;
        if missing && response.status() == 404 {
            return Ok(Value::Null);
        }
        ensure!(response.status().is_success(), "Gitea request rejected");
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 2 * 1024 * 1024, "Gitea response budget");
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
    fn collection(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for page in 1..=200 {
            let rows = self.api(&format!("{path}?limit=50&page={page}"), None, None, false)?;
            let a = rows.as_array().context("invalid Gitea collection")?;
            ensure!(a.len() <= 50, "Gitea pagination budget");
            out.extend(a.iter().cloned());
            if a.len() < 50 {
                return Ok(out);
            }
        }
        bail!("Gitea pagination budget")
    }
    fn compose(&self, args: &[&str]) -> Result<String> {
        let mut a = vec!["compose"];
        a.extend_from_slice(args);
        command("docker", &a, Some(&self.root))
    }
    fn oc(&self, args: &[&str]) -> Result<String> {
        let config = format!("{}:/config", self.private.join("oc-cli").display());
        let ca = format!("{}:/ca.crt:ro", self.private.join("tls/ca.crt").display());
        let image = self.env.get("OC_IMAGE").context("OC_IMAGE")?;
        let mut a = vec![
            "run",
            "--rm",
            "--network",
            "asset-stack_admin",
            "-v",
            &config,
            "-v",
            &ca,
            image,
            "--config-dir",
            "/config",
            "--json",
        ];
        a.extend_from_slice(args);
        command("docker", &a, Some(&self.root))
    }
    fn sec(&self, name: &str) -> Result<String> {
        Ok(
            String::from_utf8(common::read_regular(&self.private.join(name), 16384)?)?
                .trim()
                .to_owned(),
        )
    }
    fn apply_policy(&self) -> Result<()> {
        self.oc(&[
            "admin",
            "policy",
            "add",
            "store",
            "assets-publish",
            "/config/publisher.json",
        ])?;
        let user = self.sec("publisher/publisher_access")?;
        let identity: Value =
            serde_json::from_str(&self.oc(&["admin", "user", "info", "store", &user])?)?;
        ensure!(
            identity["userStatus"] == "enabled",
            "publisher account not enabled"
        );
        if !identity["policyName"]
            .as_str()
            .unwrap_or("")
            .split(',')
            .any(|p| p == "assets-publish")
        {
            self.oc(&[
                "admin",
                "policy",
                "update",
                "store",
                "assets-publish",
                &format!("user={user}"),
            ])?;
        }
        Ok(())
    }
    fn check_prefix(&self, c: &Value, key: &str, mount: &str) -> Result<()> {
        let rs = c["repositories"].as_array().context("repositories")?;
        if mount != "/" && !rs.iter().any(|r| r["key"] == key) {
            for r in rs.iter().filter(|r| r["mount_path"] == "/") {
                let path = Path::new(self.env.get("DATA_ROOT").context("DATA_ROOT")?)
                    .join("images/repos")
                    .join(r["key"].as_str().context("key")?)
                    .join("current")
                    .join(mount.trim_matches('/'));
                ensure!(!path.exists(), "new prefix shadows root image files");
            }
        }
        Ok(())
    }
}
fn identity(name: &str, key: &str, mount: &str, branch: &str) -> Result<()> {
    let valid = |s: &str| {
        !s.is_empty()
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    };
    let parts: Vec<_> = name.split('/').collect();
    ensure!(
        parts.len() == 2 && parts.iter().all(|s| valid(s)),
        "repository must be owner/name"
    );
    ensure!(
        regex::Regex::new(r"^[a-z][a-z0-9-]{0,47}$")?.is_match(key),
        "invalid repository key"
    );
    ensure!(
        regex::Regex::new(r"^/(?:[a-z0-9][a-z0-9-]*/)?$")?.is_match(mount),
        "invalid mount"
    );
    ensure!(
        regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/-]*$")?.is_match(branch)
            && !branch.contains(".."),
        "invalid branch"
    );
    Ok(())
}
fn desired_config(
    c: &Value,
    name: &str,
    key: &str,
    mount: &str,
    branch: &str,
    id: u64,
) -> Result<Value> {
    identity(name, key, mount, branch)?;
    let mut result = c.clone();
    let desired = json!({"key":key,"name":name,"repository_id":id,"remote":format!("ssh://git@gitea:2222/{name}.git"),"branch":branch,"enabled":true,"serve":true,"source_directory":"public","mount_path":mount,"max_files":10000,"max_bytes":1073741824,"max_file_bytes":104857600});
    let rs = result["repositories"]
        .as_array_mut()
        .context("repositories")?;
    if let Some(old) = rs.iter().find(|r| r["key"] == key) {
        for f in [
            "name",
            "repository_id",
            "remote",
            "branch",
            "source_directory",
            "mount_path",
        ] {
            ensure!(old[f] == desired[f], "existing key has different identity");
        }
        ensure!(
            old["enabled"] == true && old["serve"].as_bool().unwrap_or(true),
            "existing repository disabled"
        );
        return Ok(result);
    }
    for old in rs.iter() {
        ensure!(
            !old["name"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(name))
                && old["repository_id"] != id,
            "repository already bound"
        );
        let m = old["mount_path"].as_str().context("mount")?;
        ensure!(
            m != mount
                && !(m != "/" && mount != "/" && (m.starts_with(mount) || mount.starts_with(m))),
            "URL prefix conflict"
        );
    }
    rs.push(desired);
    Ok(result)
}
fn extend_policy(policy: &Value, bucket: &str, key: &str) -> Result<Value> {
    let mut result = policy.clone();
    let statements = result["Statement"]
        .as_array_mut()
        .context("IAM policy statements")?;
    let matches: Vec<usize> = statements
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s["Effect"] == "Allow"
                && s["Action"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a == "s3:PutObject"))
        })
        .map(|(i, _)| i)
        .collect();
    ensure!(
        matches.len() == 1,
        "expected one publisher object-write statement"
    );
    let statement = &mut statements[matches[0]];
    let mut resources = match &statement["Resource"] {
        Value::String(s) => vec![json!(s)],
        Value::Array(a) => a.clone(),
        _ => bail!("invalid IAM resource"),
    };
    let target = json!(format!("arn:aws:s3:::{bucket}/repositories/{key}/*"));
    if !resources.contains(&target) {
        resources.push(target);
    }
    statement["Resource"] = json!(resources);
    Ok(result)
}
pub fn ensure_repository(root: &Path, args: &[String]) -> Result<()> {
    let manager = Manager::new(root)?;
    let _guard = info::lock_file(&manager.private.join("onboarding.lock"), false)?;
    let name = args.first().context("repository name")?;
    let key = flag(args, "--key").context("--key required")?;
    let mount = flag(args, "--mount-path").context("--mount-path required")?;
    let branch = flag(args, "--branch").unwrap_or("main");
    identity(name, key, mount, branch)?;
    let config_path = manager.private.join("repositories.yaml");
    let original = common::read_regular(&config_path, 1024 * 1024)?;
    let c: Value = serde_yaml::from_slice(&original)?;
    let candidate_id = c["repositories"]
        .as_array()
        .context("repositories")?
        .iter()
        .find(|r| r["key"] == key)
        .and_then(|r| r["repository_id"].as_u64())
        .unwrap_or_else(|| {
            c["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|r| r["repository_id"].as_u64())
                .max()
                .unwrap_or(0)
                + 1
        });
    common::validate(&desired_config(&c, name, key, mount, branch, candidate_id)?)?;
    manager.check_prefix(&c, key, mount)?;
    let live: Value = serde_json::from_str(&manager.oc(&[
        "admin",
        "policy",
        "info",
        "store",
        "assets-publish",
    ])?)?;
    let live_policy = live.get("policyJSON").context("live IAM policy")?;
    let policy = extend_policy(live_policy, c["bucket"].as_str().context("bucket")?, key)?;
    let public_key = manager.sec("publisher/deploy_key.pub")?;
    let hook_secret = manager.sec("webhook_secret")?;
    ensure!(
        public_key.split_whitespace().count() >= 2 && !hook_secret.is_empty(),
        "missing key or webhook secret"
    );
    manager.compose(&["config", "--quiet"])?;
    let mut repository = manager.api(&format!("repos/{name}"), None, None, true)?;
    if repository.is_null() {
        let (owner, short) = name.split_once('/').context("name")?;
        let endpoint = if owner == "content" {
            "user/repos".to_owned()
        } else {
            ensure!(
                !manager
                    .api(&format!("orgs/{owner}"), None, None, true)?
                    .is_null(),
                "inaccessible organization"
            );
            format!("orgs/{owner}/repos")
        };
        repository = manager.api(
            &endpoint,
            Some(&json!({"name":short,"private":true,"auto_init":false,"default_branch":branch})),
            None,
            false,
        )?;
    }
    ensure!(
        repository["private"] == true && repository["archived"] != true,
        "existing repository must be private and active"
    );
    let desired = desired_config(
        &c,
        name,
        key,
        mount,
        branch,
        repository["id"].as_u64().context("repository ID")?,
    )?;
    let keys = manager.collection(&format!("repos/{name}/keys"))?;
    let public_parts: Vec<_> = public_key.split_whitespace().take(2).collect();
    let matching: Vec<_> = keys
        .iter()
        .filter(|k| {
            k["key"]
                .as_str()
                .is_some_and(|s| s.split_whitespace().take(2).collect::<Vec<_>>() == public_parts)
        })
        .collect();
    ensure!(
        matching.iter().all(|k| k["read_only"] == true),
        "publisher key has write access"
    );
    if matching.is_empty() {
        manager.api(
            &format!("repos/{name}/keys"),
            Some(&json!({"title":"open-assets-publisher","key":public_key,"read_only":true})),
            None,
            false,
        )?;
    }
    let hooks = manager.collection(&format!("repos/{name}/hooks"))?;
    let url = "http://webhook:9000/hooks/publish-assets";
    let matching: Vec<_> = hooks.iter().filter(|h| h["config"]["url"] == url).collect();
    ensure!(matching.len() <= 1, "duplicate publisher webhooks");
    let hook = json!({"type":"gitea","active":true,"events":["push"],"config":{"url":url,"content_type":"json","secret":hook_secret}});
    if let Some(h) = matching.first() {
        ensure!(h["type"] == "gitea", "incompatible webhook provider");
        manager.api(
            &format!(
                "repos/{name}/hooks/{}",
                h["id"].as_u64().context("hook ID")?
            ),
            Some(&hook),
            Some(reqwest::Method::PATCH),
            false,
        )?;
    } else {
        manager.api(&format!("repos/{name}/hooks"), Some(&hook), None, false)?;
    }
    let backup = manager.private.join("onboarding").join(key);
    fs::create_dir_all(&backup)?;
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o700))?;
    for (path, bytes) in [
        (backup.join("repositories.before.yaml"), original),
        (
            backup.join("publisher.before.json"),
            serde_json::to_vec_pretty(live_policy)?,
        ),
    ] {
        if !path.exists() {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
    }
    private_write(
        &manager.private.join("oc-cli/publisher.json"),
        &serde_json::to_vec_pretty(&policy)?,
        None,
    )?;
    manager.apply_policy()?;
    let marker = backup.join("result.json");
    let previous = info::read_json_bounded(&marker, 16384).unwrap_or(json!({}));
    let changed = desired != c;
    let recreate =
        changed || previous["config_inode"].as_u64() != Some(fs::metadata(&config_path)?.ino());
    let mut stopped = false;
    let result = (|| -> Result<()> {
        if changed {
            manager.compose(&["stop", "webhook", "publisher"])?;
            stopped = true;
            manager.check_prefix(&c, key, mount)?;
            private_write(
                &config_path,
                serde_yaml::to_string(&desired)?.as_bytes(),
                Some(20010),
            )?;
        }
        manager.compose(&["run", "--rm", "--no-deps", "publisher", "validate"])?;
        let generated = manager.private.join("generated");
        let bind = format!("{}:/generated", generated.display());
        let render_args = onboarding_render_args(&generated, &bind)?;
        manager.compose(&render_args.iter().map(String::as_str).collect::<Vec<_>>())?;
        preserve_onboarding_policy(&generated)?;

        let live = generated.join("images.conf");
        let old = fs::read(&live)?;
        let new = fs::read(generated.join(".onboarding/images.conf"))?;
        if old != new {
            if !backup.join("images.before.conf").exists() {
                private_write(&backup.join("images.before.conf"), &old, None)?;
            }
            let mut f = OpenOptions::new()
                .write(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&live)?;
            f.write_all(&new)?;
            f.sync_all()?;
            let check = manager
                .compose(&[
                    "exec",
                    "-T",
                    "nginx",
                    "nginx",
                    "-c",
                    "/etc/nginx/open-assets.conf",
                    "-t",
                ])
                .and_then(|_| {
                    manager.compose(&[
                        "exec",
                        "-T",
                        "nginx",
                        "nginx",
                        "-c",
                        "/etc/nginx/open-assets.conf",
                        "-s",
                        "reload",
                    ])
                });
            if check.is_err() {
                fs::write(&live, &old)?;
                bail!("Nginx candidate validation failed; prior config restored");
            }
        }
        if recreate {
            manager.compose(&[
                "up",
                "-d",
                "--no-build",
                "--force-recreate",
                "webhook",
                "publisher",
            ])?;
        }
        stopped = false;
        Ok(())
    })();
    if stopped {
        let _ = manager.compose(&[
            "up",
            "-d",
            "--no-build",
            "--force-recreate",
            "webhook",
            "publisher",
        ]);
    }
    result?;
    let receipt = json!({"name":name,"key":key,"repository_id":repository["id"],"mount_path":mount,"branch":branch,"config_inode":fs::metadata(config_path)?.ino(),"services_recreated":recreate});
    private_write(&marker, &serde_json::to_vec_pretty(&receipt)?, None)?;
    let mut public = receipt;
    public.as_object_mut().unwrap().remove("config_inode");
    public["status"] = json!("ready");
    println!("{public}");
    Ok(())
}
// Public access is independently configured: repository onboarding must never
// replace its policy with renderer defaults or add it to repositories.yaml.
fn onboarding_render_args(generated: &Path, bind: &str) -> Result<Vec<String>> {
    let mut args = [
        "run",
        "--rm",
        "--no-deps",
        "-v",
        bind,
        "publisher",
        "render",
        "/generated/.onboarding",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    let policy = generated.join("public-access.json");
    match fs::symlink_metadata(&policy) {
        Ok(_) => {
            crate::public_access::Policy::load(&policy)?;
            args.extend([
                "--public-access-config".into(),
                "/generated/public-access.json".into(),
            ]);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(args)
}
fn preserve_onboarding_policy(generated: &Path) -> Result<()> {
    let live = generated.join("public-access.json");
    let candidate = generated.join(".onboarding/public-access.json");
    let candidate_policy = crate::public_access::Policy::load(&candidate)?;
    // Require an actual generated file; optional loading alone would mistake a
    // missing candidate for a successful default-policy render.
    let candidate_bytes = common::read_regular(&candidate, 65536)?;
    match fs::symlink_metadata(&live) {
        Ok(_) => {
            ensure!(
                crate::public_access::Policy::load(&live)? == candidate_policy,
                "onboarding changed public access policy"
            );
            // Preserve the exact existing JSON and its live inode (Reader uses a
            // file bind mount); only the private candidate needs synchronization.
            let bytes = common::read_regular(&live, 65536)?;
            private_write(&candidate, &bytes, None)?;
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Legacy deployments have no independent policy. Publish the new
            // renderer's defaults without replacing any existing file.
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&live)?;
            f.set_permissions(fs::Permissions::from_mode(0o644))?;
            f.write_all(&candidate_bytes)?;
            f.sync_all()?;
            fs::File::open(generated)?.sync_all()?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn iam_policy(c: &Value, compaction: bool) -> Result<Value> {
    let bucket = c["bucket"].as_str().context("bucket")?;
    let mut scopes = Vec::new();
    let mut prefixes = Vec::new();
    let mut writable = Vec::new();
    for r in c["repositories"].as_array().context("repositories")? {
        let key = r["key"].as_str().context("key")?;
        let prefix = format!("repositories/{key}/releases/*");
        prefixes.push(json!(prefix));
        let scope = json!(format!("arn:aws:s3:::{bucket}/{prefix}"));
        scopes.push(scope.clone());
        let mut p = json!({"enabled":false,"exclusive_writer":false});
        for source in [&c["object_retention"], &r["object_retention"]] {
            for flag in ["enabled", "exclusive_writer"] {
                if let Some(v) = source.get(flag) {
                    ensure!(v.is_boolean(), "invalid object-retention flag");
                    p[flag] = v.clone();
                }
            }
        }
        if compaction || p["enabled"] == true && p["exclusive_writer"] == true {
            writable.push(scope);
        }
    }
    let mut statements = vec![
        json!({"Effect":"Allow","Action":["s3:GetBucketVersioning"],"Resource":[format!("arn:aws:s3:::{bucket}")]}),
        json!({"Effect":"Allow","Action":["s3:ListBucket"],"Resource":[format!("arn:aws:s3:::{bucket}")],"Condition":{"StringLike":{"s3:prefix":prefixes}}}),
        json!({"Effect":"Allow","Action":["s3:GetObject"],"Resource":scopes}),
    ];
    if !writable.is_empty() {
        statements.push(json!({"Effect":"Allow","Action":["s3:DeleteObject"],"Resource":writable}));
    }
    Ok(json!({"Version":"2012-10-17","Statement":statements}))
}
pub fn objects_setup(root: &Path, compaction: bool) -> Result<()> {
    let manager = Manager::new(root)?;
    let _lock = info::lock_file(&manager.private.join("onboarding.lock"), false)?;
    let c = config(root)?;
    let directory = manager.private.join("object-cleaner");
    fs::create_dir_all(&directory)?;
    ensure!(
        fs::symlink_metadata(&directory)?.is_dir(),
        "linked cleaner directory"
    );
    chown(&directory, 20010, 20010)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o750))?;
    let identity = directory.join("identity.json");
    let user = if identity.exists() {
        let saved = info::read_json_bounded(&identity, 16384)?;
        let user = saved["user"].as_str().context("cleaner user")?.to_owned();
        ensure!(
            regex::Regex::new(r"^oa-cleaner-[a-f0-9]{24}$")?.is_match(&user),
            "invalid cleaner identity"
        );
        ensure!(
            String::from_utf8(common::read_regular(
                &directory.join("cleaner_access"),
                16384
            )?)? == user,
            "cleaner identity mismatch"
        );
        common::read_regular(&directory.join("cleaner_secret"), 16384)?;
        user
    } else {
        ensure!(
            fs::read_dir(&directory)?.next().is_none(),
            "unrecognized cleaner directory"
        );
        let mut bytes = [0; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let user = format!("oa-cleaner-{}", hex::encode(&bytes[..12]));
        private_write(&identity, &serde_json::to_vec(&json!({"user":user}))?, None)?;
        private_write(
            &directory.join("cleaner_access"),
            user.as_bytes(),
            Some(20010),
        )?;
        rand::thread_rng().fill_bytes(&mut bytes);
        private_write(
            &directory.join("cleaner_secret"),
            hex::encode(bytes).as_bytes(),
            Some(20010),
        )?;
        user
    };
    let account = manager.oc(&["admin", "user", "info", "store", &user]);
    let info = match account {
        Ok(s) => serde_json::from_str::<Value>(&s)?,
        Err(_) => {
            let secret = String::from_utf8(common::read_regular(
                &directory.join("cleaner_secret"),
                16384,
            )?)?;
            manager.oc(&["admin", "user", "add", "store", &user, &secret])?;
            serde_json::from_str(&manager.oc(&["admin", "user", "info", "store", &user])?)?
        }
    };
    ensure!(
        info["userStatus"] == "enabled"
            && info["policyName"]
                .as_str()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.is_empty())
                .all(|s| s == "assets-object-retention"),
        "unexpected cleaner identity state"
    );
    let policy = iam_policy(&c, compaction)?;
    private_write(
        &manager.private.join("oc-cli/object-retention.json"),
        &serde_json::to_vec(&policy)?,
        None,
    )?;
    manager.oc(&[
        "admin",
        "policy",
        "add",
        "store",
        "assets-object-retention",
        "/config/object-retention.json",
    ])?;
    manager.oc(&[
        "admin",
        "policy",
        "set",
        "store",
        "assets-object-retention",
        &format!("user={user}"),
    ])?;
    private_write(
        &directory.join("ca.crt"),
        &common::read_regular(&manager.private.join("tls/ca.crt"), 1024 * 1024)?,
        Some(20010),
    )?;
    println!("{}", json!({"status":"ready","compaction":compaction}));
    Ok(())
}
pub fn initialize_services(args: &[String]) -> Result<()> {
    host_only()?;
    let root = protected_root(&root_arg(args))?;
    let private = root.join("private");
    let marker = bootstrap::initialize(&private)?;
    let manager = Manager::new(&root)?;
    let data = PathBuf::from(manager.env.get("DATA_ROOT").context("DATA_ROOT")?);
    let users = command(
        "docker",
        &[
            "exec",
            "-u",
            "20001:20001",
            "asset-stack-gitea-1",
            "gitea",
            "admin",
            "user",
            "list",
        ],
        None,
    )?;
    if !users
        .lines()
        .any(|l| l.split_whitespace().any(|s| s == "content"))
    {
        manager.compose(&[
            "exec",
            "-T",
            "-u",
            "20001:20001",
            "gitea",
            "gitea",
            "admin",
            "user",
            "create",
            "--username",
            "content",
            "--password",
            &manager.sec("admin_password")?,
            "--email",
            "admin@localhost.invalid",
            "--admin",
            "--must-change-password=false",
        ])?;
    }
    let mut repository = manager.api("repos/content/images", None, None, true)?;
    if repository.is_null() {
        repository = manager.api(
            "user/repos",
            Some(
                &json!({"name":"images","private":true,"auto_init":false,"default_branch":"main"}),
            ),
            None,
            false,
        )?;
    }
    ensure!(
        repository["private"] == true && repository["archived"] != true,
        "repository identity conflict"
    );
    let public = manager.sec("publisher/deploy_key.pub")?;
    let keys = manager.collection("repos/content/images/keys")?;
    if !keys.iter().any(|k| {
        k["key"].as_str().is_some_and(|s| {
            s.split_whitespace().take(2).collect::<Vec<_>>()
                == public.split_whitespace().take(2).collect::<Vec<_>>()
        }) && k["read_only"] == true
    }) {
        ensure!(keys.is_empty(), "existing deploy key needs explicit review");
        manager.api(
            "repos/content/images/keys",
            Some(&json!({"title":"open-assets-publisher","key":public,"read_only":true})),
            None,
            false,
        )?;
    }
    let hooks = manager.collection("repos/content/images/hooks")?;
    if hooks.is_empty() {
        manager.api("repos/content/images/hooks",Some(&json!({"type":"gitea","active":true,"events":["push"],"config":{"url":"http://webhook:9000/hooks/publish-assets","content_type":"json","secret":manager.sec("webhook_secret")?}})),None,false)?;
    } else {
        ensure!(
            hooks.len() == 1
                && hooks[0]["type"] == "gitea"
                && hooks[0]["config"]["url"] == "http://webhook:9000/hooks/publish-assets",
            "existing webhook needs explicit review"
        );
    }
    let mut c: Value = serde_yaml::from_slice(&common::read_regular(
        &private.join("repositories.yaml"),
        1024 * 1024,
    )?)?;
    c["repositories"][0]["repository_id"] = repository["id"].clone();
    c["repositories"][0]["source_directory"] = json!("public");
    private_write(
        &private.join("repositories.yaml"),
        serde_yaml::to_string(&c)?.as_bytes(),
        Some(20010),
    )?;
    let mut hosts = Vec::new();
    for entry in fs::read_dir(data.join("gitea/data/ssh"))? {
        let p = entry?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("pub") {
            let key = String::from_utf8(common::read_regular(&p, 16384)?)?;
            ensure!(
                key.split_whitespace().count() >= 2,
                "invalid trusted host key"
            );
            hosts.push(format!("[gitea]:2222 {}", key.trim()));
        }
    }
    ensure!(!hosts.is_empty(), "no trusted host keys from Gitea volume");
    private_write(
        &private.join("publisher/known_hosts"),
        (hosts.join("\n") + "\n").as_bytes(),
        Some(20010),
    )?;
    let cli = private.join("oc-cli");
    fs::create_dir_all(cli.join("certs/CAs"))?;
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700))?;
    fs::copy(private.join("tls/ca.crt"), cli.join("certs/CAs/ca.crt"))?;
    manager.oc(&[
        "alias",
        "set",
        "store",
        "https://storage.internal:9000",
        &manager.sec("storage/root_user")?,
        &manager.sec("storage/root_password")?,
        "--admin-url",
        "https://storage.internal:9001",
        "--admin-ca",
        "/ca.crt",
        "--api",
        "s3v4",
        "--path",
        "on",
    ])?;
    manager.oc(&["mb", "--ignore-existing", "store/attachments"])?;
    let public = json!({"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":"*","Action":["s3:GetObject"],"Resource":["arn:aws:s3:::attachments/repositories/*"]}]});
    private_write(
        &cli.join("public.json"),
        &serde_json::to_vec(&public)?,
        None,
    )?;
    manager.oc(&[
        "policy",
        "set-json",
        "/config/public.json",
        "store/attachments",
    ])?;
    for (user, secret) in [
        (
            manager.sec("publisher/publisher_access")?,
            manager.sec("publisher/publisher_secret")?,
        ),
        (
            manager.sec("console_user")?,
            manager.sec("console_password")?,
        ),
    ] {
        manager.oc(&["admin", "user", "add", "store", &user, &secret])?;
    }
    let policy = json!({"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:ListBucket","s3:GetBucketLocation","s3:ListBucketMultipartUploads"],"Resource":["arn:aws:s3:::attachments"]},{"Effect":"Allow","Action":["s3:GetObject","s3:PutObject","s3:AbortMultipartUpload","s3:ListMultipartUploadParts"],"Resource":["arn:aws:s3:::attachments/repositories/main-images/*"]}]});
    private_write(
        &cli.join("publisher.json"),
        &serde_json::to_vec(&policy)?,
        None,
    )?;
    manager.oc(&[
        "admin",
        "policy",
        "add",
        "store",
        "assets-publish",
        "/config/publisher.json",
    ])?;
    manager.oc(&[
        "admin",
        "policy",
        "set",
        "store",
        "assets-publish",
        &format!("user={}", manager.sec("publisher/publisher_access")?),
    ])?;
    manager.oc(&[
        "admin",
        "policy",
        "set",
        "store",
        "consoleAdmin",
        &format!("user={}", manager.sec("console_user")?),
    ])?;
    bootstrap::complete(&marker)?;
    println!("Repository, signed hook, trusted host keys and storage identities initialized");
    Ok(())
}
pub fn acknowledge_closure(root: &Path, args: &[String]) -> Result<()> {
    super::ack::run(root, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn onboarding_retains_independent_hotlink_policy_and_live_inode() {
        let temp = tempfile::tempdir().unwrap();
        let generated = temp.path();
        fs::create_dir(generated.join(".onboarding")).unwrap();
        let raw = br#"{"hotlink_enabled":true,"allowed_referers":["example.com"],"qps":25}"#;
        fs::write(generated.join("public-access.json"), raw).unwrap();
        let inode = fs::metadata(generated.join("public-access.json"))
            .unwrap()
            .ino();
        let args = onboarding_render_args(generated, "/private/generated:/generated").unwrap();
        assert_eq!(
            &args[args.len() - 2..],
            ["--public-access-config", "/generated/public-access.json"]
        );
        let policy =
            crate::public_access::Policy::load(&generated.join("public-access.json")).unwrap();
        fs::write(
            generated.join(".onboarding/public-access.json"),
            serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();
        preserve_onboarding_policy(generated).unwrap();
        assert_eq!(fs::read(generated.join("public-access.json")).unwrap(), raw);
        assert_eq!(
            fs::metadata(generated.join("public-access.json"))
                .unwrap()
                .ino(),
            inode
        );
        assert_eq!(
            fs::read(generated.join(".onboarding/public-access.json")).unwrap(),
            raw
        );
        fs::write(generated.join(".onboarding/public-access.json"), b"{}").unwrap();
        assert!(preserve_onboarding_policy(generated).is_err());
        assert_eq!(fs::read(generated.join("public-access.json")).unwrap(), raw);
    }
    #[test]
    fn onboarding_legacy_policy_defaults_are_optional_but_candidate_is_required() {
        let temp = tempfile::tempdir().unwrap();
        let generated = temp.path();
        fs::create_dir(generated.join(".onboarding")).unwrap();
        assert!(!onboarding_render_args(generated, "x:/generated")
            .unwrap()
            .iter()
            .any(|s| s == "--public-access-config"));
        assert!(preserve_onboarding_policy(generated).is_err());
        fs::write(
            generated.join(".onboarding/public-access.json"),
            serde_json::to_vec(&crate::public_access::Policy::default()).unwrap(),
        )
        .unwrap();
        preserve_onboarding_policy(generated).unwrap();
        assert_eq!(
            crate::public_access::Policy::load(&generated.join("public-access.json")).unwrap(),
            crate::public_access::Policy::default()
        );
    }
    #[test]
    fn policy_extends_only_one_write_statement() {
        let p = json!({"Statement":[{"Effect":"Allow","Action":["s3:PutObject"],"Resource":["arn:existing"]}]});
        let r = extend_policy(&p, "attachments", "other").unwrap();
        assert_eq!(r["Statement"][0]["Resource"].as_array().unwrap().len(), 2);
        assert!(extend_policy(&json!({"Statement":[]}), "attachments", "other").is_err());
    }
    #[test]
    fn desired_binding_preserves_disabled_intent() {
        let c = json!({"repositories":[]});
        let mut r = desired_config(&c, "content/images", "main-images", "/", "main", 1).unwrap();
        r["repositories"][0]["enabled"] = json!(false);
        assert!(desired_config(&r, "content/images", "main-images", "/", "main", 1).is_err());
    }
}
