"""Restore fixture objects with real Git and an isolated, write-scoped S3 identity."""

import json
import os
import sys
from pathlib import Path

import yaml
from unittest.mock import patch

from botocore.exceptions import ClientError

from open_assets import object_restore as restore
from open_assets import restore_check as check
from open_assets import restore_check_info as check_info
from open_assets import restore_close as close
from open_assets import restore_close_info as close_info
from open_assets import restore_audit as audit
from open_assets import audit_info, audit_observation
from open_assets import __main__ as core

assert os.environ.get("OPEN_ASSETS_OBJECT_RESTORE_TEST") == "1"
sys.path.insert(0, "/tests")
import test_object_restore as fixtures  # noqa: E402

fixture = fixtures.ObjectRestore()
fixture.setUp()
try:
    config = fixture.config
    config.update(bucket="restore-fixture", endpoint="http://objects-fixture:9000")
    from open_assets.restore_info import summary

    host = (
        Path(fixture.tmp.name) / "info-host"
        if hasattr(fixture, "tmp")
        else fixture.state.parent / "info-host"
    )
    host.mkdir(exist_ok=True)
    (host / "private").mkdir(exist_ok=True)
    (host / "data").mkdir(exist_ok=True)
    (host / "data/publisher").symlink_to(fixture.state, target_is_directory=True)
    (host / "data/images").symlink_to(fixture.images, target_is_directory=True)
    (host / "private/repositories.yaml").write_text(
        yaml.safe_dump(
            dict(config, schema_version=1, state_root="/state", images_root="/images")
        )
    )

    def observation():
        row = summary(host, host / "data")["repositories"][0]
        assert row["result_status"] == "ok"
        return row["attempt"]

    root = core.s3client(config, bounded=True)
    root.create_bucket(Bucket=config["bucket"])
    for key, value in fixture.original_objects.items():
        root.put_object(
            Bucket=config["bucket"],
            Key=key,
            Body=value["data"],
            Metadata=value["metadata"],
            ContentType=value["mime"],
            CacheControl=value["cache"],
        )
    for key in fixture.original_objects:
        if key.startswith(fixture.prefix()):
            root.delete_object(Bucket=config["bucket"], Key=key)
    with patch.dict(
        os.environ,
        S3_ACCESS_KEY_FILE="/fixture/cleaner_access",
        S3_SECRET_KEY_FILE="/fixture/cleaner_secret",
    ):
        writer = core.s3client(config, bounded=True)
    try:
        writer.delete_object(
            Bucket=config["bucket"], Key=fixture.prefix() + "images/pic.png"
        )
    except ClientError as error:
        assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 403
    else:
        raise AssertionError("Restore identity unexpectedly has DeleteObject")
    assert (
        core.enqueue(config, "assets", fixture.sha, "again", kind="manual")
        == fixture.manual
    )
    report = restore.run(config, "assets", fixture.sha, writer)
    assert report["status"] == "complete" and report["uploaded_objects"] == len(
        fixture.manifests[0]["files"]
    )
    assert observation()["objects_verified"]
    fixture.assert_current()
    assert fixture.job_bytes() == fixture.original_jobs
    assert json.loads(fixture.marker.read_text())["status"] == "restored"
    for key, value in fixture.original_objects.items():
        assert (
            root.get_object(Bucket=config["bucket"], Key=key)["Body"].read()
            == value["data"]
        )
    repeat = restore.run(config, "assets", fixture.sha, writer)
    assert repeat["uploaded_objects"] == 0 and repeat["repaired_objects"] == 0
    # A failed upload leaves the deletion barrier in place and the next command resumes.
    for key in fixture.original_objects:
        if key.startswith(fixture.prefix()):
            root.delete_object(Bucket=config["bucket"], Key=key)
    core.atomic(
        fixture.marker,
        dict(schema_version=1, key="assets", sha=fixture.sha, status="complete"),
    )
    put = writer.put_object
    calls = [0]

    def interrupt(**kwargs):
        calls[0] += 1
        if calls[0] == 2:
            raise OSError("intentional fixture interruption")
        return put(**kwargs)

    try:
        with patch.object(writer, "put_object", side_effect=interrupt):
            restore.run(config, "assets", fixture.sha, writer)
    except OSError:
        pass
    else:
        raise AssertionError("Expected partial restore failure")
    assert json.loads(fixture.marker.read_text())["status"] == "complete"
    assert json.loads(restore.report_path(config, "assets", fixture.sha).read_text())[
        "partial_possible"
    ]
    assert observation()["status"] == "failed" and observation()["uncertain"]
    diagnosed = check.run(config, "assets", fixture.sha, writer)
    assert (
        diagnosed["diagnosis"] == "issues"
        and diagnosed["checks"]["barrier"]["status"] == "blocked"
    )
    source_before_close = restore.report_path(
        config, "assets", fixture.sha
    ).read_bytes()
    receipt_before_close = fixture.marker.read_bytes()
    refused = close.run(
        config, "assets", fixture.sha, "Fixture partial upload must not close", writer
    )
    assert refused["status"] == "refused" and refused["reason"] == "check-failed"
    assert (
        restore.report_path(config, "assets", fixture.sha).read_bytes()
        == source_before_close
    )
    assert fixture.marker.read_bytes() == receipt_before_close
    resumed = restore.run(config, "assets", fixture.sha, writer)
    assert resumed["uploaded_objects"] == len(fixture.manifests[0]["files"]) - 1
    key = fixture.prefix() + "images/pic.png"
    original = fixture.original_objects[key]
    root.put_object(
        Bucket=config["bucket"],
        Key=key,
        Body=original["data"],
        Metadata={"custom": "preserved"},
        ContentType="application/octet-stream",
        CacheControl="no-cache",
    )
    repaired = restore.run(config, "assets", fixture.sha, writer)
    assert repaired["repaired_objects"] == 1
    assert (
        root.get_object(Bucket=config["bucket"], Key=key)["Metadata"]["custom"]
        == "preserved"
    )
    fixture.assert_current()
    assert fixture.job_bytes() == fixture.original_jobs
    fixture.remove_local()
    local = restore.run(config, "assets", fixture.sha, writer, with_local=True)
    assert (
        local["local_verified"]
        and local["local_full_rebuilt"]
        and local["local_images_rebuilt"]
    )
    assert observation()["local_verified"] and not observation()["activated"]
    assert not local["activated"]
    fixture.assert_current()
    repeated = restore.run(config, "assets", fixture.sha, writer, with_local=True)
    assert not repeated["local_full_rebuilt"] and not repeated["local_images_rebuilt"]
    # Failure after durable activation intent leaves genuine uncertain evidence.
    try:
        with patch.object(
            core,
            "rollback_images_locked",
            side_effect=OSError("fixture activation interrupted"),
        ):
            restore.run(
                config, "assets", fixture.sha, writer, with_local=True, rollback=True
            )
    except OSError:
        pass
    else:
        raise AssertionError("Expected activation interruption")
    evidence = restore.report_path(config, "assets", fixture.sha).read_bytes()
    assert json.loads(evidence)["activation_uncertain"]
    diagnosed = check.run(config, "assets", fixture.sha, writer)
    assert diagnosed["diagnosis"] == "verified" and not diagnosed["current_is_target"]
    assert restore.report_path(config, "assets", fixture.sha).read_bytes() == evidence
    fixture.assert_current()
    observed = check_info.summary(host, host / "data")["repositories"][0]
    assert (
        observed["result_status"] == "ok"
        and observed["attempt"]["diagnosis"] == "verified"
    )
    assert not observed["attempt"].get("observation_unavailable")
    closed = close.run(
        config, "assets", fixture.sha, "Fixture operator keeps existing current", writer
    )
    assert closed["status"] == "closed" and not closed["current_is_target"]
    assert restore.report_path(config, "assets", fixture.sha).read_bytes() == evidence
    fixture.assert_current()
    audit_directory = fixture.state / "restore-closures/assets" / closed["audit_id"]
    audit_bytes = {p.name: p.read_bytes() for p in audit_directory.iterdir()}
    assert audit_bytes["source.json"] == evidence
    closed_row = close_info.summary(host, host / "data")["repositories"][0]
    assert (
        closed_row["result_status"] == "ok"
        and closed_row["attempt"]["status"] == "closed"
    )
    assert not closed_row["attempt"].get("observation_unavailable")
    repeated_close = close.run(
        config, "assets", fixture.sha, "Fixture repeat preserves prior audit", writer
    )
    assert (
        repeated_close["status"] == "closed"
        and repeated_close["audit_id"] != closed["audit_id"]
    )
    preview = audit.run(
        config, "assets", fixture.sha, action="archive", keep_last=1, min_age_days=0
    )
    assert preview["mode"] == "preview" and preview["archived_records"] == 0
    assert audit_bytes == {p.name: p.read_bytes() for p in audit_directory.iterdir()}
    archived = audit.run(
        config,
        "assets",
        fixture.sha,
        action="archive",
        apply=True,
        keep_last=1,
        min_age_days=0,
    )
    assert (
        archived["status"] == "complete"
        and archived["archived_records"] == 1
        and not audit_directory.exists()
    )
    package = (
        fixture.state
        / "restore-audit-archives/assets"
        / (closed["audit_id"] + ".tar.gz")
    )
    _, saved = audit.verify_archive(package, "assets", closed["audit_id"])
    assert saved == audit_bytes
    assert (
        audit.run(config, "assets", fixture.sha, action="verify")["status"]
        == "complete"
    )
    assert audit.run(config, "assets", fixture.sha)["total"] == 3
    fixture.assert_current()
    stats = audit_info.collect(config, {})
    sample = stats["repositories"]["assets"]["sample"]
    assert (
        sample["online"]["records"] == 2
        and sample["archive"]["records"] == 1
        and sample["retired"]["records"] == 0
    )
    assert audit_observation.read(config, "assets")["attempt"]["archived_records"] == 1
    with patch.object(
        audit, "scan", side_effect=AssertionError("cached call must not scan")
    ):
        assert audit_info.collect(config, stats) == stats
    (host / "data/info").mkdir()
    (host / "data/info/audit-cache.json").write_text(json.dumps(stats))
    dashboard = audit_info.summary(host, host / "data")["repositories"][0]
    assert (
        dashboard["archive"]["records"] == 1
        and dashboard["archival"]["attempt"]["status"] == "complete"
    )
    activated = restore.run(
        config, "assets", fixture.sha, writer, with_local=True, rollback=True
    )
    assert observation()["activated"] and observation()["rollback_requested"]
    assert activated["activated"]
    assert (
        os.readlink(fixture.images / "repos/assets/current")
        == "releases/images-" + fixture.sha
    )
    assert (
        json.loads((fixture.state / "published/assets").read_text())
        == fixture.manifests[0]
    )
    assert fixture.job_bytes() == fixture.original_jobs
    diagnosed = check.run(config, "assets", fixture.sha, writer)
    assert (
        diagnosed["diagnosis"] == "verified"
        and diagnosed["current_is_target"]
        and diagnosed["published_is_target"]
    )
    closed_row = close_info.summary(host, host / "data")["repositories"][0]
    assert (
        closed_row["attempt"]["source_changed"]
        and closed_row["attempt"]["references_changed"]
    )
    assert audit.verify_archive(package, "assets", closed["audit_id"])[1] == audit_bytes
    print(
        "Isolated restore: S3 bytes, metadata, retry and no-delete identity verified; both local copies rebuilt and repeated without activation; explicit rollback activated verified version; filtered Info success/partial-failure/local/activation observations verified; read-only diagnostic checks and Info snapshots verified; explicit closure/audit preservation/changed-source Info verified; query/preview/verified archival/current preservation verified; cached audit counts/operation outcomes and no-repeat scanning verified; jobs unchanged"
    )
finally:
    fixture.doCleanups()
