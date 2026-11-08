"""Classify historical reminders using evidence already read by host collectors.

Correlation identities remain private. No extra scans, business writes or S3
checks are performed, and a different SHA or source report cannot close an alert.
"""

CHANGED = (
    "scope_changed",
    "source_changed",
    "manifest_changed",
    "observation_unavailable",
)


def stable(attempt):
    return not any(attempt.get(k) for k in CHANGED)


def verified(attempt):
    return (
        attempt.get("status") == "complete"
        and attempt.get("diagnosis") == "verified"
        and stable(attempt)
    )


def same(left, right, source=False):
    if not left or not right or left.get("sha") != right.get("sha"):
        return False
    digest = left.get("source_digest") if source else left.get("report_digest")
    return (
        isinstance(digest, str)
        and len(digest) == 64
        and digest == right.get("source_digest")
    )


def annotate(snapshot, evidence):
    groups = {
        name: {r["key"]: r for r in snapshot.get(name, {}).get("repositories", [])}
        for name in ("restorations", "restoration_checks", "restoration_closures")
    }
    for name, rows in groups.items():
        for key, row in rows.items():
            a = row.get("attempt", {})
            state, reason = "active", "unavailable"
            if row.get("result_status") == "never":
                state, reason = "none", "never"
            elif row.get("result_status") == "ok" and a:
                check = groups["restoration_checks"].get(key, {}).get("attempt", {})
                closure = groups["restoration_closures"].get(key, {}).get("attempt", {})
                own = evidence.get(name, {}).get(key)
                checked = evidence.get("restoration_checks", {}).get(key)
                closed = evidence.get("restoration_closures", {}).get(key)
                closed_ok = closure.get("status") == "closed" and stable(closure)
                if name == "restorations":
                    if a.get("scope_changed"):
                        reason = "scope-changed"
                    elif closed_ok and same(own, closed):
                        state, reason = "resolved", "explicitly-closed"
                    elif a.get("status") == "complete" and not a.get("uncertain"):
                        state, reason = "none", "complete"
                    elif (
                        same(own, checked)
                        and verified(check)
                        and not check.get("references_changed")
                    ):
                        if a.get("uncertain"):
                            reason = "needs-close"
                        else:
                            state, reason = "resolved", "later-verified"
                    else:
                        reason = "needs-check"
                elif name == "restoration_checks":
                    if (
                        closed_ok
                        and same(own, closed, source=True)
                        and closure.get("finished_at", 0) >= a.get("started_at", 0)
                    ):
                        state, reason = "resolved", "explicitly-closed"
                    elif verified(a) and not a.get("source_uncertain"):
                        if a.get("references_changed") or a.get("stale"):
                            state, reason = "historical", "verified-history"
                        else:
                            state, reason = "none", "verified"
                    elif (
                        verified(a)
                        and not a.get("references_changed")
                        and not a.get("stale")
                    ):
                        reason = "needs-close"
                    else:
                        reason = "needs-check"
                else:
                    if a.get("status") == "refused" and row.get(
                        "acknowledgment", {}
                    ).get("status") in ("invalid", "unavailable"):
                        reason = "acknowledgment-invalid"
                    elif (
                        a.get("status") == "refused"
                        and row.get("acknowledgment", {}).get("status") == "valid"
                    ):
                        state, reason = "resolved", "manually-acknowledged"
                    elif closed_ok:
                        state, reason = "resolved", "explicitly-closed"
                    elif (
                        a.get("status") == "refused"
                        and a.get("reason") == "ineligible"
                        and a.get("source_status") == "complete"
                        and a.get("source_uncertain") is False
                        and stable(a)
                    ):
                        state, reason = "historical", "close-not-needed"
                    else:
                        reason = "close-review"
            row["reminder"] = dict(state=state, reason=reason)
