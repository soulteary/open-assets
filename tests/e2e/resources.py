"""Large ordinary publication against disposable Git/S3, never production state."""

import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
from unittest.mock import patch

from open_assets import __main__ as core
from open_assets import resources

assert os.environ.get("OPEN_ASSETS_RESOURCES_TEST") == "1"
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    source = root / "source"
    source.mkdir()
    images = root / "images"
    images.mkdir()

    def git(*args):
        return subprocess.check_output(
            ["git", "-C", str(source), *args], stderr=subprocess.DEVNULL
        )

    git("init", "-b", "main")
    git("config", "user.name", "Fixture")
    git("config", "user.email", "fixture@localhost.invalid")
    (source / "public/images").mkdir(parents=True)
    (source / "public/images/pic.png").write_bytes(b"fixture-image")
    size = 96 * 1024**2
    digest = hashlib.sha256()
    with (source / "public/large.bin").open("wb") as stream:
        for _ in range(96):
            block = os.urandom(1024**2)
            stream.write(block)
            digest.update(block)
    del block
    git("add", ".")
    git("-c", "commit.gpgsign=false", "commit", "-m", "fixture")
    sha = git("rev-parse", "HEAD").decode().strip()
    config = dict(
        state_root=str(root / "state"),
        images_root=str(images),
        endpoint="http://objects-fixture:9000",
        bucket="resource-fixture",
        revision="fixture",
        resource_limits=dict(queue_active_records=2, queue_total_records=2),
        repositories=[
            dict(
                key="assets",
                enabled=True,
                remote=str(source),
                branch="main",
                source_directory="public",
                mount_path="/",
                max_bytes=128 * 1024**2,
            )
        ],
    )
    store = core.s3client(config, bounded=True)
    store.create_bucket(Bucket=config["bucket"])
    job_id = core.enqueue(config, "assets", sha, "fixture")
    # This fixture only contacts its disposable local Git/S3 instance. Retain
    # Git diagnostics for failures here; production continues to discard stderr.
    original_popen = subprocess.Popen
    with tempfile.TemporaryFile() as diagnostic:

        def traced_popen(*args, **kwargs):
            kwargs["stderr"] = diagnostic
            return original_popen(*args, **kwargs)

        with patch.object(subprocess, "Popen", traced_popen):
            try:
                core.work_once(config)
                job = core.read_job(root / "state/jobs" / (job_id + ".json"))
                assert job["status"] == "published", (
                    job.get("error"),
                    job.get("resource_limit"),
                )
            except Exception:
                diagnostic.seek(0)
                print(diagnostic.read(65536).decode(errors="replace"), file=sys.stderr)
                raise
    assert (
        core.read_job(root / "state/jobs" / (job_id + ".json"))["status"] == "published"
    )
    # A distinct manual task verifies already-present objects without replacing them.
    manual = core.enqueue(config, "assets", sha, "fixture", kind="manual")
    core.work_once(config)
    assert (
        core.read_job(root / "state/jobs" / (manual + ".json"))["status"] == "published"
    )
    assert core.enqueue(config, "assets", sha, "duplicate") == job_id
    try:
        core.enqueue(config, "assets", "a" * 40, "refused")
    except resources.ResourceLimit:
        pass
    else:
        raise AssertionError("full queue admitted work")
    item = dict(size=size, sha256=digest.hexdigest())
    resources.verify_body(
        store.get_object(
            Bucket=config["bucket"], Key=f"repositories/assets/releases/{sha}/large.bin"
        )["Body"],
        item,
    )
    assert (images / "repos/assets/current/pic.png").read_bytes() == b"fixture-image"
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
    assert peak < size, "publisher peak RSS reached the single-file size"
    print(
        json.dumps(
            dict(
                file_bytes=size,
                publisher_peak_rss_bytes=peak,
                initial_publish=True,
                immutable_replay=True,
                streamed_readback=True,
                queue_refusal=True,
                dedup_at_capacity=True,
            )
        )
    )
