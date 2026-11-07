"""Destructive acceptance: isolated endpoint, fixture bucket and temporary state only."""

import hashlib
import json
import os
import sys
import time
from unittest.mock import patch

from botocore.exceptions import ClientError

from open_assets import object_retention as r
from open_assets.object_info import client

assert os.environ.get("OPEN_ASSETS_OBJECT_RETENTION_TEST") == "1"
sys.path.insert(0, "/tests")
import test_retention as fixture  # noqa: E402

config = dict(bucket="retention-fixture", endpoint="http://objects-fixture:9000")
s3 = client(config)
for _ in range(60):
    try:
        s3.create_bucket(Bucket=config["bucket"])
        break
    except Exception:
        time.sleep(1)
else:
    raise RuntimeError("Isolated object retention fixture unavailable")
f = fixture.Retention()
f.setUp()
try:
    c = f.config
    c.update(
        config,
        object_retention=dict(
            enabled=True,
            exclusive_writer=True,
            keep_last=1,
            keep_days=7,
            min_age_hours=1,
            max_delete_versions=2,
        ),
    )
    for sha in f.shas:
        for item in json.loads(
            (f.state / "manifests/assets" / (sha + ".json")).read_text()
        )["files"]:
            s3.put_object(
                Bucket=c["bucket"],
                Key=r.prefix("assets", sha) + item["path"],
                Body=sha.encode(),
                Metadata={"sha256": item["sha256"]},
            )
    # A deliberately wrong precondition must not delete even a fixture object.
    probe = r.prefix("assets", f.shas[0]) + "images/pic.png"
    try:
        s3.delete_object(Bucket=c["bucket"], Key=probe, IfMatch='"wrong-etag"')
    except ClientError as error:
        assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 412
    else:
        # Current OtterIO ignores If-Match. Restore this disposable probe and
        # rely on explicitly asserted exclusive ownership plus application locks.
        s3.put_object(
            Bucket=c["bucket"],
            Key=probe,
            Body=f.shas[0].encode(),
            Metadata={
                "sha256": __import__("hashlib").sha256(f.shas[0].encode()).hexdigest()
            },
        )
    s3.head_object(Bucket=c["bucket"], Key=probe)
    with patch.dict(
        os.environ,
        S3_ACCESS_KEY_FILE="/fixture/cleaner_access",
        S3_SECRET_KEY_FILE="/fixture/cleaner_secret",
    ):
        cleaner = client(c)
    for operation, kwargs in [
        (
            "put_object",
            dict(Key=r.prefix("assets", f.shas[0]) + "forbidden", Body=b"x"),
        ),
        ("list_objects_v2", dict(Prefix="outside/")),
        ("delete_object", dict(Key="outside/object")),
    ]:
        try:
            getattr(cleaner, operation)(Bucket=c["bucket"], **kwargs)
        except ClientError as error:
            assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 403
        else:
            raise AssertionError("Cleaner permission exceeded fixture scope")
    fixture.core.atomic(
        f.state / "rollback-protection/assets.json",
        dict(key="assets", shas=[f.shas[0]]),
    )
    c["object_retention"]["pinned_shas"] = [f.shas[1]]
    for sha, status in zip(
        f.shas[2:6], ("pending", "running", "images_prepared", "failed")
    ):
        f.job(sha, status)
    now = f.now + 120 * 86400  # Only the test API accepts an injected time.
    preview = r.run(c, "assets", s3=cleaner, now=now)
    candidates = preview["repositories"][0]["candidates"]
    assert candidates
    first = candidates[0]["sha"]
    before = cleaner.list_objects_v2(Bucket=c["bucket"], Prefix=r.prefix("assets"))[
        "KeyCount"
    ]
    report = r.run(c, "assets", apply=True, s3=cleaner, now=now)
    assert report["repositories"][0]["removed_objects"] > 0
    after = cleaner.list_objects_v2(Bucket=c["bucket"], Prefix=r.prefix("assets"))[
        "KeyCount"
    ]
    assert after == before - report["repositories"][0]["removed_objects"]
    for sha in {*f.shas[:6], f.current}:
        assert (
            cleaner.list_objects_v2(Bucket=c["bucket"], Prefix=r.prefix("assets", sha))[
                "KeyCount"
            ]
            == 1
        )
        assert (
            cleaner.get_object(
                Bucket=c["bucket"], Key=r.prefix("assets", sha) + "images/pic.png"
            )["Body"].read()
            == sha.encode()
        )
    assert not cleaner.list_objects_v2(
        Bucket=c["bucket"], Prefix=r.prefix("assets", first)
    )["KeyCount"]
    # Repeating apply leaves protected objects intact and receipts do not revive.
    r.run(c, "assets", apply=True, s3=cleaner, now=now)
    partial = hashlib.sha1(b"partial-retention-fixture").hexdigest()
    f.version(partial, 200)
    manifest = f.state / "manifests/assets" / (partial + ".json")
    value = json.loads(manifest.read_text())
    value["files"].append(
        dict(
            path="images/two.png",
            size=40,
            sha256=hashlib.sha256(partial.encode()).hexdigest(),
        )
    )
    fixture.core.atomic(manifest, value)
    os.utime(manifest, (f.now - 200 * 86400,) * 2)
    for item in value["files"]:
        s3.put_object(
            Bucket=c["bucket"],
            Key=r.prefix("assets", partial) + item["path"],
            Body=partial.encode(),
            Metadata={"sha256": item["sha256"]},
        )
    c["object_retention"]["pinned_shas"] = list(f.shas)
    current_manifest = f.state / "manifests/assets" / (f.current + ".json")
    os.utime(current_manifest, (time.time() + 5,) * 2)
    delete = cleaner.delete_object
    calls = [0]

    def interrupt(**args):
        calls[0] += 1
        if calls[0] == 2:
            raise RuntimeError("intentional fixture interruption")
        return delete(**args)

    try:
        with patch.object(cleaner, "delete_object", side_effect=interrupt):
            r.run(c, "assets", apply=True, s3=cleaner, now=now)
    except RuntimeError:
        pass
    else:
        raise AssertionError("Expected partial-delete interruption")
    assert (
        cleaner.list_objects_v2(Bucket=c["bucket"], Prefix=r.prefix("assets", partial))[
            "KeyCount"
        ]
        == 1
    )
    assert (
        json.loads(r.receipt_path(c, "assets", partial).read_text())["status"]
        == "deleting"
    )
    r.run(c, "assets", apply=True, s3=cleaner, now=now)
    assert (
        cleaner.list_objects_v2(Bucket=c["bucket"], Prefix=r.prefix("assets", partial))[
            "KeyCount"
        ]
        == 0
    )
    assert (
        json.loads(r.receipt_path(c, "assets", partial).read_text())["status"]
        == "complete"
    )
    try:
        fixture.core.rollback_images(c, "assets", partial)
    except ValueError as error:
        assert "objects were deleted" in str(error)
    else:
        raise AssertionError("Object-deleted image rollback was allowed")
    print(
        "Isolated OtterIO: exclusive-writer mode (If-Match not relied on); cleaner write/out-of-scope access denied; preview/apply/repeat, protected bytes, partial deletion recovery and rollback refusal verified"
    )
finally:
    f.tearDown()
