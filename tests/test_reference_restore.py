"""A reference release restores the exact source object without changing MIME owners."""

import copy
from pathlib import Path
import unittest

from open_assets import __main__ as core, object_restore, restore_check
import test_object_restore as fixture


class ReferenceRestore(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.ObjectRestore()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        f = self.fixture
        f.store.objects = copy.deepcopy(f.original_objects)
        self.origin, self.sha = f.shas
        self.manifest = copy.deepcopy(f.manifests[1])
        self.manifest.update(object_layout="references-v1", object_sources={})
        for item in self.manifest["files"]:
            if item["path"] == "images/pic.png":
                continue
            self.manifest["object_sources"][item["path"]] = dict(
                key=f.prefix(self.origin) + item["path"],
                content_type=item["content_type"],
            )
            del f.store.objects[f.prefix(self.sha) + item["path"]]
        # Logical response MIME may differ from the immutable source owner.
        next(x for x in self.manifest["files"] if x["path"] == "document.txt")[
            "content_type"
        ] = "application/x-logical-document"
        core.atomic(f.state / "manifests/assets" / (self.sha + ".json"), self.manifest)
        core.atomic(f.state / "published/assets", self.manifest)

    def test_restore_missing_shared_source_keeps_manifest_and_current_version(self):
        f = self.fixture
        name = f.prefix(self.origin) + "document.txt"
        del f.store.objects[name]
        report = object_restore.run(f.config, "assets", self.sha, f.store)
        self.assertEqual(report["status"], "complete")
        self.assertEqual(f.store.puts, [name])
        self.assertEqual(f.store.objects[name]["mime"], "text/plain")
        self.assertEqual(f.store.objects[name]["data"], b"attachment")
        checked = restore_check.run(f.config, "assets", self.sha, f.store)
        self.assertEqual(checked["checks"]["objects"]["status"], "ok")
        self.assertEqual(
            core.read_job(next((f.state / "jobs").glob("*.json")))["status"],
            "published",
        )

    def test_conflicting_reference_source_is_never_overwritten(self):
        f = self.fixture
        name = f.prefix(self.origin) + "document.txt"
        f.store.objects[name]["data"] = b"wrong"
        with self.assertRaises(ValueError):
            object_restore.run(f.config, "assets", self.sha, f.store)
        self.assertEqual(f.store.puts, [])

    def test_direct_snapshot_local_recovery_and_rollback(self):
        import shutil

        f = self.fixture
        self.manifest["image_layout"] = "snapshot-v1"
        core.atomic(f.state / "manifests/assets" / (self.sha + ".json"), self.manifest)
        core.atomic(f.state / "published/assets", self.manifest)
        # Switch to the direct format then lose only this disposable local tree.
        base = Path(f.config["images_root"]) / "repos/assets"
        core.switch(base, self.sha + "/images")
        shutil.rmtree(base / "releases" / self.sha)
        shutil.rmtree(base / "releases" / ("images-" + self.sha))
        report = object_restore.run(
            f.config, "assets", self.sha, f.store, with_local=True
        )
        self.assertTrue(report["local_verified"])
        core.rollback_images(f.config, "assets", self.sha)
        self.assertFalse((base / "releases" / ("images-" + self.sha)).exists())
        self.assertEqual(
            (base / "current").readlink().as_posix(), "releases/" + self.sha + "/images"
        )
        checked = restore_check.run(f.config, "assets", self.sha, f.store)
        self.assertEqual(checked["checks"]["images"]["status"], "ok")
