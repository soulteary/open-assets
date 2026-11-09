"""Command-specific arguments shared by container and host entry points."""

import argparse
import re

INTERNAL = {"render", "serve", "worker", "once"}
DESCRIPTIONS = {
    "materialize-image-views": "校验并重建旧目录格式，供旧程序回退；默认预览。",
    "compact-objects": "完整验证并合并历史相同对象；默认预览，需清理身份。",
    "deduplicate-local": "完整校验后去重本地相同内容；默认预览，--apply 才执行。",
    "validate": "校验配置，不修改服务。",
    "render": "生成 Nginx 配置（内部部署命令）。",
    "serve": "启动 Webhook（由 Compose 管理）。",
    "worker": "启动发布循环（由 Compose 管理）。",
    "once": "执行一轮队列处理（内部命令）。",
    "status": "查询发布任务；published 才表示该任务完成。",
    "publish": "提交指定版本的发布任务；执行成功后会改变当前图片版本。",
    "retry": "重试失败任务；不适用于已经 published 的去重记录。",
    "rollback-images": "校验已有本地副本后切换图片版本，不从 Git 重建。",
    "restore-objects": "从 Git 恢复对象；默认不重建本地副本、不切换版本。",
    "check-restore": "重新核对对象、本地副本与引用，只写核对报告。",
    "close-restore": "同锁重新核对后显式结案，保留原报告，不切换版本。",
    "restore-audit-list": "按仓库和可选 SHA 查询在线及归档审计。",
    "restore-audit-archive": "默认只预览；--apply 才校验并移出在线目录。",
    "restore-audit-verify": "验证归档及待清理暂存，不修复或切换版本。",
}


def key(value):
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", value):
        raise argparse.ArgumentTypeError(
            "key 应为 1–48 位小写字母、数字或连字符，以字母开头；先在宿主机用 scripts/open-assets repo list 查询。"
        )
    return value


def sha(value):
    if not re.fullmatch(r"[0-9a-f]{40}", value) or value == "0" * 40:
        raise argparse.ArgumentTypeError(
            "需要完整的 40 位小写 commit SHA；可在内容仓库运行 git rev-parse HEAD。"
        )
    return value


def job(value):
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise argparse.ArgumentTypeError("需要 status 输出的完整 64 位任务 ID。")
    return value


def reason(value):
    if not 1 <= len(value.strip()) <= 256 or any(
        ord(c) < 32 or ord(c) == 127 for c in value
    ):
        raise argparse.ArgumentTypeError(
            "结案原因需为 1–256 字符，不能包含控制字符；不要填写凭据。"
        )
    return value.strip()


def bounded(low, high):
    def parse(value):
        try:
            result = int(value)
        except ValueError:
            raise argparse.ArgumentTypeError("需要整数。") from None
        if not low <= result <= high:
            raise argparse.ArgumentTypeError(f"范围应为 {low}–{high}。")
        return result

    return parse


def add_worker_commands(parser, *, internal=True):
    commands = parser.add_subparsers(dest="command", required=True)
    parser.set_defaults(
        with_local=False,
        rollback=False,
        reason=None,
        apply=False,
        keep_last=None,
        min_age_days=None,
        max_records=None,
        limit=None,
        offset=None,
        status_key=None,
        status_sha=None,
    )
    for name, description in DESCRIPTIONS.items():
        if not internal and name in INTERNAL:
            continue
        command = commands.add_parser(name, help=description, description=description)
        if name in (
            "publish",
            "rollback-images",
            "restore-objects",
            "check-restore",
            "close-restore",
        ):
            command.add_argument(
                "key", type=key, help="稳定仓库 key，不是 owner/repository 名称"
            )
            command.add_argument("sha", type=sha, help="完整 commit SHA")
        if name in ("deduplicate-local", "materialize-image-views", "compact-objects"):
            command.add_argument("--apply", action="store_true")
        if name == "render":
            command.add_argument("destination", help="配置输出目录")
        if name == "retry":
            command.add_argument("job_id", type=job, help="失败任务 ID")
        if name == "restore-objects":
            command.add_argument(
                "--with-local",
                action="store_true",
                help="同时重建并校验 public 和图片副本，仍不切换",
            )
            command.add_argument(
                "--rollback",
                action="store_true",
                help="明确切换到恢复版本，必须同时指定 --with-local",
            )
        if name == "close-restore":
            command.add_argument(
                "--reason", required=True, type=reason, help="私有审计中的人工结案原因"
            )
        if name.startswith("restore-audit-"):
            command.add_argument("key", type=key, help="仓库 key，已禁用仓库也可查询")
            command.add_argument("sha", nargs="?", type=sha, help="可选完整 SHA")
            if name == "restore-audit-archive":
                command.add_argument(
                    "--apply", action="store_true", help="实际归档；省略时只预览"
                )
                command.add_argument(
                    "--keep-last",
                    type=bounded(1, 1000),
                    help="保留最近条数，默认 20，至少 1",
                )
                command.add_argument(
                    "--min-age-days",
                    type=bounded(0, 36500),
                    help="最短完成天数，默认 30",
                )
                command.add_argument(
                    "--max-records", type=bounded(1, 500), help="单轮上限，默认 100"
                )
            else:
                command.add_argument(
                    "--limit",
                    type=bounded(1, 1000),
                    help="返回条数，默认 100；不跳过校验",
                )
                command.add_argument(
                    "--offset", type=bounded(0, 1000000), help="分页偏移，默认 0"
                )
        if name == "status":
            command.add_argument(
                "scope",
                nargs="?",
                choices=("active", "all", "archived"),
                default="active",
                help="活动目录、全部或已归档任务",
            )
            command.add_argument(
                "--key", dest="status_key", type=key, help="按仓库 key 筛选"
            )
            command.add_argument(
                "--sha", dest="status_sha", type=sha, help="按完整 commit SHA 筛选"
            )
    return commands


def normalize(parser, options):
    if options.rollback and not options.with_local:
        parser.error("--rollback 必须与 --with-local 同时使用。")
    name = options.command
    options.args = []
    if name in (
        "publish",
        "rollback-images",
        "restore-objects",
        "check-restore",
        "close-restore",
    ) or name.startswith("restore-audit-"):
        options.args = [options.key] + ([options.sha] if options.sha else [])
    elif name == "retry":
        options.args = [options.job_id]
    elif name == "render":
        options.args = [options.destination]
    elif name == "status":
        options.args = [] if options.scope == "active" else [options.scope]
    return options


def parse(argv=None):
    parser = argparse.ArgumentParser(
        description="Open Assets 发布与恢复工具；每个子命令可使用 --help。"
    )
    add_worker_commands(parser)
    return parser, normalize(parser, parser.parse_args(argv))


def forward(options):
    result = [options.command, *options.args]
    for field, flag in (
        ("with_local", "--with-local"),
        ("rollback", "--rollback"),
        ("apply", "--apply"),
    ):
        if getattr(options, field, False):
            result.append(flag)
    for field, flag in (
        ("reason", "--reason"),
        ("keep_last", "--keep-last"),
        ("min_age_days", "--min-age-days"),
        ("max_records", "--max-records"),
        ("limit", "--limit"),
        ("offset", "--offset"),
        ("status_key", "--key"),
        ("status_sha", "--sha"),
    ):
        value = getattr(options, field, None)
        if value is not None:
            result.extend([flag, str(value)])
    return result
