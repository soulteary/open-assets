"""Host-only reconciliation of the separate cleaner identity and IAM policy."""

import json
import os
import re
import secrets


from .admin import write_private
from .object_info import scope
from .object_retention import policy, prefix
from .retention import read_json, real_directory


def iam_policy(config, compaction=False):
    statements = [
        dict(
            Effect="Allow",
            Action=["s3:GetBucketVersioning"],
            Resource=[f"arn:aws:s3:::{config['bucket']}"],
        ),
        dict(
            Effect="Allow",
            Action=["s3:ListBucket"],
            Resource=[f"arn:aws:s3:::{config['bucket']}"],
            Condition={
                "StringLike": {
                    "s3:prefix": [
                        prefix(r["key"]) + "*" for r in config["repositories"]
                    ]
                }
            },
        ),
        dict(
            Effect="Allow",
            Action=["s3:GetObject"],
            Resource=[
                f"arn:aws:s3:::{config['bucket']}/" + prefix(r["key"]) + "*"
                for r in config["repositories"]
            ],
        ),
    ]
    writable = [
        f"arn:aws:s3:::{config['bucket']}/" + prefix(r["key"]) + "*"
        for r in config["repositories"]
        if compaction
        or (policy(config, r)["enabled"] and policy(config, r)["exclusive_writer"])
    ]
    if writable:
        statements.append(
            dict(Effect="Allow", Action=["s3:DeleteObject"], Resource=writable)
        )
    return dict(Version="2012-10-17", Statement=statements)


def setup(manager, compaction=False):
    config, _, _ = scope(manager.private / "repositories.yaml")
    for r in config["repositories"]:
        policy(config, r)
    directory = manager.private / "object-cleaner"
    directory.mkdir(mode=0o750, exist_ok=True)
    real_directory(directory)
    os.chown(directory, 20010, 20010)
    directory.chmod(0o750)
    identity = directory / "identity.json"
    if identity.exists() or identity.is_symlink():
        saved = read_json(identity)
        if not re.fullmatch(r"oa-cleaner-[a-f0-9]{24}", saved["user"]):
            raise ValueError("invalid cleaner identity")
        user = saved["user"]
        for name in ("cleaner_access", "cleaner_secret"):
            file = directory / name
            if not file.is_file() or file.is_symlink():
                raise ValueError("incomplete cleaner credentials")
        if (directory / "cleaner_access").read_text() != user:
            raise ValueError("cleaner identity mismatch")
    else:
        if any(directory.iterdir()):
            raise ValueError("unrecognized cleaner directory")
        user = "oa-cleaner-" + secrets.token_hex(12)
        write_private(identity, json.dumps(dict(user=user)))
        write_private(directory / "cleaner_access", user, uid=20010)
        write_private(directory / "cleaner_secret", secrets.token_hex(32), uid=20010)
    # Retry-safe account convergence. Do not replace the Publisher identity/policy.
    try:
        info = json.loads(manager.oc("admin", "user", "info", "store", user))
    except RuntimeError:
        manager.oc(
            "admin",
            "user",
            "add",
            "store",
            user,
            (directory / "cleaner_secret").read_text(),
        )
        info = json.loads(manager.oc("admin", "user", "info", "store", user))
    if info.get("userStatus") != "enabled":
        raise ValueError("cleaner account disabled")
    policies = set(filter(None, info.get("policyName", "").split(",")))
    if policies - {"assets-object-retention"}:
        raise ValueError("unexpected cleaner policy binding")
    value = iam_policy(config, compaction=compaction)
    write_private(manager.private / "oc-cli/object-retention.json", json.dumps(value))
    manager.oc(
        "admin",
        "policy",
        "add",
        "store",
        "assets-object-retention",
        "/config/object-retention.json",
    )
    manager.oc(
        "admin", "policy", "set", "store", "assets-object-retention", "user=" + user
    )
    write_private(
        directory / "ca.crt", (manager.private / "tls/ca.crt").read_text(), uid=20010
    )
    # Identity is host-only; containers only need the credential and CA files.
    print(
        json.dumps(
            dict(
                status="ready",
                delete_enabled_repositories=sum(
                    policy(config, r)["enabled"]
                    and policy(config, r)["exclusive_writer"]
                    for r in config["repositories"]
                ),
            )
        )
    )
