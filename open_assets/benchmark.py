"""Bounded, loopback HTTPS read benchmark against an explicitly published fixture."""

import argparse
import concurrent.futures
import hashlib
import http.client
import json
import math
import os
import shutil
import socket
import statistics
import threading
import time
import urllib.parse
from pathlib import Path

import yaml

from . import telemetry


class LoopbackHTTPS(http.client.HTTPSConnection):
    def connect(self):
        sock = socket.create_connection(("127.0.0.1", 443), timeout=self.timeout)
        self.sock = self._context.wrap_socket(sock, server_hostname=self.host)


def percentile(values, fraction):
    return (
        sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else 0
    )


def resources():
    result = []
    for line in telemetry.command(
        "docker", "stats", "--no-stream", "--format", "{{json .}}"
    ).splitlines():
        raw = json.loads(line)
        # Logical names only; exclude full IDs, paths, inspect data and addresses.
        name = raw["Name"]
        role = next(
            (
                role
                for role in telemetry.SERVICES + ("traefik",)
                if name.endswith("-" + role + "-1")
            ),
            None,
        )
        if role:
            result.append(
                dict(
                    service=role,
                    cpu_percent=float(raw["CPUPerc"].rstrip("%")),
                    memory_bytes=telemetry.size_bytes(raw["MemUsage"].split("/")[0]),
                )
            )
    return dict(
        collected_at=time.time(),
        components=result,
        memory=telemetry.memory()["memory"],
        load=list(os.getloadavg()),
    )


def summarize_resources(samples):
    roles = sorted(
        {row["service"] for sample in samples for row in sample["components"]}
    )
    result = []
    for role in roles:
        rows = [
            row
            for sample in samples
            for row in sample["components"]
            if row["service"] == role
        ]
        result.append(
            dict(
                service=role,
                samples=len(rows),
                cpu_mean=round(statistics.mean(row["cpu_percent"] for row in rows), 2),
                cpu_max=max(row["cpu_percent"] for row in rows),
                memory_mean_bytes=round(
                    statistics.mean(row["memory_bytes"] for row in rows)
                ),
                memory_max_bytes=max(row["memory_bytes"] for row in rows),
            )
        )
    return result


def safe_resources(data_root):
    if telemetry.memory()["memory"]["available"] < 512 * 1024**2:
        raise RuntimeError("aborted: less than 512 MiB available memory")
    if shutil.disk_usage(data_root).free < 2 * 1024**3:
        raise RuntimeError("aborted: less than 2 GiB free disk")
    if os.getloadavg()[0] > (os.cpu_count() or 1) * 2:
        raise RuntimeError("aborted: host load exceeds safety threshold")


def scenario(host, path, expected, label, concurrency, seconds, max_rps, data_root):
    safe_resources(data_root)
    start, deadline = time.monotonic(), time.monotonic() + seconds
    lock = threading.Lock()
    times, errors, transferred = [], [], []
    next_slot = [start]
    stop = threading.Event()

    def worker():
        connection = LoopbackHTTPS(host, timeout=3)
        try:
            while not stop.is_set():
                with lock:
                    slot = max(next_slot[0], time.monotonic())
                    next_slot[0] = slot + 1 / max_rps
                if slot >= deadline:
                    break
                if stop.wait(max(0, slot - time.monotonic())):
                    break
                began = time.monotonic()
                try:
                    connection.request("GET", urllib.parse.quote(path, safe="/"))
                    response = connection.getresponse()
                    body = response.read(16 * 1024**2 + 1)
                    if (
                        response.status != 200
                        or len(body) > 16 * 1024**2
                        or hashlib.sha256(body).hexdigest() != expected
                    ):
                        raise ValueError("unexpected response")
                    with lock:
                        times.append((time.monotonic() - began) * 1000)
                        transferred.append(len(body))
                except (OSError, ValueError, http.client.HTTPException):
                    connection.close()
                    connection = LoopbackHTTPS(host, timeout=3)
                    with lock:
                        errors.append(True)
                        if len(errors) >= 5:
                            stop.set()
        finally:
            connection.close()

    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        futures = [pool.submit(worker) for _ in range(concurrency)]
        try:
            while time.monotonic() < deadline and not all(f.done() for f in futures):
                safe_resources(data_root)
                if stop.wait(1):
                    break
        except Exception:
            stop.set()
            raise
        for future in futures:
            future.result()
    elapsed = time.monotonic() - start
    result = dict(
        label=label,
        concurrency=concurrency,
        duration_seconds=round(elapsed, 3),
        max_rps=max_rps,
        requests=len(times) + len(errors),
        errors=len(errors),
        requests_per_second=round((len(times) + len(errors)) / elapsed, 3),
        mib_per_second=round(sum(transferred) / 1024**2 / elapsed, 3),
        p50_ms=percentile(times, 0.5),
        p95_ms=percentile(times, 0.95),
        p99_ms=percentile(times, 0.99),
    )
    if errors:
        raise RuntimeError(
            "benchmark encountered errors; inspect private result before continuing"
        )
    return result


def targets(root, key):
    env = telemetry.environment(root)
    data = Path(env["DATA_ROOT"])
    config = yaml.safe_load((root / "private/repositories.yaml").read_text())
    repo = next(
        r for r in config["repositories"] if r["key"] == key and r.get("enabled")
    )
    manifest = json.loads((data / "publisher/published" / key).read_text())
    image = next(
        row
        for row in manifest["files"]
        if row["path"].startswith("images/") and row["size"] <= 16 * 1024**2
    )
    attachment = next(
        row
        for row in manifest["files"]
        if not row["path"].startswith("images/") and row["size"] <= 16 * 1024**2
    )
    domains = json.loads((root / "private/domains.json").read_text())
    return data, [
        (
            domains["img"],
            repo["mount_path"] + image["path"][7:],
            image["sha256"],
            "public-image",
        ),
        (
            domains["oss"],
            f"/attachments/repositories/{key}/releases/{manifest['sha']}/{attachment['path']}",
            attachment["sha256"],
            "public-attachment",
        ),
    ]


def main():
    parser = argparse.ArgumentParser(
        description="Bounded local acceptance benchmark, not a capacity test"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument("--baseline", action="store_true")
    parser.add_argument("--key")
    parser.add_argument("--seconds", type=int, default=10, choices=range(5, 31))
    parser.add_argument("--max-rps", type=int, default=40, choices=range(1, 101))
    parser.add_argument(
        "--concurrency", type=int, nargs="+", default=[1, 4, 8], choices=[1, 2, 4, 8]
    )
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run on the trusted deployment host as root")
    root = args.root.resolve()
    output = root / "private/performance"
    output.mkdir(mode=0o700, exist_ok=True)
    output.chmod(0o700)
    stop = threading.Event()
    samples = []

    def sampler():
        while not stop.is_set():
            samples.append(resources())
            stop.wait(3)

    thread = threading.Thread(target=sampler)
    thread.start()
    results = []
    try:
        if args.baseline:
            stop.wait(args.seconds)
        else:
            if not args.key:
                parser.error("--key is required for reads")
            data, reads = targets(root, args.key)
            for concurrency in args.concurrency:
                for host, path, expected, label in reads:
                    results.append(
                        scenario(
                            host,
                            path,
                            expected,
                            label,
                            concurrency,
                            args.seconds,
                            args.max_rps,
                            data,
                        )
                    )
    finally:
        stop.set()
        thread.join(timeout=25)
    result = dict(
        collected_at=time.time(),
        mode="baseline" if args.baseline else "bounded-loopback-https",
        cpu_count=os.cpu_count(),
        memory_total_bytes=telemetry.memory()["memory"]["total"],
        scenarios=results,
        resources=summarize_resources(samples),
        samples=samples,
    )
    filename = "baseline.json" if args.baseline else "latest-summary.json"
    destination = output / filename
    destination.write_text(json.dumps(result, indent=2))
    destination.chmod(0o600)
    print(json.dumps({k: v for k, v in result.items() if k != "samples"}, indent=2))


if __name__ == "__main__":
    main()
