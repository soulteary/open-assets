# Deployment acceptance — 2026-11-07

Platform: Ubuntu Linux amd64, single host, Docker Compose. Public examples use lab.dev; production identifiers and credentials are excluded.

Verified versions: OtterIO RELEASE.2026-10-28T07-52-43Z; OC RELEASE.2026-10-28T07-34-44Z; Stargate 1.2.7; Gitea 28.0.0-rootless; Traefik 3.7.13; Nginx 1.28. Component image digests are pinned in the example environment file; existing Traefik retains its prior image.

Passed:

- Seven local tests covering durable enqueue, duplicate jobs, rejected commit IDs, disabled repositories, real Git export, symlink rejection and signed HTTP events.
- Actual SSH Git Push → Gitea signed Webhook → durable task → read-only Git fetch → verified S3 objects → image activation.
- Two independent repositories and URL prefixes; Chinese, space, plus and percent filename access.
- Public image and attachment byte comparisons and HTTP Range.
- Valid public HTTPS certificates on all six service hosts.
- Anonymous management requests redirected to authentication; host-only authentication cookies.
- Authenticated Traefik JSON API, OC native IAM login and bucket listing; Gitea outer login.
- Second version publication, image rollback and roll-forward, recovery of a persisted running job after Worker restart, retained historical object URL.

Limits: HTTP/API acceptance is not a browser visual or accessibility audit. Tests do not certify all OC upload/CSRF/browser combinations, power-loss durability of the underlying disk, arbitrary migrations or distributed deployments. Off-host backup destination is an operator responsibility.
