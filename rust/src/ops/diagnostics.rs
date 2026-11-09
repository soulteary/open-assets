use super::*;
const SERVICES: [&str; 8] = [
    "webhook",
    "publisher",
    "info",
    "nginx",
    "otterio",
    "oc",
    "stargate",
    "gitea",
];
const TIMERS: [(&str, bool, f64); 8] = [
    ("info", true, 120.0),
    ("objects", true, 2100.0),
    ("audits", true, 2100.0),
    ("resources", true, 180.0),
    ("logrotate", true, 7800.0),
    ("repair", false, 180.0),
    ("retention", false, 46800.0),
    ("object-retention", false, 46800.0),
];
const PROPERTIES:&str="Id,LoadState,ActiveState,UnitFileState,Result,ExecMainCode,ExecMainStatus,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic,ConditionResult,ConditionTimestampMonotonic";
fn timer_parse(name: &str, required: bool, max_age: f64, raw: &str, uptime: f64) -> Result<Value> {
    let blocks: BTreeMap<&str, BTreeMap<&str, &str>> = raw
        .trim()
        .split("\n\n")
        .filter_map(|b| {
            let values: BTreeMap<&str, &str> =
                b.lines().filter_map(|l| l.split_once('=')).collect();
            values.get("Id").copied().map(|id| (id, values))
        })
        .collect();
    let timer = format!("open-assets-{name}.timer");
    let unit = format!("open-assets-{name}.service");
    let schedule = blocks.get(timer.as_str()).context("timer properties")?;
    let service = blocks.get(unit.as_str()).context("service properties")?;
    let get = |k: &str| service.get(k).copied().unwrap_or("");
    let integer = |k: &str| -> Result<f64> { Ok(get(k).parse::<u64>()? as f64) };
    let active = schedule
        .get("ActiveState")
        .copied()
        .context("timer state")?;
    let mut row = json!({"timer":name,"required":required,"max_age_seconds":max_age,"status":"ok","reason":"ok","active":active,"enabled":schedule.get("UnitFileState").copied().unwrap_or("unknown"),"hint":"Scheduling enabled; most recent run succeeded"});
    let disabled = schedule.get("LoadState") == Some(&"not-found") || active != "active";
    let failed = !matches!(get("Result"), "" | "success")
        || get("ActiveState") == "failed"
        || integer("ExecMainStatus")? != 0.0
        || !matches!(integer("ExecMainCode")? as u64, 0 | 1);
    if disabled {
        row["status"] = json!(if required { "attention" } else { "optional" });
        row["reason"] = json!("not-enabled");
        row["hint"] = json!(if required {
            "Install and enable the required timer after inspecting service prerequisites"
        } else {
            "Optional operation disabled; do not enable destructive maintenance merely to pass diagnostics"
        });
        if failed {
            row["status"] = json!("attention");
            row["reason"] = json!("failed");
        } else {
            return Ok(row);
        }
    } else {
        ensure!(get("LoadState") == "loaded", "service not loaded");
    }
    let started = integer("ExecMainStartTimestampMonotonic")? / 1e6;
    let finished = integer("ExecMainExitTimestampMonotonic")? / 1e6;
    row["result"] = json!(if get("Result").is_empty() {
        "unknown"
    } else {
        get("Result")
    });
    row["exit_code"] = json!(integer("ExecMainStatus")? as u64);
    if matches!(get("ActiveState"), "activating" | "active" | "deactivating") {
        row["status"] = json!("ok");
        row["reason"] = json!("running");
        row["hint"] = json!("Service is executing; inspect diagnostics again after completion");
        return Ok(row);
    }
    if failed {
        row["status"] = json!("attention");
        row["reason"] = json!("failed");
    } else if get("ConditionResult") == "no" && integer("ConditionTimestampMonotonic")? > 0.0 {
        row["status"] = json!("attention");
        row["reason"] = json!("condition-skipped");
    } else if started == 0.0 || finished == 0.0 || finished < started || finished > uptime + 5.0 {
        row["status"] = json!("attention");
        row["reason"] = json!("never-or-unknown");
    } else {
        let age = (uptime - finished).round().max(0.0);
        row["age_seconds"] = json!(age);
        if age > max_age {
            row["status"] = json!("attention");
            row["reason"] = json!("overdue");
        }
    }
    if row["status"] == "attention" {
        row["hint"]=json!("Inspect the service journal, maintenance marker and policy before restarting; cleanup is never run by diagnostics");
    }
    Ok(row)
}
fn timer(name: &str, required: bool, max_age: f64, uptime: f64) -> Value {
    let timer = format!("open-assets-{name}.timer");
    let service = format!("open-assets-{name}.service");
    let p = format!("--property={PROPERTIES}");
    info::telemetry_command("systemctl",&["show",&timer,&service,&p],None,Duration::from_secs(20)).and_then(|s|timer_parse(name,required,max_age,&s,uptime)).unwrap_or(json!({"timer":name,"required":required,"max_age_seconds":max_age,"status":"attention","reason":"unavailable","hint":"Inspect systemctl status for the named timer and service; verify read permissions"}))
}
fn caches(root: &Path, data: &Path, c: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    let keys: Vec<String> = c["repositories"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r["key"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut sorted = keys.clone();
    sorted.sort();
    let resources_scope = common::limits(c)
        .and_then(|p| common::digest(&json!([sorted, p])))
        .unwrap_or_default();
    let scopes = [
        (
            "local-versions",
            "repositories-cache.json",
            hex::encode(Sha256::digest(
                common::read_regular(&root.join("private/repositories.yaml"), 1024 * 1024)
                    .unwrap_or_default(),
            )),
            300.0,
        ),
        (
            "objects",
            "objects-cache.json",
            common::digest(&json!([c["endpoint"], c["bucket"], sorted])).unwrap_or_default(),
            900.0,
        ),
        (
            "audits",
            "audit-cache.json",
            common::digest(&json!(["audit-cache-v1", sorted])).unwrap_or_default(),
            900.0,
        ),
        ("resources", "resources-cache.json", resources_scope, 900.0),
    ];
    for (name, file, scope, interval) in scopes {
        let summary = (|| -> Result<Vec<Value>> {
            let cache = info::read_json_bounded(&data.join("info").join(file), 4 * 1024 * 1024)?;
            ensure!(cache["schema_version"] == 1, "unsupported cache schema");
            ensure!(
                if name == "local-versions" {
                    cache["config_digest"] == scope
                } else {
                    cache["scope"] == scope
                },
                "cache scope changed"
            );
            let mut samples = Vec::new();
            if name == "resources" {
                let mut sample = cache["sample"].clone();
                ensure!(sample.is_object(), "missing resource sample");
                sample["status"] = cache["status"].clone();
                samples.push(sample);
            } else {
                let repos = cache["repositories"]
                    .as_object()
                    .context("cache repositories")?;
                ensure!(
                    repos.len() == keys.len() && keys.iter().all(|k| repos.contains_key(k)),
                    "cache repository scope changed"
                );
                for key in &keys {
                    let r = &repos[key];
                    let mut sample = match name {
                        "objects" => r["complete"].clone(),
                        "audits" => r["sample"].clone(),
                        _ => r.clone(),
                    };
                    if !sample.is_object() {
                        sample = json!({});
                    }
                    sample["key"] = json!(key);
                    sample["status"] = if name == "local-versions" {
                        json!(if cache["failures"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|k| k == key))
                        {
                            "unavailable"
                        } else {
                            "ok"
                        })
                    } else {
                        r["status"].clone()
                    };
                    samples.push(sample);
                }
            }
            Ok(samples)
        })();
        match summary{Err(_)=>rows.push(json!({"cache":name,"status":"attention","reason":"unavailable-or-scope-changed","hint":"Inspect the collector journal and configured scope; no directory scan or S3 request was performed"})),Ok(samples)=>for sample in samples{let at=sample["measured_at"].as_f64();let mut reason=if !at.is_some_and(|t|t.is_finite()&&t>0.0){"never-or-invalid"}else if at.unwrap()>common::now()+5.0{"clock-skew"}else if common::now()-at.unwrap()>interval*2.0{"stale"}else{"fresh"};if sample["status"]!="ok"{reason=match sample["status"].as_str(){Some("busy")=>"collector-busy",Some("scanning")=>"collector-scanning",Some("never")=>"collector-never",_=>"collector-unavailable"};}rows.push(json!({"cache":name,"key":sample["key"],"status":if reason=="fresh"{"ok"}else{"attention"},"reason":reason,"age_seconds":at.filter(|t|t.is_finite()&&*t>=0.0&&*t<=common::now()+5.0).map(|t|(common::now()-t).round().max(0.0)),"max_age_seconds":interval*2.0,"hint":if reason=="fresh"{"Sample remains within its freshness window"}else{"Inspect the corresponding collector journal; diagnostics do not run cleanup or modify caches"}}));}}
    }
    rows
}
pub fn doctor(root: &Path) -> Value {
    let mut checks = Vec::new();
    let c = config(root);
    checks.push(json!({"check":"configuration","status":if c.is_ok(){"ok"}else{"attention"},"hint":if c.is_ok(){"Configuration structure and policy validated"}else{"Inspect private/repositories.yaml locally; do not repeat initialization"}}));
    if root.join("private/maintenance.flag").exists() {
        checks.push(json!({"check":"maintenance","status":"attention","hint":"Maintenance marker present; verify the operation before removing it"}));
    }
    let mut components = Vec::new();
    let output = info::telemetry_command(
        "docker",
        &[
            "compose",
            "--project-directory",
            root.to_str().unwrap_or("/app/open-assets"),
            "ps",
            "--all",
            "--format",
            "json",
        ],
        Some(root),
        Duration::from_secs(20),
    );
    if let Ok(raw) = output {
        let parsed: Vec<Value> = serde_json::from_str(&raw).unwrap_or_else(|_| {
            raw.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        });
        for service in SERVICES {
            let matches: Vec<_> = parsed.iter().filter(|r| r["Service"] == service).collect();
            if matches.len() != 1 {
                components.push(json!({"service":service,"state":"missing","health":"unknown"}));
                continue;
            }
            let v = matches[0];
            components.push(json!({"service":service,"state":v["State"].as_str().unwrap_or("unknown").chars().take(32).collect::<String>(),"health":v["Health"].as_str().unwrap_or("unknown").chars().take(32).collect::<String>()}));
        }
    }
    let containers_ok = components.len() == SERVICES.len()
        && components
            .iter()
            .all(|r| r["state"] == "running" && r["health"] == "healthy");
    checks.push(json!({"check":"containers","status":if containers_ok{"ok"}else{"attention"},"hint":"Only this project's eight original containers are checked here; inspect external Traefik separately"}));
    let uptime = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    let timers: Vec<_> = TIMERS
        .into_iter()
        .map(|(n, r, m)| timer(n, r, m, uptime))
        .collect();
    checks.push(json!({"check":"timers","status":if timers.iter().any(|t|t["status"]=="attention"){"attention"}else{"ok"},"hint":"Required collection and logging timers and optional maintenance timers checked independently"}));
    let env = info::environment(root);
    let data = env
        .as_ref()
        .ok()
        .and_then(|e| e.get("DATA_ROOT"))
        .map(PathBuf::from);
    let snapshot = data
        .as_ref()
        .and_then(|p| info::read_snapshot(&p.join("info/snapshot.json")).ok());
    let stale = snapshot.as_ref().is_none_or(|v| {
        v["stale"] == true || v["collected_at"].as_f64().unwrap_or(0.0) > common::now() + 5.0
    });
    checks.push(json!({"check":"info-snapshot","status":if stale{"attention"}else{"ok"},"hint":"Inspect host snapshot freshness separately from lower frequency storage and audit samples"}));
    let caches = match (c, data) {
        (Ok(c), Some(data)) => caches(root, &data, &c),
        _ => Vec::new(),
    };
    checks.push(json!({"check":"caches","status":if !caches.is_empty()&&caches.iter().all(|r|r["status"]=="ok"){"ok"}else{"attention"},"hint":"Local versions, object, audit, and resource caches read without scanning or accessing storage"}));
    let status = if checks.iter().all(|c| c["status"] == "ok") {
        "ok"
    } else {
        "attention"
    };
    json!({"status":status,"checks":checks,"components":components,"timers":timers,"caches":caches})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_optional_timer_does_not_hide_failure() {
        let text="Id=open-assets-repair.timer\nLoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n\nId=open-assets-repair.service\nLoadState=loaded\nActiveState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=1\nExecMainStartTimestampMonotonic=1000000\nExecMainExitTimestampMonotonic=2000000\nConditionResult=yes\nConditionTimestampMonotonic=0\n";
        let r = timer_parse("repair", false, 180.0, text, 10.0).unwrap();
        assert_eq!(r["status"], "attention");
        assert_eq!(r["reason"], "failed");
    }
    #[test]
    fn successful_timer_can_be_overdue() {
        let text="Id=open-assets-info.timer\nLoadState=loaded\nActiveState=active\nUnitFileState=enabled\n\nId=open-assets-info.service\nLoadState=loaded\nActiveState=inactive\nResult=success\nExecMainCode=1\nExecMainStatus=0\nExecMainStartTimestampMonotonic=1000000\nExecMainExitTimestampMonotonic=2000000\nConditionResult=yes\nConditionTimestampMonotonic=0\n";
        let r = timer_parse("info", true, 120.0, text, 300.0).unwrap();
        assert_eq!(r["reason"], "overdue");
    }
}
