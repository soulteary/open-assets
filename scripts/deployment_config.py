"""Pure node/domain configuration shared by initialization and read-only checks."""

import argparse
import json
import re
from pathlib import Path

DEFAULT_NODE_NAME = "node-name"
ROLES = ("oss", "img", "storage", "git", "auth", "traefik", "info")
MANAGEMENT_ROLES = {"storage", "git", "auth", "traefik", "info"}


def build_domains(node_name, public_base):
    """Validate DNS inputs before initialization creates any files or secrets."""
    if not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?", node_name):
        raise ValueError(
            "NODE_NAME must be a lowercase DNS label of 1 to 48 characters"
        )
    labels = public_base.split(".")
    if len(labels) < 2 or any(
        not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
        for label in labels
    ):
        raise ValueError(
            "PUBLIC_BASE must be a lowercase DNS domain without a URL scheme"
        )
    domains = {
        role: f"{role}-{node_name}."
        + ("console." if role in MANAGEMENT_ROLES else "")
        + public_base
        for role in ROLES
    }
    if any(len(value) > 253 for value in domains.values()):
        raise ValueError("generated hostname exceeds the DNS length limit")
    return domains


def render_env(template, node_name, public_base):
    """Resolve naming fields explicitly; leave pinned images and other values intact."""
    domains = build_domains(node_name, public_base)
    values = {
        "NODE_NAME": node_name,
        "PUBLIC_BASE": public_base,
        "STORAGE_DOMAIN": domains["storage"],
        "GIT_DOMAIN": domains["git"],
    }
    lines = []
    found = set()
    for line in template.splitlines():
        key = line.partition("=")[0]
        if key in values:
            found.add(key)
            line = f"{key}={values[key]}"
        lines.append(line)
    if found != values.keys():
        raise ValueError("environment template is missing naming fields")
    return "\n".join(lines) + "\n"


def check_naming(env_path, domains_path):
    """Compare persisted naming metadata with runtime values without rewriting them."""
    env = dict(
        line.split("=", 1)
        for line in Path(env_path).read_text().splitlines()
        if line and not line.startswith("#") and "=" in line
    )
    expected = build_domains(env["NODE_NAME"], env["PUBLIC_BASE"])
    if json.loads(Path(domains_path).read_text()) != expected:
        raise ValueError(
            "naming metadata differs from runtime domains; no files changed"
        )
    for role in ("storage", "git"):
        if env[role.upper() + "_DOMAIN"] != expected[role]:
            raise ValueError(
                "Compose domain differs from runtime domains; no files changed"
            )


def main():
    parser = argparse.ArgumentParser(
        description="Read-only node naming consistency check"
    )
    parser.add_argument("--env", required=True)
    parser.add_argument("--domains", required=True)
    args = parser.parse_args()
    try:
        check_naming(args.env, args.domains)
    except (ValueError, KeyError, OSError):
        parser.exit(
            1,
            "Naming check failed; inspect the private configuration locally. No files changed.\n",
        )
    print("Node naming matches all configured runtime domains; no files changed")


if __name__ == "__main__":
    main()
