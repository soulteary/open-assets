# Deployment acceptance — 2026-11-07

This file records checks performed at each implementation stage. Counts and component versions belong to their respective stage; documentation-only revisions do not imply a new production acceptance run.

Platform: Ubuntu Linux amd64, single host, Docker Compose. Public examples use lab.dev; production identifiers and credentials are excluded.

Initial deployment versions: OtterIO RELEASE.2026-10-28T07-52-43Z; OC RELEASE.2026-10-28T07-34-44Z; Stargate 1.2.7; Gitea 28.0.0-rootless; Traefik 3.7.13; Nginx 1.28. The current example environment has since been updated; the upgrade section below records the later baseline.

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

## Software, TLS, Info and health recovery — 2026-11-07

The existing server was upgraded before publishing the source changes. Versions and release rationale are recorded in [upgrade notes](upgrade.md); measurements and limits are in [performance](performance.md).

- Stargate 1.3.1, Gitea 28.3.0-rootless, Traefik 3.7.17 and Nginx 1.30.6-alpine were installed with pinned image digests. OtterIO/OC remain at their latest verified compatible release pair. Disabled Warden/Herald services were not introduced.
- Existing Traefik configuration, ACME state and DNS credentials were backed up before changes. Required application/auth networks now survive Traefik recreation.
- Certs Maker 3.8.2 generated independent storage and console CAs and leaves, each valid for 3650 days. Chain, hostname, key pair and validity checks passed; clients continue verifying TLS.
- All nine containers have passing Docker health checks. Host timers collect read-only status and perform bounded, scoped recovery. A controlled Info-only SIGSTOP fault went from unhealthy to healthy in 77.94 seconds. Docker lifecycle events and repair records confirmed recovery; public images stayed available. Docker RestartCount did not increase for this host-initiated restart and is not a complete repair counter.
- Real SSH Push through the signed Webhook reached published in 3.722 seconds. New current image bytes changed, while the preceding immutable attachment stayed readable. Repeated repository onboarding reused resources without routine service recreation.
- Gitea private egress now permits the fixed Webhook IP on a dedicated internal network rather than all private destinations. OC IAM login, CSRF rejection, upload, public-byte comparison and logout/revocation passed HTTP/API checks.
- Anonymous requests to all four management services redirected to Stargate; authenticated requests passed. Chrome login reached Info and rendered component health, a full-width disk section and separate internal/public certificate columns. The authentication status link displays status; it does not provide logout UI. Screenshots remain private.
- Bounded loopback reads completed 3989 requests without errors. These are short, capped local tests, not WAN capacity measurements.
- Thirty-four automated tests, Ruff and JavaScript syntax checks passed. Fresh initialization of configuration and ten-year certificates also passed in an isolated private mount namespace. This revision did not repeat the full cold-container DinD suite; historical results above apply to their original revisions.

Automatic recovery restarts eligible persistently unhealthy containers within documented budgets. It does not repair corrupted data, malformed queue entries, low disk space or incorrect configuration. Previously documented rollback-copy and malformed-task risks remain open. See [health operations](health.md) and [risks](risks.md).

## Compressed log retention — 2026-11-07

All nine service containers were recreated with explicit Docker local logging (10m, three files, compression enabled) and returned healthy. Public bytes/Range and management authentication regression checks passed. Host journald limits and an hourly, scoped private text-log rotation timer were installed; isolated fixtures verified Docker retention and logrotate gzip content. Database queue logs, certificates, credentials and backups were excluded. Configuration and retention limits are documented in [logging](logging.md).

## Protected-page gzip and optional probe display — 2026-11-07

Traefik dynamically loaded gzip after ForwardAuth for the four management routes and authenticated return document. Real authenticated requests negotiated gzip with Vary: Accept-Encoding, and decompressed HTML/JSON passed validation: Gitea 5925/15860 bytes, OC 9590/37736, Traefik API 1243/6792, Info status 1742/5505 (compressed/decoded, one response each). Anonymous redirects, login, public image/object bytes and Range checks passed; all nine containers remained healthy. Info now omits the third status item and separator when no additional probe is configured. Eight relevant regression tests, Ruff and JavaScript syntax checks passed.

## Checked image rollback and malformed-task isolation — 2026-11-07

These fixes close the two issues that remained open in the earlier stage records above.

- The old production image reproduced both faults inside a disposable container using only temporary data: rollback activated an incorrect image copy, and malformed JSON aborted queue processing.
- The candidate then passed all 42 tests in an independent container with network disabled, a read-only root, a temporary writable filesystem and no production credentials/data mounts. Eight recovery tests cover missing/corrupt/extra/linked serving files, malformed manifests, activation failures before/after link replacement, invalid task fields/identity, oversized/nonregular JSON records and continued processing when quarantine fails. The isolated publication fixture uses real Git commits/export and a fake S3 store; it is not a complete cold-stack Gitea/S3 test.
- The previous application image and a readable, hashed source/private/publisher/image snapshot were retained before deployment. Only Webhook and Publisher were recreated; other application containers and the existing Traefik remained running.
- Live acceptance used the existing dedicated performance-check repository. A controlled malformed task was preserved byte-for-byte in quarantine while a real SSH Push, signed Gitea Hook and actual S3 publication completed normally.
- A damaged inactive image copy and a temporarily missing inactive image directory both caused rollback refusal; the current image bytes and published record remained at the new version. Test changes were restored. Valid rollback and roll-forward passed public byte checks; the historical immutable attachment remained readable. The temporary personal Push key was revoked.
- Public image/object bytes, Range, four management authentication flows, gzip and fresh Info data passed regression checks. All nine containers were healthy.

The application does not automatically repair damaged versions or replay quarantined records. Disk/power failure, concurrent trusted administrators, cross-system commit windows, unbounded historical retention and external alerting remain outside these fixes. See [operations](operations.md) and the updated [risk record](risks.md).

## Configurable local history retention — 2026-11-07

- The final candidate passed all 60 tests in an independent container with network disabled, a read-only root, temporary writable space and no production data or credentials. Eighteen retention tests cover protection sets, age/count policy, opt-in application, bounded scans/deletions, archive deduplication, lock conflicts, symlinks, invalid references and interrupted claims followed by recreation of the same SHA.
- Previous images and a readable, hashed application/private/publisher/image snapshot were retained before installation. Only Publisher and Webhook were recreated. The private deployment policy explicitly enables keep_last=5, keep_days=30, min_age_hours=24 and rollback_versions=2; public examples continue to default to disabled.
- Deployed CLI acceptance used disposable data under separate mounted subdirectories, without public routes or S3 calls. It deleted one eligible local version pair, archived its terminal task byte-for-byte, and preserved current, rollback, pinned, pending, running, images_prepared and failed references. Preview, repeated execution, invalid-job refusal, busy locking and archived-task deduplication passed; test directories were removed afterward.
- Real SSH Push, signed Hook publication, corrupt/missing-copy refusal, valid image rollback/roll-forward and retained historical attachment reads passed on the dedicated content fixture. The temporary personal Push key was revoked. Four management authentication and gzip checks, public byte/Range checks and fresh Info status also passed.
- A full production apply checked four configured repositories and found no eligible old versions/tasks: zero newly claimed versions and zero archived production tasks. Actual deletion was proven only on the disposable CLI fixture; no claim is made that production disk use fell substantially.
- The six-hour retention timer is enabled; its service completed successfully. All nine containers were healthy, and the existing Info, repair and log rotation timers remained active.

This mechanism retains S3 objects, Git history, manifests, archive records, quarantine data and backups. It is not a global disk quota, external capacity alert, off-host backup or hard resource limit. See [retention operations](retention.md) and the remaining [capacity risks](risks.md#3-容量限制不能约束整个部署).

## Info repository storage and capacity notices — 2026-11-07

- The candidate passed 72 Python tests in an independent container with network disabled, read-only root, temporary writable space and no production credentials/data mounts. Eleven storage tests cover unique version counts, allocation totals, filesystem selection, symlink exclusion, bounded caching/failure fallback, filtered metadata, cleanup states and capacity thresholds. Four isolated Node rendering tests with a minimal DOM stub verify rendering, text-only output, stale data and failed-refresh withdrawal of a previous normal notice; CI runs these tests without npm dependencies.
- Old application source/image and Info data were backed up before deployment. Info and Publisher were recreated; Webhook and the other application containers remained running. Publisher now persists one bounded complete-result summary per repository so a single-repository run cannot overwrite another repository’s last result. The deletion and protection rules were unchanged. No DNS, certificate, authentication, repository or retention-policy changes were needed.
- A full apply initialized four durable repository summaries. A targeted apply preserved the other three files byte-for-byte; no business versions were eligible for deletion. After Publisher replacement, a real signed Git Push, checked image rollback/roll-forward and historical attachment read passed; the temporary Push key was revoked.
- Live host checks verified all four repository counts against release directory names and allocated local bytes against GNU du. Policies and last complete cleanup summaries matched private runtime records. The cache file was mode 0600, while Info retained only its read-only snapshot mount and existing HTTP routes.
- Authenticated Chrome rendered the new capacity notice and all four repository rows with version counts, bytes, policy values, sampling times and cleanup summaries. Disk remained a full-width section and certificate columns remained separate. Native Chrome accessibility/window inspection was used because the extension-based browser connection was unavailable; screenshots stay outside the public repository.
- Anonymous/authenticated management flows, gzip, public image/object bytes, Range and fresh snapshot checks passed. All nine containers were healthy. Live space/inode conditions were normal; warning, critical, unknown and stale conditions were simulated only in isolated tests, without filling production disks or changing live thresholds.

Numbers describe bounded local samples, not S3 totals, content integrity or precisely reclaimable storage. The page remains read-only and provides no external alert delivery or automatic corrective action. Limits and configurable thresholds are documented in [Info](info.md).

## Info cached OtterIO repository statistics — 2026-11-07

- The final candidate passed 85 Python tests in a container with network disabled, read-only root and no production data/credential mounts. Thirteen object-statistics tests cover prefix isolation, empty listings, pagination, cross-run fairness, request budgets, interval caching, expired cursors, scope changes, failure fallback, filtered summaries and SDK timeout/retry settings. Five Node rendering tests cover the existing capacity panel plus object totals and incomplete/failed states.
- A separate OtterIO instance used an internal temporary Docker network, no exposed ports, disposable tmpfs data and fixture credentials. Counts and logical bytes matched 1003 objects with Chinese/special-character keys, two objects in an adjacent prefix and zero in an empty prefix; an out-of-scope object was excluded. A one-page budget resumed correctly across four runs. An immediate repeat made zero ListObjectsV2 calls. Only the temporary test containers and network were removed.
- Application/private configuration and Info state were backed up and hashed, and the previous image was retained. Deployment recreated Publisher and Info only. Existing Traefik certificates, credentials, authentication, routes, IAM and retention policies were retained. The new fifteen-minute object timer is separate from the thirty-second host timer.
- At the live acceptance sample, all four configured repositories matched an independent SDK-paginator aggregation of object count and Size. An immediate service repeat left the private cache byte-for-byte unchanged. The root cache was mode 0600, and the host snapshot included only stable keys, status, counts, bytes and sample times; no object names, continuation tokens, endpoints or credentials.

- The deployed authenticated HTML, JavaScript and API exposed the new column and all four complete samples; anonymous requests still entered Stargate, and direct access to the private cache returned 404. Real signed Git Push, checked rollback/roll-forward, retained attachment bytes, public Range requests, management authentication and gzip passed regression checks. The temporary Push key was revoked. All nine containers were healthy; Info, object statistics, retention, repair and log rotation timers were active. These checks used HTTP/API and isolated DOM rendering, not a new native-browser session.

These values describe current visible objects and logical sizes over the sampling window. They do not measure physical OtterIO disk use, noncurrent S3 versions or unfinished multipart uploads, and do not drive retention decisions. New pushes appear at the next completed object sample rather than the next browser refresh. See [collection limits and operations](info.md#otterio-对象统计).

## Opt-in OtterIO historical object retention — 2026-11-07

- The candidate passed 102 Python tests in a network-disabled container with a read-only root, temporary writable space and no production data/credential mounts. Seventeen new tests cover preview/double opt-in plus exclusive ownership, common protection references, age/count/object/byte limits, incomplete or unknown object sets, hash metadata checks, versioned-bucket refusal, lock contention, corrupt metadata, bounded scans, resumable receipts, dedicated IAM convergence and rollback refusal. Five existing Node rendering tests passed.
- A disposable OtterIO instance on an internal temporary network used fixture-only root and restricted cleaner identities, tmpfs data and no public ports. Real preview/apply/repeated execution preserved current, rollback, pinned and in-flight object bytes. Upload, out-of-scope listing and out-of-scope deletion were denied for the cleaner. An intentionally interrupted two-object deletion resumed from its receipt; image rollback to that deleted release was rejected. Only the fixture containers/network were removed afterward.
- The actual OtterIO release ignored a deliberately wrong If-Match ETag during deletion. The implementation therefore does not claim an atomic conditional-delete fence. Apply requires an explicit exclusive_writer declaration as well as enabled, IAM scope and --apply; shared application locks and whole-version metadata validation protect normal application operations. External trusted administrators must not change target objects/references concurrently. HEAD time comparison uses second precision because list XML can include finer timestamps than HTTP Last-Modified.
- Application/private configuration, publishing/image/Info state were backed up and hashed; the previous image was retained. Only Publisher and Webhook were recreated. Dedicated cleaner setup was repeated without credential rotation. The Publisher policy matched the saved pre-deployment policy after normalizing unordered IAM collections; no delete permission was added to it.
- All four business repositories were previewed with current references protected. Defaults remained enabled=false and exclusive_writer=false. The dedicated cleaner IAM policy had no DeleteObject or PutObject permission; even explicit --apply against those disabled policies removed zero objects and created no deletion receipts. The installed six-hour service ran successfully in preview mode with no --apply. A manual final host sample overlapped its normal timer and lost the nonblocking collector lock; the timer's fresh snapshot was used rather than forcing the lock.
- After deployment, a real signed Git Push, malformed-task isolation, corrupt/missing-image rollback refusal, valid rollback/roll-forward and historical attachment reads passed. Anonymous/authenticated management routes, object-statistics fields, gzip and public byte/Range checks passed. All nine containers were healthy and six timers active. No business S3 objects were deleted; actual destructive validation was confined to the disposable OtterIO instance. The temporary personal Push key was revoked.

This is not an S3 lifecycle rule, bucket-version cleanup, global quota or transaction across a release. Missing manifests, unknown/orphaned objects, unfinished multipart data, metadata archives and external alerts remain separate concerns. Operational limits and the exclusive-writer requirement are documented in [object retention](object-retention.md).

## Info object retention plans and execution outcomes — 2026-11-07

- The final candidate passed 111 Python tests in a network-disabled container with a read-only root and no production data/credential mounts. Nine new observation tests cover independent preview/apply history, partial-failure counters, busy/disabled outcomes, policy changes, expiry, missing/corrupt records, privacy filtering, late writes, per-key isolation, read-only sampling, exclusion limits and failed service conditions. Eight Node rendering tests cover the previous dashboard plus object policies, candidate counts/bytes, preview versus apply, partial failure, busy/policy-change states and corrupted records.
- The disposable OtterIO acceptance also checked real partial-delete observation records: failure preserved the complete preview and recorded the one acknowledged deletion, while successful resumption updated complete apply history. Delete permissions and exclusive-writer assumptions were confined to fixture identities; no business deletion was used to manufacture dashboard examples.
- Application/private configuration, Info and publishing state were backed up and hashed; the previous image was retained. Only Publisher and Info were recreated. Runtime configuration, authentication credentials, CA, IAM policy and retention settings were retained. Four complete per-repository preview summaries were initialized. A targeted preview preserved the other three summary files byte-for-byte; the normal service then completed its default preview.
- Host snapshots matched persisted policy, eligible totals, budget-selected candidates and exclusion counts for all four repositories. Preview/apply histories remained distinct and no apply history was invented. The snapshot omitted object keys, full SHAs, bucket/endpoint, policy digests, remote URLs and raw errors. Two authenticated HTTP API reads left observation files unchanged; HTML/JS contained the new read-only section and anonymous routes still entered Stargate. A DOM-stub render of the live authenticated snapshot verified four policy/plan/preview-only rows; this was HTTP/API plus isolated rendering acceptance, not a new native-browser session.
- Real signed Git Push, malformed-task isolation, rollback validation, retained attachment bytes, public Range requests, management authentication, gzip and fresh Info data passed regression checks. The temporary Push key was revoked. All nine containers were healthy and six timers active. Business object deletion stayed disabled; production failure/partial-deletion cases were not injected.

Counts are cached planning observations, not real-time remaining storage or exact physical reclamation. The most recent attempt can fail or be skipped while older complete preview/apply records remain visible. Hard termination or lost responses can leave uncertain partial effects; see [Info field semantics](info.md#对象保留策略与清理记录) and [object-retention recovery](object-retention.md).
