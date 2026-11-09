"""Compaction verifies bytes before deleting only redundant physical objects."""

import hashlib
import json
from pathlib import Path
import unittest
from unittest.mock import patch

from open_assets import object_compact, object_refs
import test_incremental as fixture
import test_reference_publication as refs


class Store(fixture.Store):
    def __init__(self):
        super().__init__()
        self.list_calls = 0
        self.deleted = []
        self.partial = False

    list_objects_v2 = refs.ReferenceStore.list_objects_v2

    def get_bucket_versioning(self, **kwargs):
        return {}

    def get_object(self, **kwargs):
        result = super().get_object(**kwargs)
        result["ContentType"] = self.objects[kwargs["Key"]][1]["ContentType"]
        return result

    def delete_objects(self, Bucket, Delete):
        if self.partial:
            return dict(Errors=[dict(Key=Delete["Objects"][0]["Key"], Code="Denied")])
        for item in Delete["Objects"]:
            self.deleted.append(item["Key"])
            del self.objects[item["Key"]]
        return dict(Deleted=Delete["Objects"])


class Compact(unittest.TestCase):
    def setUp(self):
        reader = patch.object(object_compact, "reader_ready")
        reader.start()
        self.addCleanup(reader.stop)
        f = self.f = fixture.Incremental()
        f.setUp()
        self.addCleanup(f.doCleanups)
        f.store = Store()
        self.versions = [f.publish()[0], f.publish()[0]]
        f.config["publication"] = dict(reference_releases=True)
        self.versions.extend([f.publish()[0], f.publish()[0]])
        self.state = Path(f.config["state_root"])
        (self.state / "object-retention").mkdir(exist_ok=True)
        (self.state / "object-retention/compaction.lock").touch()

    def assert_versions(self):
        for sha in self.versions:
            manifest = json.loads(
                (self.state / "manifests/assets" / (sha + ".json")).read_text()
            )
            for item in manifest["files"]:
                source = object_refs.source(manifest, item)
                data, metadata = self.f.store.objects[source["key"]]
                self.assertEqual(hashlib.sha256(data).hexdigest(), item["sha256"])
                self.assertEqual(metadata["ContentType"], source["content_type"])

    def test_preview_then_one_object_per_content_and_all_urls_retained(self):
        f = self.f
        original = (self.state / "published/assets").read_bytes()
        preview = object_compact.run(f.config, f.store)
        self.assertEqual(preview["objects_before"], 9)
        self.assertEqual(preview["objects_after"], 3)
        self.assertFalse(f.store.deleted)
        result = object_compact.run(f.config, f.store, apply=True)
        self.assertEqual(result["deleted"], 6)
        self.assertEqual(len(f.store.objects), 3)
        self.assertEqual((self.state / "published/assets").read_bytes(), original)
        self.assert_versions()
        self.assertEqual(object_compact.run(f.config, f.store)["redundant_bytes"], 0)

    def test_empty_status_is_unversioned_but_enabled_is_refused(self):
        with patch.object(
            self.f.store, "get_bucket_versioning", return_value={"Status": ""}
        ):
            self.assertEqual(
                object_compact.run(self.f.config, self.f.store)["objects_after"], 3
            )
        for status in ("Enabled", "Suspended"):
            with (
                patch.object(
                    self.f.store,
                    "get_bucket_versioning",
                    return_value={"Status": status},
                ),
                self.assertRaises(ValueError),
            ):
                object_compact.run(self.f.config, self.f.store, apply=True)
        self.assertFalse(self.f.store.deleted)

    def test_bad_old_source_stops_before_metadata_or_delete(self):
        f = self.f
        name = next(iter(f.store.objects))
        _, metadata = f.store.objects[name]
        f.store.objects[name] = (b"wrong", metadata)
        originals = {
            p: p.read_bytes() for p in (self.state / "manifests/assets").glob("*.json")
        }
        with self.assertRaises(ValueError):
            object_compact.run(f.config, f.store, apply=True)
        self.assertFalse(f.store.deleted)
        self.assertEqual(originals, {p: p.read_bytes() for p in originals})

    def test_partial_delete_keeps_every_logical_version_resolvable(self):
        f = self.f
        f.store.partial = True
        with self.assertRaises(ValueError):
            object_compact.run(f.config, f.store, apply=True)
        self.assert_versions()
        self.assertTrue(
            list((self.state / "object-compaction").glob("*/committed.json"))
        )

    def test_missing_reader_guard_refuses_rewrite(self):
        (self.state / "object-retention/compaction.lock").unlink()
        with self.assertRaises(ValueError):
            object_compact.run(self.f.config, self.f.store, apply=True)
        self.assertFalse(self.f.store.deleted)

    def test_reader_guard_prevents_exclusive_deletion(self):
        import time

        guard = self.state / "object-retention/compaction.lock"
        with (
            object_refs.read_guard(guard),
            patch.object(time, "monotonic", side_effect=[0, 31]),
            self.assertRaises(BlockingIOError),
        ):
            with object_refs.read_guard(guard, exclusive=True, required=True):
                self.fail("reader still holds its source object")
