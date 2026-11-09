use super::telemetry::command;
use super::*;
fn eligible(inspections: &[Value], external: &str) -> Vec<Value> {
    inspections.iter().filter_map(|raw|{let labels=&raw["Config"]["Labels"];let project=labels["com.docker.compose.project"].as_str()?;let service=labels["com.docker.compose.service"].as_str()?;let own=project=="asset-stack"&&["webhook","publisher","info","object-reader","nginx","stargate","gitea","oc","otterio"].contains(&service);if !own&&!(project==external&&!external.is_empty()&&service=="traefik"){return None;}Some(json!({"id":raw["Id"],"service":service,"scope":format!("{project}/{service}"),"running":raw["State"]["Running"]==true,"paused":raw["State"]["Paused"]==true,"health":raw["State"]["Health"]["Status"]}))}).collect()
}
pub fn plan(rows: &[Value], state: &Value, at: f64) -> Result<(Value, Vec<Value>)> {
    let mut state = state.clone();
    if !state.is_object() {
        state = json!({});
    }
    if !state["unhealthy"].is_object() {
        state["unhealthy"] = json!({});
    }
    if !state["events"].is_array() {
        state["events"] = json!([]);
    }
    let recent: Vec<Value> = state["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "restart" && e["result"] != "skipped")
        .cloned()
        .collect();
    let mut actions = Vec::new();
    for row in rows {
        let scope = row["scope"].as_str().context("repair scope")?;
        if row["running"] != true || row["paused"] == true || row["health"] != "unhealthy" {
            state["unhealthy"].as_object_mut().unwrap().remove(scope);
            continue;
        }
        let old = &state["unhealthy"][scope];
        if old.is_null() || old["id"] != row["id"] {
            state["unhealthy"][scope] = json!({"id":row["id"],"since":at});
            continue;
        }
        let since = old["since"].as_f64().context("repair start")?;
        if !since.is_finite() || since > at || at - since < 30.0 {
            continue;
        }
        let own: Vec<_> = recent.iter().filter(|e| e["scope"] == scope).collect();
        if own
            .iter()
            .filter_map(|e| e["time"].as_f64())
            .any(|t| at - t < 600.0)
        {
            continue;
        }
        if own
            .iter()
            .filter_map(|e| e["time"].as_f64())
            .filter(|t| at - t < 3600.0)
            .count()
            >= 3
        {
            continue;
        }
        if recent
            .iter()
            .filter_map(|e| e["time"].as_f64())
            .filter(|t| at - t < 900.0)
            .count()
            + actions.len()
            >= 3
        {
            continue;
        }
        if actions.len() < 2 {
            actions.push(row.clone());
        }
    }
    Ok((state, actions))
}
pub fn run(args: &[String]) -> Result<()> {
    crate::ops::host_only()?;
    let root = crate::ops::protected_root(&root_arg(args))?;
    let env = environment(&root)?;
    let folder = PathBuf::from(env.get("DATA_ROOT").context("DATA_ROOT")?).join("repair");
    fs::create_dir_all(&folder)?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    let _lock = lock_file(&folder.join(".lock"), true)?;
    if root.join("private/maintenance.flag").exists() {
        println!("Health repair suppressed by maintenance flag");
        return Ok(());
    }
    let external = env.get("TRAEFIK_PROJECT").map(String::as_str).unwrap_or("");
    let mut ids: Vec<String> = command(
        "docker",
        &[
            "ps",
            "-aq",
            "--filter",
            "label=com.docker.compose.project=asset-stack",
        ],
        None,
        Duration::from_secs(35),
    )?
    .split_whitespace()
    .map(str::to_owned)
    .collect();
    if !external.is_empty() {
        ids.extend(
            command(
                "docker",
                &[
                    "ps",
                    "-aq",
                    "--filter",
                    &format!("label=com.docker.compose.project={external}"),
                    "--filter",
                    "label=com.docker.compose.service=traefik",
                ],
                None,
                Duration::from_secs(35),
            )?
            .split_whitespace()
            .map(str::to_owned),
        );
    }
    ids.sort();
    ids.dedup();
    let inspections: Vec<Value> = if ids.is_empty() {
        Vec::new()
    } else {
        let mut a = vec!["inspect"];
        a.extend(ids.iter().map(String::as_str));
        serde_json::from_str(&command("docker", &a, None, Duration::from_secs(35))?)?
    };
    let rows = eligible(&inspections, external);
    let path = folder.join("state.json");
    let state = read_json_bounded(&path, 1024 * 1024).unwrap_or(json!({}));
    let (mut state, actions) = plan(&rows, &state, now())?;
    let apply = args.iter().any(|a| a == "--apply");
    if apply {
        atomic(&path, &state)?;
        for row in &actions {
            if root.join("private/maintenance.flag").exists() {
                break;
            }
            let mut event = json!({"service":row["service"],"scope":row["scope"],"time":now(),"action":"restart","reason":"persistent-unhealthy","result":"attempt"});
            state["events"]
                .as_array_mut()
                .context("repair events")?
                .push(event.clone());
            if state["events"].as_array().unwrap().len() > 100 {
                state["events"].as_array_mut().unwrap().remove(0);
            }
            atomic(&path, &state)?;
            let result = (|| -> bool {
                let Some(id) = row["id"].as_str() else {
                    return false;
                };
                let Ok(s) = command("docker", &["inspect", id], None, Duration::from_secs(35))
                else {
                    return false;
                };
                let Ok(v) = serde_json::from_str::<Vec<Value>>(&s) else {
                    return false;
                };
                let current = eligible(&v, external);
                if current.first().is_none_or(|c| {
                    c["running"] != true || c["paused"] == true || c["health"] != "unhealthy"
                }) {
                    event["result"] = json!("skipped");
                    return true;
                }
                command("docker", &["restart", id], None, Duration::from_secs(35)).is_ok()
            })();
            if event["result"] != "skipped" {
                event["result"] = json!(if result { "ok" } else { "failed" });
            }
            let last = state["events"].as_array_mut().unwrap().last_mut().unwrap();
            *last = event;
            atomic(&path, &state)?;
        }
    }
    println!(
        "{}",
        json!({"checked":rows.len(),"apply":apply,"planned":actions.iter().map(|r|r["service"].clone()).collect::<Vec<_>>()})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_unhealthy_and_caps() {
        let rows:Vec<_>=(0..4).map(|i|json!({"id":format!("id{i}"),"scope":format!("asset-stack/s{i}"),"service":format!("s{i}"),"running":true,"paused":false,"health":"unhealthy"})).collect();
        let (s, a) = plan(&rows, &json!({}), 0.0).unwrap();
        assert!(a.is_empty());
        let (_, a) = plan(&rows, &s, 31.0).unwrap();
        assert_eq!(a.len(), 2);
        let (_, a) = plan(&rows, &s, 10.0).unwrap();
        assert!(a.is_empty());
    }
    #[test]
    fn excludes_other_projects() {
        let a = vec![
            json!({"Config":{"Labels":{"com.docker.compose.project":"other","com.docker.compose.service":"nginx"}},"State":{"Running":true}}),
        ];
        assert!(eligible(&a, "").is_empty());
    }
}
