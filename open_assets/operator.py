"""Host entry point for read-only discovery and explicit existing operations."""

import argparse
import json
import os
import re
import subprocess
import sys
import time

from . import diagnostics
from pathlib import Path, PurePosixPath
from urllib.parse import quote

import yaml

from . import __main__ as core, cli
from .info import read_snapshot
from .storage_info import read_json

SERVICES = (
    "webhook",
    "publisher",
    "info",
    "nginx",
    "otterio",
    "oc",
    "stargate",
    "gitea",
)
TIMERS = tuple(diagnostics.TIMERS)


def parser():
    p = argparse.ArgumentParser(
        description="Open Assets 运维入口：先用 repo list 查 key，doctor 查看部署，再执行具体操作。",
        epilog="发布会改变当前图片；恢复默认不切换；审计归档默认预览。每个子命令均可使用 --help。",
    )
    p.add_argument(
        "--root",
        type=Path,
        default=Path("/app/open-assets"),
        help="部署目录；放在子命令前，默认 /app/open-assets",
    )
    commands = cli.add_worker_commands(p, internal=False)
    repo = commands.add_parser("repo", help="查询或接入仓库").add_subparsers(
        dest="operation", required=True
    )
    listing = repo.add_parser(
        "list", help="只读列出 key、仓库名、分支和图片前缀，无需 Gitea 管理密码"
    )
    listing.add_argument("--json", action="store_true", help="JSON 输出")
    ensure = repo.add_parser(
        "ensure", help="创建或收敛 Gitea/Deploy Key/Webhook/IAM/Nginx 配置"
    )
    ensure.add_argument("name", help="owner/repository")
    ensure.add_argument("--key", required=True, type=cli.key)
    ensure.add_argument("--mount-path", required=True)
    ensure.add_argument("--branch", default="main")
    commands.add_parser("objects", help="配置专用对象清理身份").add_subparsers(
        dest="operation", required=True
    ).add_parser("setup")
    doctor = commands.add_parser(
        "doctor", help="只读检查配置、容器、定时任务与快照，不修复或重启"
    )
    doctor.add_argument("--json", action="store_true")
    ack = commands.add_parser(
        "acknowledge-closure",
        help="预览人工确认结案拒绝；--confirm TOKEN 后保留证据并记录确认，不替恢复结案",
    )
    ack.add_argument("key", type=cli.key)
    ack.add_argument("audit_id", type=audit_id)
    ack.add_argument("--reason", required=True, type=cli.reason)
    ack.add_argument(
        "--confirm", type=evidence_token, help="使用本次预览的64位证据令牌显式确认"
    )
    links = commands.add_parser(
        "links", help="只读生成图片/版本附件 URL，正确编码中文、空格、加号和百分号"
    )
    links.add_argument("key", type=cli.key)
    links.add_argument("sha", type=cli.sha)
    links.add_argument("path", help="相对 public/ 的文件路径，也接受配置的源目录前缀")
    links.add_argument("--json", action="store_true")
    return p


def audit_id(value):
    if not re.fullmatch(r"[0-9a-f]{32}", value):
        raise argparse.ArgumentTypeError(
            "需要 restore-audit-list 输出的32位结案审计 ID。"
        )
    return value


def evidence_token(value):
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise argparse.ArgumentTypeError("需要本次 preview 输出的完整64位证据令牌。")
    return value


def configured(root):
    return core.load(root / "private/repositories.yaml")


def repositories(root):
    c = configured(root)
    return [
        dict(
            key=r["key"],
            name=r.get("name", "—"),
            branch=r["branch"],
            mount_path=r["mount_path"],
            enabled=r.get("enabled") is True,
            serve=r.get("serve", True) is True,
        )
        for r in c["repositories"]
    ]


def hostname(value):
    if (
        not isinstance(value, str)
        or len(value) > 253
        or len(value.split(".")) < 2
        or any(
            not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", part)
            for part in value.split(".")
        )
    ):
        raise ValueError("invalid configured domain")
    return value


def links(root, key, sha, name):
    c = configured(root)
    repository = next((r for r in c["repositories"] if r["key"] == key), None)
    if repository is None:
        raise ValueError("unknown repository key")
    prefix = repository["source_directory"].rstrip("/") + "/"
    if name.startswith(prefix):
        name = name[len(prefix) :]
    path = PurePosixPath(name)
    if (
        not name
        or path.is_absolute()
        or path.as_posix() != name
        or any(part.startswith(".") for part in path.parts)
        or any(ord(ch) < 32 or ord(ch) == 127 for ch in name)
    ):
        raise ValueError("invalid relative public path")
    domains = read_json(root / "private/domains.json", 16384)
    bucket = c["bucket"]
    if not isinstance(bucket, str) or not re.fullmatch(
        r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", bucket
    ):
        raise ValueError("invalid configured bucket")
    result = dict(
        key=key,
        sha=sha,
        verified=False,
        attachment="https://"
        + hostname(domains["oss"])
        + "/"
        + quote(
            bucket + "/repositories/" + key + "/releases/" + sha + "/" + name, safe="/"
        ),
        image=None,
    )
    if name.startswith("images/") and repository.get("serve", True):
        result["image"] = (
            "https://"
            + hostname(domains["img"])
            + repository["mount_path"]
            + quote(name[len("images/") :], safe="/")
        )
    return result


def inspect_command(args, *, root=None):
    value = subprocess.run(args, cwd=root, capture_output=True, text=True, timeout=20)
    if value.returncode or len(value.stdout) > 2097152:
        raise OSError("diagnostic command unavailable")
    return value.stdout


def doctor(root):
    checks = []
    config = None

    def add(name, state, hint):
        checks.append(dict(check=name, status=state, hint=hint))

    try:
        config = configured(root)
        add("configuration", "ok", "配置结构和策略校验通过。")
    except (OSError, ValueError, TypeError, KeyError, AttributeError, yaml.YAMLError):
        add(
            "configuration",
            "attention",
            "检查 private/repositories.yaml 与文件权限；不要重跑初始化重置凭据。",
        )
    if (root / "private/maintenance.flag").exists():
        add(
            "maintenance",
            "attention",
            "维护标记存在，部分任务会被跳过；先确认维护是否结束，再按维护流程恢复，勿仅为通过检查删除标记。",
        )
    components = []
    cache_rows = []
    try:
        raw = inspect_command(
            [
                "docker",
                "compose",
                "--project-directory",
                str(root),
                "ps",
                "--all",
                "--format",
                "json",
            ],
            root=root,
        ).strip()
        parsed = (
            json.loads(raw)
            if raw.startswith("[")
            else [json.loads(line) for line in raw.splitlines()]
        )
        if not isinstance(parsed, list):
            raise ValueError("invalid compose status")
        for name in SERVICES:
            matches = [v for v in parsed if v.get("Service") == name]
            if len(matches) != 1:
                components.append(dict(service=name, state="missing", health="unknown"))
                continue
            value = matches[0]
            components.append(
                dict(
                    service=name,
                    state=str(value.get("State", "unknown"))[:32],
                    health=str(value.get("Health", "unknown"))[:32],
                )
            )
        ok = all(
            c["state"] == "running" and c["health"] == "healthy" for c in components
        )
        add(
            "containers",
            "ok" if ok else "attention",
            "仅核对本项目八个容器；现有外部 Traefik 仍需单独检查。",
        )
    except (
        OSError,
        ValueError,
        KeyError,
        TypeError,
        AttributeError,
        subprocess.SubprocessError,
    ):
        add(
            "containers",
            "attention",
            "确认 Docker 可用、Compose 项目目录正确，且当前用户有读取权限。",
        )
    try:
        uptime = float(Path("/proc/uptime").read_text().split()[0])
    except (OSError, ValueError):
        uptime = 0
    timer_rows = [
        diagnostics.timer(root, name, inspect_command, uptime) for name in TIMERS
    ]
    add(
        "timers",
        "attention" if any(t["status"] == "attention" for t in timer_rows) else "ok",
        "必需：Info、对象/审计/资源采集、日志轮转；可选：自动修复、本地/对象保留。详见下方逐项结果。",
    )
    try:
        from .telemetry import environment

        data = Path(environment(root)["DATA_ROOT"])
        snapshot = read_snapshot(data / "info/snapshot.json")
        snapshot["stale"] = (
            snapshot["stale"] or snapshot["collected_at"] > time.time() + 5
        )
        add(
            "info-snapshot",
            "attention" if snapshot["stale"] else "ok",
            "主机快照年龄 "
            + str(snapshot["age_seconds"])
            + " 秒；不代表低频缓存也刚更新。"
            + (
                "查看 sudo journalctl -u open-assets-info.service -n 50 --no-pager，修复后 sudo systemctl start open-assets-info.service。"
                if snapshot["stale"]
                else ""
            ),
        )
    except (OSError, ValueError, KeyError, TypeError):
        add(
            "info-snapshot",
            "attention",
            "检查 DATA_ROOT 与快照权限，查看 sudo journalctl -u open-assets-info.service -n 50 --no-pager；修复后 sudo systemctl start open-assets-info.service。",
        )
    try:
        from .telemetry import environment

        cache_rows = diagnostics.caches(
            root, Path(environment(root)["DATA_ROOT"]), config
        )
        add(
            "caches",
            "attention" if any(r["status"] != "ok" for r in cache_rows) else "ok",
            "分别检查本地版本、对象、审计、Git/队列缓存；只读固定文件，不扫描目录或访问 S3。",
        )
    except (OSError, ValueError, KeyError, TypeError):
        add("caches", "attention", "先修复配置与 DATA_ROOT，再重新运行 doctor。")
    return dict(
        status="ok" if all(c["status"] == "ok" for c in checks) else "attention",
        checks=checks,
        components=components,
        timers=timer_rows,
        caches=cache_rows,
    )


def main(argv=None):
    p = parser()
    options = cli.normalize(p, p.parse_args(argv))
    root = options.root.resolve()
    try:
        if options.command == "acknowledge-closure":
            from .closure_ack import run

            if os.geteuid() != 0:
                raise ValueError("run host acknowledgment as root")
            result = run(
                root, options.key, options.audit_id, options.reason, options.confirm
            )
            print(json.dumps(result, ensure_ascii=False))
            return (
                2
                if result["status"] == "busy"
                else 0
                if result["status"] in ("preview", "acknowledged")
                else 1
            )
        if options.command in ("repo", "objects") and not (
            options.command == "repo" and options.operation == "list"
        ):
            from .admin import main as administrative

            args = ["--root", str(root), options.command, options.operation]
            if options.command == "repo":
                args += [
                    options.name,
                    "--key",
                    options.key,
                    "--mount-path",
                    options.mount_path,
                    "--branch",
                    options.branch,
                ]
            return administrative(args)
        if options.command == "repo":
            result = repositories(root)
            if options.json:
                print(json.dumps(dict(repositories=result), ensure_ascii=False))
            else:
                for r in result:
                    print(
                        r["key"],
                        r["name"],
                        "分支=" + r["branch"],
                        "图片前缀=" + r["mount_path"],
                        "启用" if r["enabled"] else "已禁用",
                        "提供图片" if r["serve"] else "不提供图片",
                    )
            return 0
        if options.command == "links":
            result = links(root, options.key, options.sha, options.path)
            if options.json:
                print(json.dumps(result, ensure_ascii=False))
            else:
                if result["image"]:
                    print("图片（当前版本）：" + result["image"])
                print("附件（指定版本）：" + result["attachment"])
                print("仅按配置生成，未验证对象存在。图片固定链接可能已指向其他提交。")
            return 0
        if options.command == "doctor":
            result = doctor(root)
            if options.json:
                print(json.dumps(result, ensure_ascii=False))
            else:
                for value in result["checks"]:
                    print(
                        ("通过" if value["status"] == "ok" else "待处理")
                        + " · "
                        + value["check"]
                        + "："
                        + value["hint"]
                    )
                for value in result["components"]:
                    if value["state"] != "running" or value["health"] != "healthy":
                        print(
                            "  容器 "
                            + value["service"]
                            + "："
                            + value["state"]
                            + " / "
                            + value["health"]
                        )
                for value in result["timers"]:
                    print(
                        "  定时任务 "
                        + value["timer"]
                        + ("（必需）" if value["required"] else "（可选）")
                        + "："
                        + value["reason"]
                        + "；"
                        + value["hint"]
                    )
                for value in result["caches"]:
                    age = value.get("age_seconds")
                    print(
                        "  缓存 "
                        + value["cache"]
                        + (" / " + value["key"] if value.get("key") else "")
                        + "："
                        + value["reason"]
                        + ("，采样年龄 " + str(age) + " 秒" if age is not None else "")
                        + "；"
                        + value["hint"]
                    )
            return 0 if result["status"] == "ok" else 1
        # Explicit allowlist from the shared parser; never invoke a shell or arbitrary container commands.
        if options.command not in cli.DESCRIPTIONS or options.command in cli.INTERNAL:
            p.error("unsupported operator command")
        return subprocess.run(
            [
                "docker",
                "compose",
                "--project-directory",
                str(root),
                "exec",
                "-T",
                "publisher",
                "python",
                "-m",
                "open_assets",
                *cli.forward(options),
            ],
            cwd=root,
        ).returncode
    except KeyboardInterrupt:
        print(
            "命令已中断；远端操作可能仍在运行。先查询任务/核对报告或审计位置，再决定重试。",
            file=sys.stderr,
        )
        return 130
    except (
        OSError,
        ValueError,
        TypeError,
        KeyError,
        AttributeError,
        subprocess.SubprocessError,
        yaml.YAMLError,
    ) as error:
        print(
            json.dumps(
                dict(
                    status="failed",
                    error=type(error).__name__,
                    hint=(
                        "请用 sudo 在宿主机运行；先用 restore-audit-list 核对最新在线 refused 审计 ID，再检查原报告、确认指针及备份。不要重跑初始化或手工改写指针。"
                        if options.command == "acknowledge-closure"
                        else "确认 --root 指向已初始化部署；先用 doctor 检查。原始异常和凭据不输出。"
                    ),
                ),
                ensure_ascii=False,
            ),
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    sys.exit(main())
