"""Bounded host-only restarts for persistently unhealthy, explicitly scoped services."""

import argparse
import copy
import fcntl
import json
import os
from pathlib import Path
import subprocess
import time

from .telemetry import environment

APP_SERVICES = {
    "webhook",
    "publisher",
    "info",
    "object-reader",
    "nginx",
    "stargate",
    "gitea",
    "oc",
    "otterio",
}


def eligible(inspections, traefik_project):
    rows = []
    for raw in inspections:
        labels = raw["Config"].get("Labels", {})
        project, service = (
            labels.get("com.docker.compose.project"),
            labels.get("com.docker.compose.service"),
        )
        if not (
            (project == "asset-stack" and service in APP_SERVICES)
            or (traefik_project and project == traefik_project and service == "traefik")
        ):
            continue
        state = raw["State"]
        rows.append(
            dict(
                id=raw["Id"],
                service=service,
                scope=f"{project}/{service}",
                running=state.get("Running", False),
                paused=state.get("Paused", False),
                health=state.get("Health", {}).get("Status"),
            )
        )
    return rows


def plan(rows, state, now):
    state = copy.deepcopy(state)
    seen = state.setdefault("unhealthy", {})
    events = state.setdefault("events", [])
    actions = []
    recent = [
        e
        for e in events
        if e.get("action") == "restart" and e.get("result") != "skipped"
    ]
    for row in rows:
        key = row["scope"]
        if not row["running"] or row["paused"] or row["health"] != "unhealthy":
            seen.pop(key, None)
            continue
        first = seen.get(key)
        if not first or first["id"] != row["id"]:
            seen[key] = dict(id=row["id"], since=now)
            continue
        if now - first["since"] < 30:
            continue
        own = [e for e in recent if e["scope"] == key]
        if own and now - max(e["time"] for e in own) < 600:
            continue
        if sum(now - e["time"] < 3600 for e in own) >= 3:
            continue
        if sum(now - e["time"] < 900 for e in recent) + len(actions) >= 3:
            continue
        if len(actions) < 2:
            actions.append(row)
    return state, actions


def save(path, state):
    temporary = path.with_suffix(".new")
    with temporary.open("w") as stream:
        json.dump(state, stream)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.chmod(0o600)
    os.replace(temporary, path)


def docker(*args):
    result = subprocess.run(
        ["docker", *args], capture_output=True, timeout=35, check=False
    )
    if result.returncode:
        raise RuntimeError("Docker health-repair operation failed")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(
        description="Host-only bounded container health repair"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument(
        "--apply",
        action="store_true",
        help="allow bounded restarts; otherwise report only",
    )
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run on the trusted host as root")
    root = args.root.resolve()
    env = environment(root)
    folder = Path(env["DATA_ROOT"]) / "repair"
    folder.mkdir(mode=0o700, exist_ok=True)
    folder.chmod(0o700)
    with (folder / ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if (root / "private/maintenance.flag").exists():
            print("Health repair suppressed by maintenance flag")
            return
        ids = (
            docker(
                "ps", "-aq", "--filter", "label=com.docker.compose.project=asset-stack"
            )
            .decode()
            .split()
        )
        if env.get("TRAEFIK_PROJECT"):
            ids += (
                docker(
                    "ps",
                    "-aq",
                    "--filter",
                    "label=com.docker.compose.project=" + env["TRAEFIK_PROJECT"],
                    "--filter",
                    "label=com.docker.compose.service=traefik",
                )
                .decode()
                .split()
            )
        ids = list(dict.fromkeys(ids))
        inspections = json.loads(docker("inspect", *ids)) if ids else []
        rows = eligible(inspections, env.get("TRAEFIK_PROJECT", ""))
        path = folder / "state.json"
        state = json.loads(path.read_text()) if path.exists() else {}
        state, actions = plan(rows, state, time.time())
        if args.apply:
            save(path, state)
            for row in actions:
                if (root / "private/maintenance.flag").exists():
                    break
                event = dict(
                    service=row["service"],
                    scope=row["scope"],
                    time=time.time(),
                    action="restart",
                    reason="persistent-unhealthy",
                    result="attempt",
                )
                state["events"].append(event)
                state["events"] = state["events"][-100:]
                save(path, state)
                try:
                    current = eligible(
                        json.loads(docker("inspect", row["id"])),
                        env.get("TRAEFIK_PROJECT", ""),
                    )
                    if (
                        not current
                        or not current[0]["running"]
                        or current[0]["paused"]
                        or current[0]["health"] != "unhealthy"
                    ):
                        event["result"] = "skipped"
                    else:
                        docker("restart", "--time", "10", row["id"])
                        event["result"] = "restarted"
                except (RuntimeError, subprocess.SubprocessError):
                    event["result"] = "failed"
                save(path, state)
        print(
            json.dumps(
                dict(
                    checked=len(rows),
                    apply=args.apply,
                    planned=[row["service"] for row in actions],
                )
            )
        )


if __name__ == "__main__":
    main()
