# Security

Do not commit credentials, production domains, host addresses, private keys, generated runtime configuration, ACME state, logs or backups. Use private files and least-privilege identities. Docker build excludes private state.

The internal receiver verifies Gitea HMAC-SHA256 over raw request bytes and enforces an ID/branch allowlist. It never uses remote URLs or commands from payloads. No content-repository scripts execute. Publication endpoints must remain internal.

Known limits: one Worker; file queue on a local POSIX filesystem; no distributed locking; no object garbage collection; image formats validated by extension (not a malware scanner); S3 version publication is not batch-atomic; HTTPS OC reverse-proxy behavior requires deployment-specific acceptance. Administrators of the host or infrastructure configuration are trusted.

Report vulnerabilities privately through the repository security advisory mechanism. Do not include live secrets in issues.
