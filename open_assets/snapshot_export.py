"""Bounded linking of independent immutable inodes; aliases always stay serial."""

from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import threading

from . import incremental
from .publication import policy


def prepare(config, destination, blobs, reuse, prepare_parent, check=None):
    if not reuse or not reuse.get("reference_mode"):
        return {}
    groups = {}
    count = 0
    for index, (oid, relative, _, size) in enumerate(blobs):
        record = reuse["sources"].get(oid.decode())
        if record is None or record["item"]["size"] != size:
            continue
        proof = record.get("verified") or {}
        identity = proof.get("fingerprint", [])
        if not isinstance(identity, list) or len(identity) != 6:
            return {}  # Let the normal path reject incomplete evidence.
        group = (identity[0], identity[1])
        groups.setdefault(group, []).append((index, oid, relative, size))
        count += 1
    if count < 32 or policy(config) == 1:
        return {}
    for entries in groups.values():
        for _, _, relative, _ in entries:
            prepare_parent((destination / relative).parent)
    groups = list(groups.values())
    batches = iter(groups[offset : offset + 32] for offset in range(0, len(groups), 32))
    result = {}
    check_guard = threading.Lock()

    def materialize(batch):
        local = dict(
            sources=reuse["sources"],
            reference_mode=True,
            reused={},
            verified={},
            reused_files=0,
        )
        completed = []
        for entries in batch:
            for index, oid, relative, size in entries:
                if check is not None:
                    with check_guard:
                        check()
                item = incremental.copy_cached(
                    local, oid, size, relative, destination / relative
                )
                if item is not None:
                    completed.append((index, item))
        return completed, local

    with ThreadPoolExecutor(max_workers=policy(config)) as pool:
        pending = set()

        def submit():
            batch = next(batches, None)
            if batch is not None:
                pending.add(pool.submit(materialize, batch))

        try:
            for _ in range(policy(config)):
                submit()
            while pending:
                done, pending = wait(pending, return_when=FIRST_COMPLETED)
                for future in done:
                    completed, local = future.result()
                    result.update(completed)
                    reuse["verified"].update(local["verified"])
                    reuse["reused"].update(local["reused"])
                    reuse["reused_files"] += local["reused_files"]
                for _ in done:
                    submit()
        except BaseException:
            for future in pending:
                future.cancel()
            raise
    return result
