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
- OC create-only upload through the proxy, missing-CSRF rejection and logout revocation.
- Authentication outage closes management while public images remain available.
- Original DNS credential bytes and all prior certificate identities retained.
- Application snapshot database integrity and encrypted off-host archive readability.
- Second version publication, image rollback and roll-forward, recovery of a persisted running job after Worker restart, retained historical object URL.

Limits: The initial HTTP/API acceptance was supplemented by the Chrome UI checks below. Tests do not certify accessibility or all OC upload/CSRF/browser combinations, power-loss durability of the underlying disk, arbitrary migrations or distributed deployments. Off-host backup destination is an operator responsibility.

## Chrome browser acceptance — 2026-11-07

Actual Chrome extension-driven UI checks completed:

- Unauthenticated management navigation reaches Stargate; password login reaches the requested service. Gitea's own login succeeds and displays both configured content repositories.
- Traefik Dashboard renders entrypoints, certificates, routers and services; the final topology reports no disabled/error routers or services.
- OC renders native IAM login, overview, bucket directory, object browser and upload dialog. Native IAM login succeeds.
- Real browser upload from an independent temporary file succeeds, including an object key containing Chinese, a space, `+` and `%`. Download through OC saves the expected filename. Original and downloaded 84-byte files compare equal with SHA-256 `18e1796fb1b2f89f2e84ab2d02587da142470f5126c66151cbb7b5239ac10ba1`.
- OC logout clears its UI session; refresh remains at IAM login. Direct navigation to the old download URL is denied by OC's origin guard.

Browser-discovered fix: OC rejected the authentication redirect chain with `invalid_origin`. A fixed, authenticated `/__auth_return` document now ends that chain before same-origin navigation to `/`. OC Origin, Fetch Metadata and CSRF protections are unchanged; no custom OC binary is used.

Tool/environment observations: Chrome's upload interface required temporary file-URL permission, explicitly approved and manually enabled by the operator, then manually restored off. An initial Documents-path transfer was interrupted before a PUT reached the server; independent temporary-file retries succeeded. The failed task expired without reporting success. OC opens downloads in a new tab, so waiting for the original tab's download event timed out; the actual downloaded file was verified on disk. These are recorded separately from application acceptance.

Scope remains Chrome on macOS. Other browser engines, mobile layouts, accessibility compliance and large concurrent browser transfers are not certified by these checks. Screenshots with production hostnames stay outside the public repository.
