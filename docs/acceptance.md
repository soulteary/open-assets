# Deployment acceptance — 2026-11-07

This file records checks performed at each implementation stage. Counts and component versions belong to their respective stage; documentation-only revisions do not imply a new production acceptance run.

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

## Repository onboarding CLI — 2026-11-07

- Sixteen automated tests pass, including resource reuse, no restart on routine repeat, stable-key/prefix conflicts, public-repository/write-key rejection, duplicate-Hook rejection, policy failure retry and invalid-Nginx recovery.
- Single-host acceptance used a new private `content/cli-acceptance` repository: first ensure created/bound the repository, one read-only Deploy Key and one signed Hook, extended its S3 object prefix and applied its Nginx mapping.
- Repeated ensure kept the same repository ID and exactly one matching Key/Hook, one policy resource and one whitelist entry; services_recreated was false after the initial application.
- A conflicting root-prefix request was rejected before any corresponding Gitea repository existed.
- Disabling the test Hook and repeating ensure restored it without creating duplicates. Existing main-images/project-a S3 permissions remained present.
- A real SSH Push through the generated Hook reached published; both the public image and immutable attachment bytes matched the Git content.

The CLI is host-only and privileged; it does not grant application containers Docker access. Cross-system changes are resumable, not transactional. Private before-state and completion records stay outside the repository. The existing published assets continue serving during publisher/receiver configuration application.

## Isolated fresh installation — 2026-11-07

Two independent DinD instances completed the documented cold-start sequence. The second ran the guarded fresh-stack.sh from an empty daemon/configuration/data state and exited 0. Initialization, first/repeated onboarding, no restart on repeat, conflict rejection, real SSH Push in two repositories, published state, private-CA HTTPS byte comparisons, encoded image paths, Range and management authentication passed. No production host or credentials were used. See [environment and scope](fresh-install.md) and [reproduction](../tests/e2e/README.md). No application behavior change was needed; the suite now includes eighteen ordinary regression tests, including default refusal of the destructive-fixture runner.

## Configurable node naming — 2026-11-07

The naming change was installed and verified on the existing server before publication to GitHub:

- Two separate private mount namespaces bound empty test application/data directories over the initialization paths. One omitted NODE_NAME and used the default `node-name`; the other explicitly used `acceptance-node-2`. Both ran prepare-private.py, the read-only naming checker and Compose configuration validation successfully. No application containers were started in these namespaces.
- Original installed files and the private environment were backed up before the host scripts/template were updated. NODE_NAME and PUBLIC_BASE metadata were derived from the existing domains; existing public service names were preserved. This was not a domain migration.
- Checksums of the existing runtime domain, authentication, repository and CA files stayed unchanged. Running container identities also stayed unchanged; no application restart was needed.
- Live Compose, Nginx and Publisher configuration checks passed. Existing public image/attachment bytes matched the manifest, and Range downloads passed. Anonymous Gitea, OC and Dashboard requests redirected to Stargate; password login and authenticated routes passed for all three.
- Twenty-two ordinary automated tests, Ruff checks and shell syntax validation passed. The updated GIF and current text examples use generic node names.

These checks cover naming initialization and regression of the existing deployment. They do not repeat the full fresh-stack Git Push suite or Chrome upload/download/logout checks for this revision. Two separate temporary-directory reproductions confirmed the rollback-copy and malformed-task issues documented in [the risk assessment](risks.md); those issues remain open.
