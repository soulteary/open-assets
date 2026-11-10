use anyhow::{bail, ensure, Result};
use open_assets::{common, info, ops, publisher, storage};
use serde_json::{json, Value};
use std::path::Path;

const HELP: &str = "Open Assets — Rust implementation\n\nUsage: open-assets [--config FILE] COMMAND [ARGS]\n\nPublication: validate render serve worker once publish status retry rollback-images\nStorage: object-reader object-retention retention compact-objects deduplicate-local\n         materialize-image-views restore-objects check-restore close-restore\n         restore-audit-list restore-audit-archive restore-audit-verify\nMonitoring: info info-refresh telemetry host-history storage-scan object-info\n            resource-info audit-info doctor-info health repair benchmark\nAdministration: operator initialize-services prepare-private deployment-config\n                configure-edge certs-maker internal-certs bootstrap-guard verify-backup\n\nOPEN_ASSETS_CONFIG defaults to /config/repositories.yaml.\nProduction state formats and public URLs remain compatible with the Python version.\n";

fn render(c: &Value, args: &[String]) -> Result<()> {
    let mut config = c.clone();
    let mut destination = None;
    let mut policy_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--public-access-config" => {
                ensure!(
                    policy_path.is_none(),
                    "duplicate public access configuration"
                );
                i += 1;
                policy_path = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("missing public access configuration"))?,
                );
            }
            value => {
                ensure!(
                    !value.starts_with('-') && destination.is_none(),
                    "render requires one destination"
                );
                destination = Some(value);
            }
        }
        i += 1;
    }
    if let Some(path) = policy_path {
        config["public_access"] = common::read_json(Path::new(path), 65_536)?;
    }
    publisher::render(
        &config,
        Path::new(destination.ok_or_else(|| anyhow::anyhow!("render destination required"))?),
    )
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
