"""Render authenticated routes; private paths are passed by the operator."""

import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from open_assets.svg import CSP  # noqa: E402

p = argparse.ArgumentParser()
p.add_argument("--domains", required=True)
p.add_argument("--ca", required=True)
p.add_argument("--output", required=True)
p.add_argument("--reference-releases", action="store_true")
o = p.parse_args()
if pathlib.Path(o.output).suffix not in [".yaml", ".yml"]:
    p.error("output must end in .yaml or .yml")
dom = json.loads(pathlib.Path(o.domains).read_text())
ca = pathlib.Path(o.ca).read_text()
services = {
    name: {"loadBalancer": {"servers": [{"url": url}]}}
    for name, url in [
        ("images", "http://nginx:8080"),
        ("git", "http://gitea:3000"),
        ("auth", "http://172.29.19.3:8080"),
        ("storage", "https://oc:9090"),
        ("objects", "https://otterio:9000"),
        ("info", "http://info:8082"),
    ]
}
if o.reference_releases:
    services["release-objects"] = {
        "loadBalancer": {"servers": [{"url": "http://object-reader:8083"}]}
    }
services["storage"]["loadBalancer"]["serversTransport"] = "console-tls"
services["objects"]["loadBalancer"]["serversTransport"] = "storage-tls"
routers = {}
for key, service in [
    ("img", "images"),
    ("oss", "objects"),
    ("git", "git"),
    ("auth", "auth"),
    ("storage", "storage"),
    ("traefik", "api@internal"),
    ("info", "info"),
]:
    rule = f"Host(`{dom[key]}`)"
    if key == "oss":
        rule += " && PathPrefix(`/attachments/repositories/`) && (Method(`GET`) || Method(`HEAD`))"
    routers[key] = {
        "rule": rule,
        "entryPoints": ["websecure"],
        "service": "release-objects"
        if key == "oss" and o.reference_releases
        else service,
        "tls": {"certResolver": "le"},
    }
    if key == "oss":
        routers[key]["middlewares"] = ["public-assets"]
    if key in ["git", "storage", "traefik", "info"]:
        routers[key]["middlewares"] = ["console-auth", "console-gzip"]
# This exact endpoint must reach Stargate before a host-only session exists.
# Its own ticket validation is the boundary; all other management paths use auth.
routers["session-exchange"] = {
    "rule": "("
    + " || ".join(
        "Host(`" + dom[k] + "`)" for k in ["git", "storage", "traefik", "info"]
    )
    + ") && Path(`/_session_exchange`)",
    "entryPoints": ["websecure"],
    "service": "auth",
    "priority": 1000,
    "tls": {"certResolver": "le"},
}
services["navigation"] = {"loadBalancer": {"servers": [{"url": "http://nginx:8081"}]}}
# A local HTML document ends the cross-site redirect chain without changing
# OC Origin/Fetch Metadata headers. It remains protected by ForwardAuth.
routers["auth-return"] = {
    "rule": "("
    + " || ".join(
        "Host(`" + dom[k] + "`)" for k in ["git", "storage", "traefik", "info"]
    )
    + ") && Path(`/__auth_return`)",
    "entryPoints": ["websecure"],
    "service": "navigation",
    "middlewares": ["console-auth", "console-gzip"],
    "priority": 900,
    "tls": {"certResolver": "le"},
}
config = {
    "http": {
        "routers": routers,
        "services": services,
        "middlewares": {
            "public-assets": {
                "headers": {
                    "customResponseHeaders": {
                        "Content-Security-Policy": CSP,
                        "X-Content-Type-Options": "nosniff",
                    }
                }
            },
            "console-gzip": {
                "compress": {
                    "encodings": ["gzip"],
                    "minResponseBodyBytes": 1024,
                    "includedContentTypes": [
                        "text/html",
                        "text/plain",
                        "text/css",
                        "text/javascript",
                        "application/javascript",
                        "application/json",
                        "application/xml",
                        "text/xml",
                        "image/svg+xml",
                    ],
                }
            },
            "console-auth": {
                "forwardAuth": {
                    "address": "http://172.29.19.3:8080/_auth",
                    "trustForwardHeader": False,
                    "maxResponseBodySize": 1048576,
                }
            },
        },
        "serversTransports": {
            "console-tls": {"serverName": dom["storage"], "rootCAs": [ca]},
            "storage-tls": {"serverName": "storage.internal", "rootCAs": [ca]},
        },
    }
}
pathlib.Path(o.output).write_text(json.dumps(config, indent=2))
