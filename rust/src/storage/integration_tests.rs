use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    sync::{Arc, Mutex},
    time::SystemTime,
};
use tiny_http::{Header, Response as TestResponse, Server};
fn headers() -> Vec<Header> {
    [
        ("ETag", "\"321c3cf486ed509164edec1e1981fec8\""),
        ("Last-Modified", "Thu, 01 Jan 2026 00:00:00 GMT"),
        ("Content-Type", "image/png"),
        ("Cache-Control", CACHE_CONTROL),
        (
            "x-amz-meta-sha256",
            "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5",
        ),
    ]
    .into_iter()
    .map(|(k, v)| Header::from_bytes(k, v).unwrap())
    .collect()
}
fn mock_client(server: &Server, root: &Path) -> Client {
    let access = root.join("access");
    let secret = root.join("secret");
    fs::write(&access, "fixture-access").unwrap();
    fs::write(&secret, "fixture-secret").unwrap();
    Client {
        http: http_client(None, 5).unwrap(),
        endpoint: Url::parse(&format!("http://{}", server.server_addr().to_ip().unwrap())).unwrap(),
        bucket: "attachments".into(),
        access_path: access,
        secret_path: secret,
    }
}
#[test]
fn stream_upload_download_and_encoded_key_use_real_http() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let client = mock_client(&server, tmp.path());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observations = Arc::clone(&observed);
    let worker = std::thread::spawn(move || {
        for _ in 0..4 {
            let mut request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let url = request.url().to_owned();
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .to_string();
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            observations
                .lock()
                .unwrap()
                .push((request.method().to_string(), url, body, auth));
            let response = if request.method() == &tiny_http::Method::Get {
                TestResponse::from_data(b"payload".to_vec())
            } else {
                TestResponse::from_data(Vec::new())
            };
            let mut response = response;
            for h in headers() {
                response.add_header(h);
            }
            if request.method() == &tiny_http::Method::Head {
                response.add_header(Header::from_bytes("Content-Length", "7").unwrap());
            }
            request.respond(response).unwrap();
        }
    });
    let key =
        "repositories/images/releases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/images/a+b%25.png";
    let source = tmp.path().join("source");
    fs::write(&source, b"payload").unwrap();
    let digest = hash(b"payload");
    client
        .put_file(
            key,
            &source,
            &json!({"sha256":digest,"content_type":"image/png","cache_control":CACHE_CONTROL}),
        )
        .unwrap();
    fs::write(&client.access_path, "rotated-access").unwrap();
    let destination = tmp.path().join("download");
    let meta = client.get_to_file(key, &destination, 7).unwrap();
    assert_eq!(meta["sha256"], digest);
    assert_eq!(fs::read(destination).unwrap(), b"payload");
    let _head = client.head(key).unwrap().unwrap();
    let verified = client
        .verify_object(key, &json!({"size":7,"sha256":digest}))
        .unwrap();
    assert_eq!(verified["sha256"], digest);
    assert_eq!(
        verified["body_etag"],
        "\"321c3cf486ed509164edec1e1981fec8\""
    );
    worker.join().unwrap();
    let observed = observed.lock().unwrap();
    assert_eq!(observed[0].0, "PUT");
    assert!(observed[0].1.ends_with("/a%2Bb%2525.png"));
    assert_eq!(observed[0].2, b"payload");
    assert!(observed[0].3.contains("Credential=fixture-access/"));
    assert!(observed[1].3.contains("Credential=rotated-access/"));
}
#[test]
fn remote_overlength_is_refused_without_partial_file() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::http("127.0.0.1:0").unwrap();
    let client = mock_client(&server, tmp.path());
    let t = std::thread::spawn(move || {
        let r = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut response = TestResponse::from_data(b"payload".to_vec());
        for h in headers() {
            response.add_header(h);
        }
        r.respond(response).unwrap();
    });
    let p = tmp.path().join("too-large");
    assert!(client.get_to_file("object", &p, 3).is_err());
    assert!(!p.exists());
    t.join().unwrap();
}
#[test]
fn precise_object_time_preserves_old_deletion_receipts() {
    let xml=b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>p/a</Key><Size>7</Size><ETag>&quot;etag&quot;</ETag><LastModified>2026-01-01T00:00:00.125Z</LastModified></Contents></ListBucketResult>";
    let p = parse_listing(xml, "p/", 1000).unwrap();
    assert_eq!(p["Contents"][0]["LastModified"], 1767225600i64);
    assert_eq!(p["Contents"][0]["LastModifiedPrecise"], 1767225600.125);
}
#[test]
fn fresh_sha_metadata_overrides_obsolete_source_metadata() {
    let h = metadata_headers(&json!({"sha256":"new","metadata":{"sha256":"old","custom":"value"}}))
        .unwrap();
    assert_eq!(h["x-amz-meta-sha256"], "new");
    assert_eq!(h["x-amz-meta-custom"], "value");
}
fn run_when_idle(command: &str, args: &[String], c: &Value) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let report = maintenance::run(command, args, c).unwrap();
        if report["status"] != "busy" {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "test fixture remained busy: {report}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn fixture() -> (tempfile::TempDir, Value) {
    let tmp = tempfile::tempdir().unwrap();
    let c = json!({"state_root":tmp.path().join("state"),"images_root":tmp.path().join("images"),"revision":"fixture","repositories":[{"key":"images","enabled":true,"retention":{"enabled":true,"keep_last":1,"keep_days":0,"min_age_hours":1,"pinned_shas":["b".repeat(40)]}}]});
    fs::create_dir_all(tmp.path().join("state/jobs")).unwrap();
    (tmp, c)
}
fn version(c: &Value, sha: &str) -> Value {
    let base = PathBuf::from(text(c, "images_root").unwrap())
        .join("repos/images/releases")
        .join(sha);
    fs::create_dir_all(base.join("images")).unwrap();
    let p = base.join("images/a.png");
    fs::write(&p, b"payload").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
    let m = json!({"key":"images","sha":sha,"image_layout":"snapshot-v1","files":[{"path":"images/a.png","size":7,"sha256":hash(b"payload"),"content_type":"image/png"}]});
    let manifest = PathBuf::from(text(c, "state_root").unwrap())
        .join("manifests/images")
        .join(format!("{sha}.json"));
    crate::common::atomic(&manifest, &m).unwrap();
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for p in [p, base.join("images"), base, manifest] {
        File::open(&p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    m
}
#[test]
fn local_cleanup_preview_and_apply_preserve_current_pins_and_manifests() {
    let (_tmp, c) = fixture();
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let s = "c".repeat(40);
    version(&c, &a);
    version(&c, &b);
    let m = version(&c, &s);
    let base = maintenance::image_base(&c, "images").unwrap();
    std::os::unix::fs::symlink(format!("releases/{s}/images"), base.join("current")).unwrap();
    crate::common::atomic(
        &maintenance::state(&c).unwrap().join("published/images"),
        &m,
    )
    .unwrap();
    let preview = run_when_idle("retention", &[], &c);
    assert_eq!(preview["repositories"][0]["candidates"][0]["sha"], a);
    assert!(base.join("releases").join(&a).exists());
    let applied = run_when_idle("retention", &["--apply".into()], &c);
    assert_eq!(
        applied["repositories"][0]["trash_removed"], 1,
        "unexpected retention result: {applied}"
    );
    assert!(!base.join("releases").join(&a).exists());
    assert!(base.join("releases").join(&b).exists());
    assert!(base.join("releases").join(&s).exists());
    assert!(maintenance::state(&c)
        .unwrap()
        .join("manifests/images")
        .join(format!("{a}.json"))
        .exists());
}
#[test]
fn local_dedup_shares_one_readonly_inode_and_legacy_materialization_reuses_bytes() {
    let (_tmp, c) = fixture();
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    version(&c, &a);
    version(&c, &b);
    let base = maintenance::image_base(&c, "images").unwrap();
    let one = base.join("releases").join(&a).join("images/a.png");
    let two = base.join("releases").join(&b).join("images/a.png");
    assert_ne!(
        fs::metadata(&one).unwrap().ino(),
        fs::metadata(&two).unwrap().ino()
    );
    let preview = run_when_idle("deduplicate-local", &[], &c);
    assert_eq!(preview["reclaimable_bytes"], 7);
    let applied = run_when_idle("deduplicate-local", &["--apply".into()], &c);
    assert_eq!(applied["replaced"], 1);
    assert_eq!(
        fs::metadata(&one).unwrap().ino(),
        fs::metadata(&two).unwrap().ino()
    );
    run_when_idle("materialize-image-views", &["--apply".into()], &c);
    let legacy = base
        .join("releases")
        .join(format!("images-{a}"))
        .join("a.png");
    assert_eq!(
        fs::metadata(&one).unwrap().ino(),
        fs::metadata(&legacy).unwrap().ino()
    );
    assert_eq!(fs::metadata(one).unwrap().mode() & 0o222, 0);
}
#[test]
fn linked_manifest_is_rejected_and_foreign_reference_cannot_escape_scope() {
    let (_tmp, c) = fixture();
    let sha = "a".repeat(40);
    let mut m = version(&c, &sha);
    m["object_layout"] = json!("references-v1");
    m["object_sources"] = json!({"images/a.png":{"key":format!("repositories/foreign/releases/{sha}/images/a.png"),"content_type":"image/png"}});
    assert!(validate_manifest(&m).is_err());
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    fs::write(&source, b"{}").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(source, &link).unwrap();
    assert!(read_json(&link).is_err());
}

#[test]
fn queue_lock_contention_reports_busy_and_leaves_all_release_paths_intact() {
    let (_tmp, c) = fixture();
    let sha = "a".repeat(40);
    version(&c, &sha);
    let base = maintenance::image_base(&c, "images").unwrap();
    let queue = maintenance::lock(&c, "queue").unwrap();
    let busy = maintenance::run("retention", &["--apply".into()], &c).unwrap();
    assert_eq!(busy["status"], "busy");
    assert!(base
        .join("releases")
        .join(&sha)
        .join("images/a.png")
        .exists());
    assert!(!base.join(".retention-trash").exists());
    drop(queue);
    assert_eq!(run_when_idle("retention", &[], &c)["status"], "complete");
}

#[test]
fn dangling_safety_records_refuse_cleanup_and_restoration_barrier_checks() {
    let (_tmp, c) = fixture();
    version(&c, &"a".repeat(40));
    let root = maintenance::state(&c).unwrap();
    let history = root.join("rollback-protection/images.json");
    fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(root.join("missing-history"), &history).unwrap();
    assert!(maintenance::history(&c, "images").is_err());
    let sha = "a".repeat(40);
    let receipt = root
        .join("object-retention/images")
        .join(format!("{sha}.json"));
    fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(root.join("missing-receipt"), &receipt).unwrap();
    assert!(maintenance::barrier(&c, "images", &sha).is_err());
    assert!(maintenance::run("retention", &["--apply".into()], &c).is_err());
    assert!(maintenance::image_base(&c, "images")
        .unwrap()
        .join("releases")
        .join(&sha)
        .join("images/a.png")
        .exists());
}

fn assert_bad_job_refuses_cleanup(oversized: bool) {
    let (_temp, c) = fixture();
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let current = "c".repeat(40);
    version(&c, &a);
    version(&c, &b);
    let manifest = version(&c, &current);
    let base = maintenance::image_base(&c, "images").unwrap();
    std::os::unix::fs::symlink(format!("releases/{current}/images"), base.join("current")).unwrap();
    crate::common::atomic(
        &maintenance::state(&c).unwrap().join("published/images"),
        &manifest,
    )
    .unwrap();
    assert_eq!(
        run_when_idle("retention", &[], &c)["repositories"][0]["candidates"][0]["sha"],
        a
    );
    let id = if oversized {
        crate::publisher::queue::identity("images", &a, "auto")
    } else {
        "d".repeat(64)
    };
    let mut job = json!({"id":id,"key":"images","sha":a,"kind":"auto","status":"published","created":1700000000.,"finished":1700000001.,"config":"fixture","delivery":"fixture"});
    if oversized {
        job["padding"] = json!("x".repeat(65536));
    }
    let path = maintenance::state(&c)
        .unwrap()
        .join("jobs")
        .join(format!("{id}.json"));
    crate::common::atomic(&path, &job).unwrap();
    assert!(maintenance::jobs(&c).is_err());
    let result = maintenance::run("retention", &["--apply".into()], &c);
    assert!(
        result.is_err(),
        "invalid task authorized cleanup: {result:?}"
    );
    assert!(base.join("releases").join(&a).join("images/a.png").exists());
    assert!(base.join("releases").join(&b).join("images/a.png").exists());
    assert!(base
        .join("releases")
        .join(&current)
        .join("images/a.png")
        .exists());
    assert!(!base.join(".retention-trash").exists());
    assert!(path.exists());
}
#[test]
fn forged_job_identity_refuses_cleanup_without_deleting_eligible_release() {
    assert_bad_job_refuses_cleanup(false);
}
#[test]
fn oversized_job_refuses_cleanup_without_deleting_eligible_release() {
    assert_bad_job_refuses_cleanup(true);
}

fn large_reference_manifest() -> Value {
    let mut files = Vec::with_capacity(3583);
    let mut sources = serde_json::Map::new();
    for index in 0..3583 {
        let name = format!("images/2026/11/10/fixture-{index:05}.png");
        files.push(json!({"path":name,"size":index+1,"sha256":format!("{index:064x}"),"content_type":"image/png"}));
        if index < 3580 {
            sources.insert(name.clone(),json!({"key":format!("repositories/images/releases/{}/{}","b".repeat(40),name),"content_type":"image/jpeg"}));
        }
    }
    json!({"key":"images","sha":"a".repeat(40),"object_layout":"references-v1","files":files,"object_sources":sources})
}
#[test]
fn large_reference_manifest_resolves_all_entries_and_keeps_owned_results() {
    let manifest = large_reference_manifest();
    let original = manifest.clone();
    let resolved = validate_manifest(&manifest).unwrap();
    assert_eq!(resolved.len(), 3583);
    for (index, item) in manifest["files"].as_array().unwrap().iter().enumerate() {
        let name = item["path"].as_str().unwrap();
        if index < 3580 {
            assert_eq!(resolved[name], manifest["object_sources"][name]);
            assert_eq!(resolved[name]["content_type"], "image/jpeg");
        } else {
            assert_eq!(
                resolved[name],
                json!({"key":format!("repositories/images/releases/{}/{}","a".repeat(40),name),"content_type":"image/png"})
            );
        }
    }
    let item = &manifest["files"][3579];
    let name = item["path"].as_str().unwrap();
    let mut selected = source(&manifest, item).unwrap();
    selected["key"] = json!("only the owned result changed");
    assert_eq!(manifest, original);
    assert_ne!(selected, manifest["object_sources"][name]);
}
#[test]
fn large_reference_manifest_still_validates_late_records_and_unknown_entries() {
    let original = large_reference_manifest();
    let name = original["files"][3579]["path"].as_str().unwrap().to_owned();
    for replacement in [
        json!({"key":format!("repositories/foreign/releases/{}/{}","b".repeat(40),name),"content_type":"image/jpeg"}),
        json!({"key":format!("repositories/images/releases/{}/{}","b".repeat(39),name),"content_type":"image/jpeg"}),
        json!({"key":format!("repositories/images/releases/{}/images/../escape.png","b".repeat(40)),"content_type":"image/jpeg"}),
        json!({"key":format!("repositories/images/releases/{}/{}","b".repeat(40),name),"content_type":"image/jpeg\r\ninjected: value"}),
        json!({"key":format!("repositories/images/releases/{}/{}","b".repeat(40),name),"content_type":"image/jpeg","unexpected":true}),
        Value::Null,
    ] {
        let mut manifest = original.clone();
        manifest["object_sources"][&name] = replacement;
        assert!(validate_manifest(&manifest).is_err());
    }
    let mut unknown = original.clone();
    unknown["object_sources"]["images/not-in-manifest.png"] =
        original["object_sources"][&name].clone();
    assert!(validate_manifest(&unknown).is_err());
    let mut layout = original;
    layout["object_layout"] = json!("unknown");
    assert!(validate_manifest(&layout).is_err());
}
#[test]
fn missing_reference_map_still_defaults_but_present_non_object_maps_are_rejected() {
    let manifest = large_reference_manifest();
    let item = &manifest["files"][3582];
    let mut missing = manifest.clone();
    missing.as_object_mut().unwrap().remove("object_sources");
    missing.as_object_mut().unwrap().remove("object_layout");
    let expected = json!({"key":format!("repositories/images/releases/{}/{}","a".repeat(40),item["path"].as_str().unwrap()),"content_type":"image/png"});
    assert_eq!(source(&missing, item).unwrap(), expected);
    missing["object_sources"] = json!({});
    assert_eq!(source(&missing, item).unwrap(), expected);
    for invalid in [Value::Null, json!([]), json!("not a map"), json!(false)] {
        missing["object_sources"] = invalid;
        assert!(source(&missing, item).is_err());
    }
}

#[test]
fn retention_tree_stats_preserves_logical_size_and_subsecond_file_directory_and_root_mtimes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let nested = root.join("nested/deep");
    fs::create_dir_all(&nested).unwrap();
    let first = root.join("first.bin");
    let second = nested.join("second.bin");
    let alias = root.join("first-alias.bin");
    fs::write(&first, b"abc").unwrap();
    fs::write(&second, b"12345").unwrap();
    fs::hard_link(&first, &alias).unwrap();
    let set_time = |path: &Path, nanos: u64| {
        let stamp = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_700_000_000)
            + Duration::from_nanos(nanos);
        File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stamp))
            .unwrap();
    };
    set_time(&root, 100_000_000);
    set_time(&root.join("nested"), 2_222_222_222);
    set_time(&first, 4_444_444_444);
    set_time(&second, 3_333_333_333);
    set_time(&nested, 5_555_555_555);
    let expected = |path: &Path| {
        fs::symlink_metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    };
    let measured =
        maintenance::tree_stats(&root, std::time::Instant::now() + Duration::from_secs(5)).unwrap();
    assert_eq!(measured, (11, expected(&nested)));
    assert_eq!(
        fs::metadata(&first).unwrap().ino(),
        fs::metadata(&alias).unwrap().ino()
    );
    set_time(&second, 6_666_666_666);
    assert_eq!(
        maintenance::tree_stats(&root, std::time::Instant::now() + Duration::from_secs(5)).unwrap(),
        (11, expected(&second))
    );
    set_time(&root, 7_777_777_777);
    assert_eq!(
        maintenance::tree_stats(&root, std::time::Instant::now() + Duration::from_secs(5)).unwrap(),
        (11, expected(&root))
    );
    let empty = temp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    set_time(&empty, 8_888_888_888);
    assert_eq!(
        maintenance::tree_stats(&empty, std::time::Instant::now() + Duration::from_secs(5))
            .unwrap(),
        (0, expected(&empty))
    );
}
#[test]
fn retention_tree_stats_rejects_linked_roots_files_and_directories_and_expired_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, b"outside contents stay intact").unwrap();
    let linked_root = temp.path().join("linked-root");
    std::os::unix::fs::symlink(&root, &linked_root).unwrap();
    assert!(maintenance::tree_stats(
        &linked_root,
        std::time::Instant::now() + Duration::from_secs(5)
    )
    .is_err());
    let link = root.join("linked-file");
    std::os::unix::fs::symlink(&sentinel, &link).unwrap();
    let error = maintenance::tree_stats(&root, std::time::Instant::now() + Duration::from_secs(5))
        .unwrap_err();
    assert!(error.to_string().contains("unsafe maintenance tree entry"));
    fs::remove_file(link).unwrap();
    let link = root.join("linked-directory");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let error = maintenance::tree_stats(&root, std::time::Instant::now() + Duration::from_secs(5))
        .unwrap_err();
    assert!(error.to_string().contains("unsafe maintenance tree entry"));
    fs::remove_file(link).unwrap();
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"outside contents stay intact"
    );
    assert!(
        maintenance::tree_stats(&root, std::time::Instant::now() - Duration::from_secs(1))
            .unwrap_err()
            .to_string()
            .contains("maintenance timeout")
    );
}

#[test]
fn parallel_retention_scan_failure_never_claims_or_deletes_another_version() {
    let (_tmp, c) = fixture();
    let old = "a".repeat(40);
    let current = "c".repeat(40);
    version(&c, &old);
    version(&c, &current);
    let base = maintenance::image_base(&c, "images").unwrap();
    std::os::unix::fs::symlink(format!("releases/{current}/images"), base.join("current")).unwrap();
    std::os::unix::fs::symlink(
        "/etc/passwd",
        base.join("releases").join(&current).join("images/unsafe"),
    )
    .unwrap();
    assert!(maintenance::run("retention", &["--apply".into()], &c).is_err());
    assert!(base
        .join("releases")
        .join(old)
        .join("images/a.png")
        .is_file());
    assert!(!base.join(".retention-trash").exists());
    assert!(!maintenance::state(&c)
        .unwrap()
        .join("retention-last.json")
        .exists());
}
