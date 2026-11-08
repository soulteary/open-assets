"""Reminders cannot be dismissed by age or unrelated successful operations."""

import copy
import json
import unittest

from open_assets import reminder_info as info

GROUPS = ("restorations", "restoration_checks", "restoration_closures")


def sample():
    snapshot = {
        k: dict(
            status="ok",
            repositories=[dict(key="assets", result_status="ok", attempt={})],
        )
        for k in GROUPS
    }
    snapshot[GROUPS[0]]["repositories"][0]["attempt"] = dict(
        status="failed", uncertain=True, started_at=1
    )
    snapshot[GROUPS[1]]["repositories"][0]["attempt"] = dict(
        status="complete",
        diagnosis="verified",
        source_uncertain=True,
        started_at=2,
        finished_at=3,
    )
    snapshot[GROUPS[2]]["repositories"][0]["attempt"] = dict(
        status="closed", started_at=4, finished_at=5
    )
    evidence = {
        GROUPS[0]: dict(assets=dict(sha="a" * 40, report_digest="b" * 64)),
        GROUPS[1]: dict(assets=dict(sha="a" * 40, source_digest="b" * 64)),
        GROUPS[2]: dict(assets=dict(sha="a" * 40, source_digest="b" * 64)),
    }
    return snapshot, evidence


def row(snapshot, group):
    return snapshot[GROUPS[group]]["repositories"][0]


class Reminders(unittest.TestCase):
    def test_matching_explicit_closure_resolves_without_rewriting_original(self):
        data, evidence = sample()
        before = copy.deepcopy(row(data, 0)["attempt"])
        info.annotate(data, evidence)
        self.assertEqual(row(data, 0)["reminder"]["state"], "resolved")
        self.assertEqual(row(data, 1)["reminder"]["state"], "resolved")
        self.assertEqual(row(data, 2)["reminder"]["state"], "resolved")
        self.assertEqual(row(data, 0)["attempt"], before)
        self.assertNotIn("b" * 64, json.dumps(data))
        self.assertNotIn("a" * 40, json.dumps(data))

    def test_other_sha_other_source_or_changed_evidence_cannot_resolve(self):
        for change in (
            "sha",
            "source_digest",
            "scope_changed",
            "source_changed",
            "manifest_changed",
            "observation_unavailable",
        ):
            data, evidence = sample()
            if change in ("sha", "source_digest"):
                evidence[GROUPS[2]]["assets"][change] = "c" * (
                    40 if change == "sha" else 64
                )
            else:
                row(data, 2)["attempt"][change] = True
            info.annotate(data, evidence)
            self.assertEqual(row(data, 0)["reminder"]["state"], "active", change)
            self.assertEqual(row(data, 1)["reminder"]["state"], "active", change)

    def test_verified_old_reference_is_history_not_processed(self):
        data, evidence = sample()
        row(data, 2).update(result_status="never", attempt={})
        check = row(data, 1)["attempt"]
        check.update(source_uncertain=False, references_changed=True, stale=True)
        info.annotate(data, evidence)
        self.assertEqual(
            row(data, 1)["reminder"],
            dict(state="historical", reason="verified-history"),
        )
        for flag in (
            "source_changed",
            "manifest_changed",
            "scope_changed",
            "observation_unavailable",
        ):
            check[flag] = True
            info.annotate(data, evidence)
            self.assertEqual(row(data, 1)["reminder"]["state"], "active")
            check.pop(flag)
        check.update(diagnosis="issues")
        info.annotate(data, evidence)
        self.assertEqual(row(data, 1)["reminder"]["state"], "active")

    def test_verified_data_does_not_close_uncertain_source_and_old_closure_cannot_cover_later_check(
        self,
    ):
        data, evidence = sample()
        row(data, 1)["attempt"]["started_at"] = 6
        info.annotate(data, evidence)
        self.assertEqual(row(data, 1)["reminder"]["reason"], "needs-close")
        row(data, 2).update(result_status="never", attempt={})
        info.annotate(data, evidence)
        self.assertEqual(row(data, 0)["reminder"]["reason"], "needs-close")
        row(data, 0)["attempt"]["uncertain"] = False
        info.annotate(data, evidence)
        self.assertEqual(row(data, 0)["reminder"]["reason"], "later-verified")

    def test_unknown_records_remain_attention_and_complete_source_needs_no_closure(
        self,
    ):
        data, evidence = sample()
        row(data, 2)["attempt"].update(
            status="refused",
            reason="ineligible",
            source_status="complete",
            source_uncertain=False,
        )
        row(data, 0)["result_status"] = "unavailable"
        info.annotate(data, evidence)
        self.assertEqual(row(data, 0)["reminder"]["state"], "active")
        self.assertEqual(row(data, 2)["reminder"]["reason"], "close-not-needed")
        row(data, 2)["attempt"]["observation_unavailable"] = True
        info.annotate(data, evidence)
        self.assertEqual(row(data, 2)["reminder"]["state"], "active")

    def test_actual_reports_correlate_privately_and_new_attempt_reactivates(self):
        import test_restore_close as fixture
        from open_assets import restore_info, restore_check_info, restore_close_info

        helper = fixture.RestoreClose()
        helper.setUp()
        self.addCleanup(helper.doCleanups)
        root = helper.helper.host()

        def observe():
            evidence = {name: {} for name in GROUPS}
            data = {}
            for group, module in zip(
                GROUPS, (restore_info, restore_check_info, restore_close_info)
            ):
                data[group] = module.summary(
                    root, root / "data", evidence=evidence[group]
                )
            info.annotate(data, evidence)
            self.assertNotIn(helper.f.sha, json.dumps(data))
            return data

        self.assertEqual(row(observe(), 0)["reminder"]["state"], "active")
        helper.run_close()
        closed = observe()
        self.assertEqual(row(closed, 0)["reminder"]["state"], "resolved")
        self.assertEqual(row(closed, 1)["reminder"]["state"], "resolved")
        self.assertEqual(helper.source.read_bytes(), helper.original)
        original = json.loads(helper.source.read_bytes())
        original["uploaded_objects"] = original.get("uploaded_objects", 0) + 1
        helper.source.write_text(json.dumps(original))
        changed = observe()
        self.assertEqual(row(changed, 0)["reminder"]["state"], "active")
        self.assertEqual(row(changed, 2)["reminder"]["state"], "active")
