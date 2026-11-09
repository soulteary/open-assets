//! Privileged host operations. Secrets stay in protected files; containers do not
//! gain Docker or administrative credentials.
use crate::{common, info};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};
mod ack;
mod admin;
mod diagnostics;
pub use ack::summary as acknowledgment_summary;
mod bootstrap;
pub use bootstrap::{build_domains, check_naming, render_env, verify_backup};
const URL_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
pub fn valid_sha(s: &str) -> bool {
    common::valid_sha(s)
}
pub fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
}
pub fn root_arg(args: &[String]) -> PathBuf {
    flag(args, "--root")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/app/open-assets"))
}
pub fn protected_root(root: &Path) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "absolute deployment root required");
    let canonical = root.canonicalize()?;
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "real deployment directory required"
    );
    Ok(canonical)
}
pub fn host_only() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "run on trusted host as root"
    );
    Ok(())
}
pub fn command(program: &str, args: &[&str], root: Option<&Path>) -> Result<String> {
    info::telemetry_command(program, args, root, Duration::from_secs(120))
}
pub fn private_write(path: &Path, data: &[u8], uid: Option<u32>) -> Result<()> {
    let parent = path.parent().context("private parent")?;
    fs::create_dir_all(parent)?;
    ensure!(
        fs::symlink_metadata(parent)?.is_dir(),
        "unsafe private directory"
    );
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(if uid.is_some() {
            0o640
        } else {
            0o600
        }))?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    if let Some(uid) = uid {
        chown(tmp.path(), uid, uid)?;
    }
    tmp.persist(path).map_err(|e| e.error)?;
    common::sync_directory(parent)
}
pub fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    ensure!(
        unsafe { libc::chown(path.as_ptr(), uid, gid) } == 0,
        "ownership update failed"
    );
    Ok(())
}
fn config(root: &Path) -> Result<Value> {
    common::load(Some(&root.join("private/repositories.yaml")))
}
pub fn repo_list(root: &Path) -> Result<Value> {
    let c = config(root)?;
    let rows:Vec<Value>=c["repositories"].as_array().context("repositories")?.iter().map(|r|json!({"key":r["key"],"name":r["name"].as_str().unwrap_or("—"),"branch":r["branch"],"mount_path":r["mount_path"],"enabled":r["enabled"]==true,"serve":r["serve"].as_bool().unwrap_or(true)})).collect();
    Ok(json!(rows))
}
fn hostname(s: &str) -> bool {
    s.len() <= 253
        && s.split('.').count() >= 2
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}
pub fn links(root: &Path, key: &str, sha: &str, name: &str) -> Result<Value> {
    ensure!(valid_sha(sha), "full commit required");
    let c = config(root)?;
    let r = c["repositories"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["key"] == key))
        .context("unknown repository")?;
    let prefix = format!(
        "{}/",
        r["source_directory"]
            .as_str()
            .context("source directory")?
            .trim_end_matches('/')
    );
    let name = name.strip_prefix(&prefix).unwrap_or(name);
    ensure!(
        common::relative(name)
            && name.split('/').all(|p| !p.starts_with('.'))
            && !name.chars().any(|c| c < ' ' || c == '\u{7f}'),
        "invalid public path"
    );
    let domains = info::read_json_bounded(&root.join("private/domains.json"), 16384)?;
    let oss = domains["oss"].as_str().context("object domain")?;
    let img = domains["img"].as_str().context("image domain")?;
    ensure!(hostname(oss) && hostname(img), "invalid configured domain");
    let bucket = c["bucket"].as_str().context("bucket")?;
    ensure!(
        (3..=63).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'),
        "bucket"
    );
    let encode = |s: &str| {
        s.split('/')
            .map(|p| percent_encoding::utf8_percent_encode(p, URL_ENCODE).to_string())
            .collect::<Vec<_>>()
            .join("/")
    };
    let image = if r["serve"].as_bool().unwrap_or(true) {
        name.strip_prefix("images/").map(|p| {
            format!(
                "https://{img}{}{}",
                r["mount_path"].as_str().unwrap_or("/"),
                encode(p)
            )
        })
    } else {
        None
    };
    Ok(
        json!({"key":key,"sha":sha,"verified":false,"attachment":format!("https://{oss}/{bucket}/repositories/{key}/releases/{sha}/{}",encode(name)),"image":image}),
    )
}
pub fn doctor(root: &Path) -> Value {
    diagnostics::doctor(root)
}

fn operator(args: &[String]) -> Result<()> {
    let root = protected_root(&root_arg(args))?;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--root" {
            ensure!(i + 1 < args.len(), "root argument");
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    let command = rest.first().context("operator command")?.as_str();
    let tail = &rest[1..];
    match command {
        "repo" => match tail.first().map(String::as_str) {
            Some("list") => {
                let result = repo_list(&root)?;
                if tail.iter().any(|a| a == "--json") {
                    println!("{}", json!({"repositories":result}));
                } else {
                    for r in result.as_array().unwrap() {
                        println!(
                            "{} {} 分支={} 图片前缀={} {} {}",
                            r["key"].as_str().unwrap_or(""),
                            r["name"].as_str().unwrap_or(""),
                            r["branch"].as_str().unwrap_or(""),
                            r["mount_path"].as_str().unwrap_or(""),
                            if r["enabled"] == true {
                                "启用"
                            } else {
                                "已禁用"
                            },
                            if r["serve"] == true {
                                "提供图片"
                            } else {
                                "不提供图片"
                            }
                        );
                    }
                }
                Ok(())
            }
            Some("ensure") => admin::ensure_repository(&root, &tail[1..]),
            _ => bail!("repository operation"),
        },
        "links" => {
            ensure!(tail.len() >= 3, "links requires key SHA path");
            let result = links(&root, &tail[0], &tail[1], &tail[2])?;
            if tail.iter().any(|a| a == "--json") {
                println!("{result}");
            } else {
                if let Some(image) = result["image"].as_str() {
                    println!("图片（当前版本）：{image}");
                }
                println!(
                    "附件（指定版本）：{}",
                    result["attachment"].as_str().unwrap_or("")
                );
                println!("仅按配置生成，未验证对象存在。图片固定链接可能已指向其他提交。");
            }
            Ok(())
        }
        "doctor" => {
            let report = doctor(&root);
            let ok = report["status"] == "ok";
            if tail.iter().any(|a| a == "--json") {
                println!("{report}");
            } else {
                for row in report["checks"].as_array().unwrap() {
                    println!(
                        "{} · {}：{}",
                        if row["status"] == "ok" {
                            "通过"
                        } else {
                            "待处理"
                        },
                        row["check"].as_str().unwrap_or(""),
                        row["hint"].as_str().unwrap_or("")
                    );
                }
                for row in report["timers"].as_array().unwrap() {
                    println!(
                        "  定时任务 {}：{}；{}",
                        row["timer"].as_str().unwrap_or(""),
                        row["reason"].as_str().unwrap_or(""),
                        row["hint"].as_str().unwrap_or("")
                    );
                }
                for row in report["caches"].as_array().unwrap() {
                    println!(
                        "  缓存 {}：{}；{}",
                        row["cache"].as_str().unwrap_or(""),
                        row["reason"].as_str().unwrap_or(""),
                        row["hint"].as_str().unwrap_or("")
                    );
                }
            }
            if ok {
                Ok(())
            } else {
                Err(common::ExitCode(1).into())
            }
        }
        "objects" => {
            ensure!(
                tail.first().map(String::as_str) == Some("setup"),
                "objects operation"
            );
            admin::objects_setup(&root, tail.iter().any(|s| s == "--compaction"))
        }
        "acknowledge-closure" => admin::acknowledge_closure(&root, tail),
        _ => {
            host_only()?;
            ensure!(
                [
                    "validate",
                    "status",
                    "publish",
                    "retry",
                    "rollback-images",
                    "restore-objects",
                    "check-restore",
                    "close-restore",
                    "restore-audit-list",
                    "restore-audit-archive",
                    "restore-audit-verify",
                    "compact-objects",
                    "deduplicate-local",
                    "materialize-image-views"
                ]
                .contains(&command),
                "unsupported operator command"
            );
            let env = info::environment(&root)?;
            ensure!(
                !root.join("private/maintenance.flag").exists(),
                "maintenance active"
            );
            let mut a = vec![
                "compose",
                "--project-directory",
                root.to_str().context("root")?,
                "exec",
                "-T",
                "publisher",
                "open-assets",
                command,
            ];
            a.extend(tail.iter().map(String::as_str));
            let _ = env;
            print!("{}", command_output(&a)?);
            Ok(())
        }
    }
}
fn command_output(args: &[&str]) -> Result<String> {
    command("docker", args, None)
}
pub fn run(command: &str, args: &[String], _c: &Value) -> Result<()> {
    match command {
        "operator" => operator(args),
        "deployment-config" => {
            let env = flag(args, "--env").context("--env required")?;
            let domains = flag(args, "--domains").context("--domains required")?;
            check_naming(Path::new(env), Path::new(domains))?;
            println!("Node naming matches all configured runtime domains; no files changed");
            Ok(())
        }
        "verify-backup" => {
            ensure!(args.len() == 2, "archive and new destination required");
            verify_backup(Path::new(&args[0]), Path::new(&args[1]))?;
            println!("Checksum and isolated extraction passed; complete application restore drill");
            Ok(())
        }
        "bootstrap-guard" => bootstrap::guard_command(args),
        "configure-edge" => bootstrap::configure_edge(args),
        "certs-maker" => {
            host_only()?;
            let root = protected_root(&root_arg(args))?;
            let env = info::environment(&root)?;
            println!(
                "{}",
                bootstrap::ensure_certs_maker(&root, &env, flag(args, "--archive").map(Path::new))?
                    .display()
            );
            Ok(())
        }
        "internal-certs" => bootstrap::internal_certs(args),
        "prepare-private" => bootstrap::prepare_private(args),
        "initialize-services" => admin::initialize_services(args),
        _ => bail!("unknown host command"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha_format() {
        assert!(valid_sha(&"a".repeat(40)));
        assert!(!valid_sha(&"0".repeat(40)));
        assert!(!valid_sha(&"A".repeat(40)));
    }
    #[test]
    fn hostname_scope() {
        assert!(hostname("img-node.example.com"));
        assert!(!hostname("https://example.com"));
        assert!(!hostname("example.com/path"));
    }
}
