import json
from pathlib import Path

import pytest

from buildq_bench.soak import (
    INVARIANTS,
    JsonlLog,
    Limits,
    SoakError,
    accounting_mismatch,
    cargo_arguments,
    dirty_after,
    ensure_inside,
    evaluate_sample,
    expected_fixture_output,
    gc_problems,
    isolated_environment,
    output_matches,
    parse_duration,
    parse_ps,
    parse_size,
    plan_action,
    read_marker,
    resolve_repos,
    summarize,
    wait_stats,
)

LIMITS = Limits(slots=2, stranded_seconds=600)
NOW = 1_800_000_000.0


def holder(slot: int, pid: int | None) -> dict:
    return {"slot": slot, "busy": pid is not None, "job": {"pid": pid} if pid else None}


def status(
    holders: list[dict], waiting: list[dict] | None = None, slots: int = 2
) -> dict:
    group = {
        "class": "build",
        "slots": slots,
        "holders": holders,
        "waiters": waiting or [],
    }
    return {"classes": [group]}


def sample(**fields: object) -> dict:
    base = {
        "type": "sample",
        "t": NOW,
        "processes": [],
        "live_groups": [],
        "dead_groups": [],
    }
    return {**base, **fields}


def invariants_of(violations: list[dict]) -> set[str]:
    return {v["invariant"] for v in violations}


@pytest.mark.parametrize(
    ("text", "seconds"),
    [("90s", 90), ("10m", 600), ("24h", 86400), ("1d12h", 129600), ("1.5h", 5400)],
)
def test_parse_duration(text: str, seconds: float) -> None:
    assert parse_duration(text) == seconds


@pytest.mark.parametrize("text", ["", "10", "h", "5x", "0s", "1h garbage"])
def test_parse_duration_rejects_garbage(text: str) -> None:
    with pytest.raises(ValueError):
        parse_duration(text)


def test_parse_size() -> None:
    assert parse_size("512M") == 512 * 1024**2
    assert parse_size("20G") == 20 * 1024**3
    assert parse_size("1GiB") == 1024**3
    with pytest.raises(ValueError):
        parse_size("lots")


def test_schedule_is_deterministic_and_resumable() -> None:
    repos = ["a", "b", "c", "d"]
    first = [plan_action(7, i, repos, 3) for i in range(200)]
    again = [plan_action(7, i, repos, 3) for i in range(200)]
    other = [plan_action(8, i, repos, 3) for i in range(200)]
    assert first == again
    assert first != other
    # Resuming at index 120 sees exactly the tail of the original schedule.
    assert [plan_action(7, i, repos, 3) for i in range(120, 200)] == first[120:]


def test_schedule_covers_every_action_kind_within_bounds() -> None:
    repos = ["a", "b", "c", "d"]
    actions = [plan_action(1, i, repos, 3) for i in range(600)]
    assert {a.kind for a in actions} >= {
        "build", "build_release", "build_features", "test",
        "edit", "burst", "cancel", "gc", "gc_pressure",
    }  # fmt: skip
    assert all(a.repo in repos and 0 <= a.worktree < 3 for a in actions)
    assert all(3 <= a.burst_size <= 8 for a in actions if a.kind == "burst")
    assert all(
        a.cancel_signal in {"SIGTERM", "SIGINT"} for a in actions if a.kind == "cancel"
    )


def test_gc_is_periodic() -> None:
    kinds = [plan_action(1, i, ["a"], 1, gc_every=5).kind for i in range(20)]
    assert [i for i, k in enumerate(kinds) if k.startswith("gc")] == [4, 9, 14, 19]


def test_edits_are_reverted_and_never_doubled() -> None:
    dirty: set[tuple[str, int]] = set()
    seen_revert = False
    for i in range(400):
        action = plan_action(3, i, ["a", "b"], 2, frozenset(dirty))
        key = (action.repo, action.worktree)
        if action.kind == "edit":
            assert key not in dirty
            dirty.add(key)
        elif action.kind == "revert":
            assert key in dirty
            seen_revert = True
            dirty.discard(key)
    assert seen_revert


def test_dirty_state_is_rebuilt_from_the_log() -> None:
    records = [
        {"type": "action", "kind": "edit", "repo": "a", "worktree": 0, "exit": 0},
        {"type": "action", "kind": "edit", "repo": "b", "worktree": 1, "exit": 0},
        {"type": "action", "kind": "revert", "repo": "a", "worktree": 0, "exit": 0},
        {"type": "action", "kind": "edit", "repo": "c", "worktree": 0, "exit": 1},
    ]
    assert dirty_after(records) == {("b", 1)}


def test_repo_resolution_and_pinned_arguments() -> None:
    oss = {r.name: r for r in resolve_repos(None, local_fixtures=False)}
    assert list(oss) == ["bevy", "tokio", "ripgrep", "rust-analyzer"]
    assert (oss["bevy"].ref, oss["tokio"].ref) == ("v0.16.1", "tokio-1.45.1")
    assert (oss["ripgrep"].ref, oss["rust-analyzer"].ref) == ("14.1.1", "2025-06-02")
    assert cargo_arguments(oss["tokio"], "test") == ["test", "-p", "tokio@1.45.1"]
    assert cargo_arguments(oss["tokio"], "build_release")[-1] == "--release"
    assert len(resolve_repos(None, local_fixtures=True)) == 4
    with pytest.raises(ValueError):
        resolve_repos("nope", local_fixtures=False)
    with pytest.raises(ValueError):
        resolve_repos("bevy", local_fixtures=True)


def test_environment_is_isolated_under_root(tmp_path: Path) -> None:
    env = isolated_environment(tmp_path)
    for name in ("BUILDQ_DIR", "BUILDQ_CONFIG", "BUILDQ_CACHE", "TMPDIR", "CARGO_HOME"):
        assert Path(env[name]).is_relative_to(tmp_path)
    assert not (tmp_path / ".git").exists()  # TMPDIR parent is not a checkout


def test_paths_outside_root_are_refused(tmp_path: Path) -> None:
    ensure_inside(tmp_path, tmp_path / "repos/x")
    with pytest.raises(SoakError):
        ensure_inside(tmp_path, tmp_path / ".." / "elsewhere")


def test_fixture_marker_and_output_checks() -> None:
    source = 'pub const MARKER: &str = "fixture-a/wt1+edit4";\n'
    marker = read_marker(source)
    assert marker == "fixture-a/wt1+edit4"
    expected = expected_fixture_output(marker, "build_features")
    assert expected == "SOAK fixture-a/wt1+edit4 profile=debug extra=1"
    assert output_matches(f"noise\n{expected}\n", expected)
    stale = "SOAK fixture-a/wt1 profile=debug extra=1"
    assert not output_matches(stale, expected)
    assert not output_matches("", expected)


def test_orphan_process_after_buildq_exit_is_a_violation() -> None:
    orphan = {"pid": 90, "ppid": 1, "pgid": 77, "rss_kb": 10, "comm": "rustc"}
    found = evaluate_sample(sample(processes=[orphan], dead_groups=[77]), LIMITS)
    assert invariants_of(found) == {"no_orphan_processes"}
    assert not evaluate_sample(sample(processes=[orphan], live_groups=[77]), LIMITS)


def test_slot_limit_violations() -> None:
    ok = status([holder(0, 10), holder(1, 11)])
    assert not evaluate_sample(sample(status=ok), LIMITS)
    over = status([holder(0, 10), holder(1, 11), holder(2, 12)])
    assert invariants_of(evaluate_sample(sample(status=over), LIMITS)) == {"slot_limit"}
    duplicate = status([holder(0, 10), holder(1, 10)])
    assert invariants_of(evaluate_sample(sample(status=duplicate), LIMITS)) == {
        "slot_limit"
    }
    cargos = [
        {"pid": p, "ppid": 1, "pgid": 5, "rss_kb": 1, "comm": "cargo"} for p in range(5)
    ]
    found = evaluate_sample(
        sample(status=ok, processes=cargos, live_groups=[5]), LIMITS
    )
    assert invariants_of(found) == {"slot_limit"}


def test_stranded_ticket_needs_age_and_a_reason() -> None:
    def waiter(age: float) -> dict:
        return {"pid": 5, "enqueued_ms": int((NOW - age) * 1000)}

    busy = [holder(0, 10), holder(1, 11)]
    fresh = status(busy, [waiter(30)])
    assert not evaluate_sample(sample(status=fresh), LIMITS)
    long_but_busy = status(busy, [waiter(900)])  # slots busy: legitimate long build
    assert not evaluate_sample(sample(status=long_but_busy), LIMITS)
    free_slot = status([holder(0, 10), holder(1, None)], [waiter(900)])
    assert invariants_of(evaluate_sample(sample(status=free_slot), LIMITS)) == {
        "no_stranded_tickets"
    }
    forever = status(busy, [waiter(3 * 600 + 1)])
    assert invariants_of(evaluate_sample(sample(status=forever), LIMITS)) == {
        "no_stranded_tickets"
    }


def test_accounting_compares_du_with_report_only_when_idle() -> None:
    idle = status([holder(0, None), holder(1, None)])
    cache = {"total": 1000, "trash_bytes": 0}
    assert accounting_mismatch(sample(status=idle, cache=cache, du_bytes=1000)) is None
    assert accounting_mismatch(sample(status=idle, cache=cache, du_bytes=10**10))
    busy = status([holder(0, 10), holder(1, None)])
    assert (
        accounting_mismatch(sample(status=busy, cache=cache, du_bytes=10**10)) is None
    )


def test_gc_problems_surface_failed_reclamation() -> None:
    assert gc_problems({"failed_deletions": 0, "dedupe": {"errors": []}}) == []
    assert gc_problems({"failed_deletions": 2})
    assert gc_problems({"dedupe": {"errors": ["EIO"]}})


def test_ps_parsing() -> None:
    text = "  10     1    10   2048 /usr/bin/cargo\n 11 10 10 100 rustc\nbad line\n"
    rows = parse_ps(text)
    assert [(r["pid"], r["pgid"], r["comm"]) for r in rows] == [
        (10, 10, "cargo"),
        (11, 10, "rustc"),
    ]


def test_wait_stats() -> None:
    jobs = [
        {"enqueued_ms": 1000, "started_ms": 3000},
        {"enqueued_ms": 1000, "started_ms": 10000},
        {"enqueued_ms": 5, "started_ms": 0},
    ]
    stats = wait_stats(jobs)
    assert stats["jobs"] == 2 and stats["max_wait_s"] == 9.0
    assert wait_stats([])["jobs"] == 0


def records_for_summary() -> list[dict]:
    idle = status([holder(0, None), holder(1, None)])
    return [
        {"type": "start", "t": NOW - 3600, "seed": 4, "min_free_bytes": 100},
        {
            "type": "action",
            "index": 0,
            "kind": "build",
            "repo": "a",
            "exit": 0,
            "expect_success": True,
            "output_ok": True,
        },
        {
            "type": "action",
            "index": 1,
            "kind": "cancel",
            "repo": "a",
            "exit": -15,
            "expect_success": False,
            "leftover_pids": [],
        },
        {
            "type": "action",
            "index": 2,
            "kind": "gc",
            "repo": "a",
            "exit": 0,
            "expect_success": True,
            "gc_problems": [],
        },
        {
            "type": "sample",
            "t": NOW - 10,
            "status": idle,
            "cache": {"trash_bytes": 7, "over_budget": False},
            "du_bytes": 50,
            "free_disk": 900,
            "idle": True,
            "processes": [{"rss_kb": 10}, {"rss_kb": 5}],
            "violations": [],
        },
        {
            "type": "history_stats",
            "stats": {"jobs": 3, "max_wait_s": 4.0, "p95_wait_s": 4.0},
        },
    ]


def test_clean_run_passes_every_invariant() -> None:
    result = summarize(records_for_summary(), NOW)
    assert result["passed"] is True
    assert set(result["invariants"]) == set(INVARIANTS)
    assert all(v["passed"] for v in result["invariants"].values())
    assert result["elapsed_s"] == 3600.0
    assert result["actions_by_kind"] == {"build": 1, "cancel": 1, "gc": 1}
    assert result["min_free_disk_bytes"] == 900
    assert result["max_trash_bytes"] == 7
    assert result["max_tree_rss_kb"] == 15
    assert result["queue_wait"]["max_wait_s"] == 4.0
    json.dumps(result)


def test_each_failure_fails_only_its_invariant() -> None:
    cases = {
        "exit_codes": {
            "type": "action",
            "index": 9,
            "kind": "test",
            "repo": "a",
            "exit": 101,
            "expect_success": True,
        },
        "correct_outputs": {
            "type": "action",
            "index": 9,
            "kind": "build",
            "repo": "a",
            "exit": 0,
            "output_ok": False,
            "output_note": "stale",
        },
        "cancellation_cleanup": {
            "type": "action",
            "index": 9,
            "kind": "cancel",
            "repo": "a",
            "exit": -15,
            "leftover_pids": [42],
        },
        "gc_errors": {
            "type": "action",
            "index": 9,
            "kind": "gc",
            "repo": "a",
            "exit": 0,
            "gc_problems": ["1 failed deletion(s)"],
        },
        "slot_limit": {
            "type": "sample",
            "t": NOW,
            "violations": [{"invariant": "slot_limit", "detail": "3 holders"}],
        },
        "accounting_matches_disk": {
            "type": "sample",
            "t": NOW,
            "violations": [],
            "accounting_mismatch": "du far above",
        },
    }
    for name, extra in cases.items():
        result = summarize([*records_for_summary(), extra], NOW)
        failed = {k for k, v in result["invariants"].items() if not v["passed"]}
        assert failed == {name}, name
        assert result["passed"] is False


def test_storage_policy_needs_a_sustained_idle_overage_or_low_idle_free_space() -> None:
    idle = status([holder(0, None), holder(1, None)])
    over = {
        "type": "sample",
        "t": NOW,
        "status": idle,
        "cache": {"over_budget": True, "over_budget_reasons": ["x"]},
        "idle": True,
        "free_disk": 900,
        "violations": [],
    }
    brief = summarize([*records_for_summary(), over, over], NOW)
    assert brief["invariants"]["storage_policy"]["passed"]
    sustained = summarize([*records_for_summary(), over, over, over], NOW)
    assert not sustained["invariants"]["storage_policy"]["passed"]
    low = {**over, "cache": {}, "free_disk": 50}
    assert not summarize([*records_for_summary(), low], NOW)["invariants"][
        "storage_policy"
    ]["passed"]


def test_log_appends_and_tolerates_a_torn_last_line(tmp_path: Path) -> None:
    log = JsonlLog(tmp_path / "soak.jsonl")
    log.append({"type": "start", "t": 1})
    log.append({"type": "action", "index": 0})
    with log.path.open("a") as handle:
        handle.write('{"type": "act')  # crash mid-write
    reopened = JsonlLog(log.path)
    assert [r["type"] for r in reopened.read()] == ["start", "action"]
