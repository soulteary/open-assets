"""Run only against a disposable, unexposed OtterIO on an isolated Docker network."""

from concurrent.futures import ThreadPoolExecutor
import os
import time
from unittest.mock import patch

from open_assets import object_info as oi

assert os.environ.get("OPEN_ASSETS_OBJECTS_TEST") == "1", "Disposable fixture required"
config = dict(bucket="statistics-fixture", endpoint="http://objects-fixture:9000")
s3 = oi.client(config)
for _ in range(60):
    try:
        s3.create_bucket(Bucket=config["bucket"])
        break
    except Exception:
        time.sleep(1)
else:
    raise RuntimeError("Isolated storage fixture unavailable")


def upload(index):
    body = b"x" * (index % 9)
    s3.put_object(
        Bucket=config["bucket"],
        Key=f"repositories/alpha/releases/fixture/{index:04d}-中文 +%.bin",
        Body=body,
    )
    return len(body)


with ThreadPoolExecutor(max_workers=4) as pool:
    expected_bytes = sum(pool.map(upload, range(1003)))
for name in ("a", "b"):
    s3.put_object(
        Bucket=config["bucket"], Key="repositories/alphabet/" + name, Body=b"abc"
    )
s3.put_object(Bucket=config["bucket"], Key="outside-scope/object", Body=b"excluded")
keys = ["alpha", "alphabet", "empty"]
cache = {}
with patch.object(oi, "MAX_PAGES", 1):
    for _ in range(4):
        cache = oi.scan(config, keys, "fixture", cache, s3)
        assert cache["request_pages"] == 1
        if _ == 0:
            assert cache["repositories"]["alpha"]["status"] == "scanning"
            assert "complete" not in cache["repositories"]["alpha"]
        for row in cache["repositories"].values():
            row["attempted_at"] -= oi.INTERVAL + 1
assert cache["repositories"]["alpha"]["complete"]["objects"] == 1003
assert cache["repositories"]["alpha"]["complete"]["bytes"] == expected_bytes
assert cache["repositories"]["alphabet"]["complete"]["objects"] == 2
assert cache["repositories"]["alphabet"]["complete"]["bytes"] == 6
assert cache["repositories"]["empty"]["complete"]["objects"] == 0
for row in cache["repositories"].values():
    row["attempted_at"] = time.time()
with patch.object(
    s3, "list_objects_v2", side_effect=AssertionError("Unexpected listing")
) as listing:
    repeat = oi.scan(config, keys, "fixture", cache, s3)
    assert repeat["request_pages"] == 0
    listing.assert_not_called()
print(
    "Isolated OtterIO: 1003/2/0 object counts and bytes verified; pagination resumed across four bounded runs; repeat issued zero S3 requests"
)
