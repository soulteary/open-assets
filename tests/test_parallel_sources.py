import datetime
import threading
import time
import unittest

from open_assets import object_refs, resources


class Sources(unittest.TestCase):
    def fixture(self, count=2501):
        prefix = "repositories/assets/releases/" + "a" * 40 + "/"
        records = [
            dict(
                key=prefix + f"{i:05}.png",
                etag='"etag"',
                modified=123,
                item=dict(size=1),
            )
            for i in range(count)
        ]
        values = {
            r["key"]: dict(
                Key=r["key"],
                Size=1,
                ETag=r["etag"],
                LastModified=datetime.datetime.fromtimestamp(
                    123, datetime.timezone.utc
                ),
            )
            for r in records
        }
        return records, values

    def client(self, values, extra=False):
        class Store:
            active = 0
            maximum = 0
            lock = threading.Lock()

            def list_objects_v2(
                s, Prefix, MaxKeys, StartAfter="", ContinuationToken="", **kwargs
            ):
                with s.lock:
                    s.active += 1
                    s.maximum = max(s.maximum, s.active)
                try:
                    time.sleep(0.02)
                    lower = ContinuationToken or StartAfter
                    keys = sorted(
                        k for k in values if k.startswith(Prefix) and k > lower
                    )
                    selected = keys[:MaxKeys]
                    page = dict(
                        Contents=[values[k] for k in selected],
                        IsTruncated=len(keys) > MaxKeys,
                    )
                    if page["IsTruncated"]:
                        page["NextContinuationToken"] = selected[-1]
                    return page
                finally:
                    with s.lock:
                        s.active -= 1

        return Store()

    def test_ranges_run_concurrently_and_cover_every_record(self):
        records, values = self.fixture()
        client = self.client(values)
        result = object_refs.verify_sources(dict(bucket="test"), client, records)
        self.assertEqual(result["source_list_requests"], 3)
        self.assertGreater(client.maximum, 1)
        self.assertLessEqual(client.maximum, 4)

    def test_unlisted_keys_between_boundaries_require_more_pages_but_not_less_validation(
        self,
    ):
        records, values = self.fixture(1002)
        name = records[0]["key"] + "-extra"
        values[name] = dict(values[records[0]["key"]], Key=name)
        result = object_refs.verify_sources(
            dict(bucket="test"), self.client(values), records
        )
        self.assertGreaterEqual(result["source_list_requests"], 3)

    def test_missing_or_changed_boundary_is_never_hidden_by_later_keys(self):
        for change in ("missing", "etag", "size", "modified"):
            records, values = self.fixture(1002)
            key = records[999]["key"]
            if change == "missing":
                del values[key]
            elif change == "etag":
                values[key]["ETag"] = "bad"
            elif change == "size":
                values[key]["Size"] = 2
            else:
                values[key]["LastModified"] = datetime.datetime.fromtimestamp(
                    124, datetime.timezone.utc
                )
            with self.assertRaises(ValueError):
                object_refs.verify_sources(
                    dict(bucket="test"), self.client(values), records
                )

    def test_budget_rejects_excessive_sources_before_any_request(self):
        records, _ = self.fixture(1)
        records = [
            dict(
                records[0], key="repositories/assets/releases/" + f"{i:040x}" + "/x.png"
            )
            for i in range(129)
        ]
        client = self.client({})
        with self.assertRaises(ValueError):
            object_refs.verify_sources(dict(bucket="test"), client, records)
        self.assertEqual(client.maximum, 0)

    def test_git_transaction_is_bounded(self):
        with self.assertRaises(ValueError):
            resources.run_git(".", [], input_data=b"x" * 65537)
