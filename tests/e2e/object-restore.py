"""Restore fixture objects with real Git and an isolated, write-scoped S3 identity."""

import json
import os
import sys
from unittest.mock import patch

from botocore.exceptions import ClientError

from open_assets import object_restore as restore
from open_assets import __main__ as core

assert os.environ.get("OPEN_ASSETS_OBJECT_RESTORE_TEST") == "1"
sys.path.insert(0, "/tests")
import test_object_restore as fixtures  # noqa: E402

fixture = fixtures.ObjectRestore()
fixture.setUp()
try:
    config = fixture.config
    config.update(bucket="restore-fixture", endpoint="http://objects-fixture:9000")
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
    print(
        "Isolated restore: archived/manual dedup bypassed without job changes; full bytes/metadata verified; repeat and partial retry passed; identity cannot delete; current images unchanged"
    )
finally:
    fixture.doCleanups()
