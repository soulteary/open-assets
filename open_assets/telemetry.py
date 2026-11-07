"""Privileged host collector; publish an explicit allowlist, never Docker inspect data."""

import argparse
import concurrent.futures
import fcntl
import http.client
import json
import os
import platform
import re
import socket
import ssl
import subprocess
import tempfile
import time
from pathlib import Path

SERVICES = (
    "gitea",
    "nginx",
    "oc",
    "otterio",
    "publisher",
    "stargate",
    "webhook",
    "info",
)


def command(*args):
    return subprocess.check_output(args, stderr=subprocess.DEVNULL, timeout=20).decode()


def environment(root):
    return dict(
        line.split("=", 1)
        for line in (root / ".env").read_text().splitlines()
        if "=" in line and not line.startswith("#")
    )


def memory():
    fields = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        name, value = line.split(":", 1)
        fields[name] = int(value.split()[0]) * 1024
    return {
        "memory": {
            "total": fields["MemTotal"],
            "available": fields["MemAvailable"],
            "used": fields["MemTotal"] - fields["MemAvailable"],
        },
        "swap": {
            "total": fields["SwapTotal"],
            "used": fields["SwapTotal"] - fields["SwapFree"],
        },
    }


def cpu_sample():
    fields = [
        int(n) for n in Path("/proc/stat").read_text().splitlines()[0].split()[1:9]
    ]
    return sum(fields), fields[3] + fields[4]


def size_bytes(value):
    number, unit = re.fullmatch(r"([\d.]+)\s*([A-Za-z]+)", value.strip()).groups()
    factors = {
        "B": 1,
        "kB": 1000,
        "MB": 1000**2,
        "GB": 1000**3,
        "KiB": 1024,
        "MiB": 1024**2,
        "GiB": 1024**3,
        "TiB": 1024**4,
    }
    return round(float(number) * factors[unit])


def disk(path, label):
    v = os.statvfs(path)
    total, free, available = (
        v.f_blocks * v.f_frsize,
        v.f_bfree * v.f_frsize,
        v.f_bavail * v.f_frsize,
    )
    return dict(
        label=label,
        total=total,
        available=available,
        used=total - free,
        used_percent=round((total - free) / total * 100, 2),
        inode_used_percent=round((v.f_files - v.f_ffree) / v.f_files * 100, 2)
        if v.f_files
        else None,
    )


def filesystems(data_root):
    """Deduplicate mounted filesystems and use the actual image volume for reserve."""
    disks, devices = [], {}
    data_device = data_root.stat().st_dev
    system_device = os.stat("/").st_dev
    locations = [
        (
            Path("/"),
            "系统 / 数据（同一文件系统）" if data_device == system_device else "系统",
        ),
        (data_root, "数据"),
        (data_root / "images", "图片存储"),
        (data_root / "publisher", "Git / 发布状态"),
        (data_root / "otterio", "对象存储"),
    ]
    image_device = data_device
    for path, label in locations:
        try:
            device = path.stat().st_dev
        except FileNotFoundError:
            continue
        if path == data_root / "images":
            image_device = device
        if device not in devices:
            devices[device] = len(disks)
            disks.append(disk(path, label))
    return disks, devices[image_device]


def probe(address, port, path="/healthz", hostname=None, ca=None):
    started = time.monotonic()
    connection = socket.create_connection((address, port), timeout=2)
    try:
        if hostname:
            context = ssl.create_default_context(cafile=str(ca) if ca else None)
            connection = context.wrap_socket(connection, server_hostname=hostname)
        connection.sendall(
            f"GET {path} HTTP/1.1\r\nHost: {hostname or 'localhost'}\r\nConnection: close\r\n\r\n".encode()
        )
        response = http.client.HTTPResponse(connection)
        response.begin()
        return dict(
            status="ok" if response.status == 200 else "http-error",
            latency_ms=round((time.monotonic() - started) * 1000, 2),
        )
    finally:
        connection.close()


def certificate(label, pem=None, address=None, hostname=None, tls_context=None):
    try:
        if pem is not None:
            dates = subprocess.run(
                ["openssl", "x509", "-noout", "-startdate", "-enddate"],
                input=pem,
                capture_output=True,
                check=True,
                timeout=5,
            ).stdout.decode()
            values = dict(line.split("=", 1) for line in dates.splitlines())
            start, end = (
                ssl.cert_time_to_seconds(values["notBefore"]),
                ssl.cert_time_to_seconds(values["notAfter"]),
            )
        else:
            context = tls_context or ssl.create_default_context()
            with socket.create_connection((address, 443), timeout=3) as sock:
                with context.wrap_socket(sock, server_hostname=hostname) as tls:
                    cert = tls.getpeercert()
            start, end = (
                ssl.cert_time_to_seconds(cert["notBefore"]),
                ssl.cert_time_to_seconds(cert["notAfter"]),
            )
        return dict(
            label=label,
            not_before=start,
            not_after=end,
            days_remaining=round((end - time.time()) / 86400, 3),
        )
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        return dict(label=label, error="unavailable")


def collect(root, refresh_storage=False):
    started = time.monotonic()
    env, domains = (
        environment(root),
        json.loads((root / "private/domains.json").read_text()),
    )
    first = cpu_sample()
    ids = command(
        "docker", "compose", "--project-directory", str(root), "ps", "-aq"
    ).split()
    external = command(
        "docker", "ps", "-aq", "--filter", "label=com.docker.compose.service=traefik"
    ).split()
    ids = list(dict.fromkeys(ids + external))
    inspections = json.loads(command("docker", "inspect", *ids)) if ids else []
    rows = {}
    for raw in inspections:
        role = raw["Config"].get("Labels", {}).get("com.docker.compose.service")
        if role not in SERVICES + ("traefik",):
            continue
        rows[raw["Id"]] = dict(
            service=role,
            image=raw["Config"]["Image"],
            state=raw["State"]["Status"],
            health=raw["State"].get("Health", {}).get("Status", "not-configured"),
            probe="not-probed",
            restarts=raw["RestartCount"],
            cpu_percent=None,
            memory_bytes=None,
        )
    active = [r["Id"] for r in inspections if r["Id"] in rows and r["State"]["Running"]]
    if active:
        for line in command(
            "docker", "stats", "--no-stream", "--format", "{{json .}}", *active
        ).splitlines():
            stats = json.loads(line)
            match = next(
                (r for key, r in rows.items() if key.startswith(stats["ID"])), None
            )
            if match:
                match.update(
                    cpu_percent=float(stats["CPUPerc"].rstrip("%")),
                    memory_bytes=size_bytes(stats["MemUsage"].split("/")[0]),
                )
    ca = root / "private/tls/ca.crt"
    for raw in inspections:
        if raw["Id"] not in rows or not raw["State"]["Running"]:
            continue
        row = rows[raw["Id"]]
        ip = next(
            (
                n["IPAddress"]
                for n in raw["NetworkSettings"]["Networks"].values()
                if n["IPAddress"]
            ),
            None,
        )
        edge_ip = (
            raw["NetworkSettings"]["Networks"]
            .get("open-assets-edge", {})
            .get("IPAddress", ip)
        )
        target = {
            "gitea": ("127.0.0.1", 13000, "/api/healthz", None, None),
            "stargate": ("127.0.0.1", 18080, "/healthz", None, None),
            "webhook": (ip, 9000, "/healthz", None, None),
            "nginx": (edge_ip, 8081, "/__health", None, None),
            "otterio": (
                "127.0.0.1",
                19000,
                "/otterio/health/ready",
                "storage.internal",
                ca,
            ),
            "oc": (edge_ip, 9090, "/", domains["storage"], ca),
            "info": (edge_ip, 8082, "/healthz", None, None),
        }.get(row["service"])
        if target:
            try:
                result = probe(*target)
                row.update(
                    probe=result["status"], probe_latency_ms=result["latency_ms"]
                )
            except (OSError, http.client.HTTPException):
                row["probe"] = "failed"
    components = list(rows.values())
    for service in SERVICES + ("traefik",):
        if not any(r["service"] == service for r in components):
            components.append(
                dict(
                    service=service,
                    state="missing",
                    health="missing",
                    probe="unavailable",
                    restarts=0,
                )
            )
    components.sort(key=lambda r: r["service"])
    data_root = Path(env["DATA_ROOT"])
    for row in components:
        if row["service"] == "publisher" and row["state"] == "running":
            try:
                from .health import read_worker_heartbeat, worker_healthy

                beat = read_worker_heartbeat(
                    data_root / "publisher/worker-heartbeat.json"
                )
                row["probe"] = (
                    "ok"
                    if worker_healthy(
                        beat,
                        time.time(),
                        int(env.get("WORKER_MAX_JOB_SECONDS", "1800")),
                    )
                    else "failed"
                )
            except (OSError, ValueError, KeyError, TypeError):
                row["probe"] = "failed"
    disks, image_disk_index = filesystems(data_root)
    certificates = []
    for name, label in [
        ("storage/public.crt", "内部 / OtterIO"),
        ("console/console.crt", "内部 / OC"),
    ]:
        certificates.append(
            certificate(label, pem=(root / "private/tls" / name).read_bytes())
        )
    issuers = sorted((root / "private/tls/issuers").glob("*/root.crt"))
    for path in issuers:
        certificates.append(
            certificate("内部 CA / " + path.parent.name, pem=path.read_bytes())
        )
    if not issuers:
        certificates.append(certificate("内部 CA", pem=ca.read_bytes()))
    public_context = ssl.create_default_context()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        certificates += list(
            pool.map(
                lambda item: certificate(
                    "公网 / " + item[0],
                    address="127.0.0.1",
                    hostname=item[1],
                    tls_context=public_context,
                ),
                sorted(domains.items()),
            )
        )
    second = cpu_sample()
    cpu = 100 * (1 - (second[1] - first[1]) / max(1, second[0] - first[0]))
    os_name = next(
        (
            line.split("=", 1)[1].strip('"')
            for line in Path("/etc/os-release").read_text().splitlines()
            if line.startswith("PRETTY_NAME=")
        ),
        "Linux",
    )
    result = dict(
        schema_version=1,
        collected_at=time.time(),
        host=dict(
            cpu_count=os.cpu_count(),
            cpu_percent=round(max(0, cpu), 2),
            load=list(os.getloadavg()),
            uptime_seconds=float(Path("/proc/uptime").read_text().split()[0]),
            os=os_name,
            kernel=platform.release(),
            **memory(),
        ),
        components=components,
        disks=disks,
        certificates=certificates,
        collection_seconds=round(time.monotonic() - started, 3),
    )
    from .storage_info import collect_storage, cleanup_service, capacity

    result["storage"] = collect_storage(root, data_root, force=refresh_storage)
    result["storage"]["cleanup_service"] = cleanup_service()
    from .object_info import summary

    result["object_storage"] = summary(root, data_root)
    result["capacity"] = capacity(disks, result["storage"], env, image_disk_index)
    result["collection_seconds"] = round(time.monotonic() - started, 3)
    performance = root / "private/performance/latest-summary.json"
    if performance.exists():
        source = json.loads(performance.read_text())
        fields = (
            "label",
            "concurrency",
            "requests",
            "errors",
            "requests_per_second",
            "p50_ms",
            "p95_ms",
            "p99_ms",
        )
        result["performance"] = dict(
            collected_at=source["collected_at"],
            scenarios=[{k: row[k] for k in fields} for row in source["scenarios"]],
        )
    repair_path = data_root / "repair/state.json"
    if repair_path.exists():
        try:
            records = json.loads(repair_path.read_text()).get("events", [])
            result["repairs"] = [
                {k: row[k] for k in ("service", "time", "reason", "result")}
                for row in reversed(records[-10:])
            ]
        except (ValueError, KeyError, TypeError):
            result["repair_status"] = "unavailable"
    return result


def write_snapshot(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    os.chown(path.parent, 0, 20010)
    path.parent.chmod(0o750)
    fd, tmp = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(data, stream, ensure_ascii=False)
            stream.flush()
            os.fsync(stream.fileno())
        os.chown(tmp, 0, 20010)
        os.chmod(tmp, 0o640)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def main():
    parser = argparse.ArgumentParser(
        description="Collect a filtered host status snapshot"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument(
        "--refresh-storage",
        action="store_true",
        help="refresh bounded repository disk measurements",
    )
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run the host collector as root")
    root = args.root.resolve()
    output = Path(environment(root)["DATA_ROOT"]) / "info/snapshot.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    with (output.parent / ".collector.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        write_snapshot(output, collect(root, args.refresh_storage))
    print("Filtered status snapshot updated")


if __name__ == "__main__":
    main()
