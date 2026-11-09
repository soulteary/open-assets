use anyhow::{bail, ensure, Result};
use open_assets::{common, info, ops, publisher, storage};
use serde_json::{json, Value};
use std::path::Path;

const HELP: &str = "Open Assets — Rust implementation\n\nUsage: open-assets [--config FILE] COMMAND [ARGS]\n\nPublication: validate render serve worker once publish status retry rollback-images\nStorage: object-reader object-retention retention compact-objects deduplicate-local\n         materialize-image-views restore-objects check-restore close-restore\n         restore-audit-list restore-audit-archive restore-audit-verify\nMonitoring: info info-refresh telemetry host-history storage-scan object-info\n            resource-info audit-info doctor-info health repair benchmark\nAdministration: operator initialize-services prepare-private deployment-config\n                configure-edge certs-maker internal-certs bootstrap-guard verify-backup\n\nOPEN_ASSETS_CONFIG defaults to /config/repositories.yaml.\nProduction state formats and public URLs remain compatible with the Python version.\n";

fn render(c: &Value, args: &[String]) -> Result<()> {
    ensure!(args.len() == 1, "render requires a destination directory");
    let mut lines = vec![
        "include /etc/nginx/mime.types;".to_owned(),
        "default_type application/octet-stream;".to_owned(),
        "server { listen 8080; server_tokens off; autoindex off; open_file_cache off;".to_owned(),
        "add_header X-Content-Type-Options nosniff always;".to_owned(),
        format!(
            "add_header Content-Security-Policy \"{}\" always;",
            storage::CSP
        ),
        "add_header Cache-Control \"public, max-age=300, must-revalidate\";".to_owned(),
        "location ~ (^|/)\\. { deny all; }".to_owned(),
    ];
    let repositories = c["repositories"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid repositories"))?;
    for r in repositories {
        if r.get("serve").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let key = r["key"].as_str().unwrap_or("");
        let mount = r["mount_path"].as_str().unwrap_or("");
        let root = format!("/srv/images/repos/{key}/current/");
        if mount == "/" {
            lines.push(format!("location / {{ root {root}; limit_except GET HEAD {{ deny all; }} try_files $uri =404; }}"));
        } else {
            lines.push(format!(
                "location {mount} {{ alias {root}; limit_except GET HEAD {{ deny all; }} }}"
            ));
        }
    }
    if !repositories
        .iter()
        .any(|r| r["mount_path"] == "/" && r.get("serve").and_then(Value::as_bool) != Some(false))
    {
        lines.push("location / { return 404; }".into());
    }
    lines.push("}".into());
    let parent = Path::new(&args[0]);
    std::fs::create_dir_all(parent)?;
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    use std::os::unix::fs::PermissionsExt;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    tmp.write_all((lines.join("\n") + "\n").as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(parent.join("images.conf"))
        .map_err(|e| e.error)?;
    common::sync_directory(parent)
}

fn execute(command: &str, args: &[String], config: Option<&Path>) -> Result<()> {
    let monitoring = [
        "info",
        "info-refresh",
        "telemetry",
        "host-history",
        "storage-scan",
        "object-info",
        "resource-info",
        "audit-info",
        "doctor-info",
        "health",
        "repair",
        "benchmark",
    ];
    let admin = [
        "operator",
        "initialize-services",
        "prepare-private",
        "deployment-config",
        "configure-edge",
        "certs-maker",
        "internal-certs",
        "bootstrap-guard",
        "verify-backup",
    ];
    if monitoring.contains(&command) {
        return info::run(command, args, &Value::Null);
    }
    if admin.contains(&command) {
        return ops::run(command, args, &Value::Null);
    }
    let c = if command == "object-reader" {
        Value::Null
    } else {
        common::load(config)?
    };
    match command {
        "validate" => {
            ensure!(args.is_empty(), "validate takes no arguments");
            println!("configuration valid");
            Ok(())
        }
        "render" => render(&c, args),
        "serve" | "worker" | "once" | "publish" | "status" | "retry" | "rollback-images" => {
            publisher::run(command, args, &c)
        }
        "object-reader"
        | "object-retention"
        | "retention"
        | "compact-objects"
        | "materialize-image-views"
        | "deduplicate-local"
        | "restore-objects"
        | "check-restore"
        | "close-restore"
        | "restore-audit-list"
        | "restore-audit-archive"
        | "restore-audit-verify" => storage::run(command, args, &c),
        _ => bail!("unknown command"),
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        print!("{HELP}");
        return;
    }
    if args[0] == "--version" {
        println!("open-assets {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let config = if args.first().map(String::as_str) == Some("--config") {
        if args.len() < 3 {
            eprintln!("{}", json!({"status":"failed","error":"invalid_arguments"}));
            std::process::exit(2);
        }
        let p = std::path::PathBuf::from(args.remove(1));
        args.remove(0);
        Some(p)
    } else {
        None
    };
    let command = args.remove(0);
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP}");
        return;
    }
    if let Some(path) = &config {
        std::env::set_var("OPEN_ASSETS_CONFIG", path);
    }
    if let Err(error) = execute(&command, &args, config.as_deref()) {
        if let Some(code) = error.downcast_ref::<common::ExitCode>() {
            std::process::exit(code.0);
        }
        // Remote stderr, URLs and credentials must never enter public logs.
        eprintln!(
            "{}",
            json!({"status":"failed","command":command,"error":"operation_failed","hint":"Check configuration, permissions and protected diagnostics before retrying."})
        );
        std::process::exit(1);
    }
}
