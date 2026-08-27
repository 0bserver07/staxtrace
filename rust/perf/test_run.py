#!/usr/bin/env python3
"""Focused negative tests for the local performance evidence runner."""

from __future__ import annotations

import argparse
import copy
import importlib.util
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

MODULE_PATH = Path(__file__).with_name("run.py")
SPEC = importlib.util.spec_from_file_location("stax_performance_runner", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def valid_receipt(
    policy: dict, digest: str, scenario: str, run_id: str, identity: dict
) -> dict:
    expected = policy["scenarios"][scenario]
    guard = expected["guard"]
    if guard["comparison"] == "at_most":
        actual = min(float(guard["limit"]), 1.0)
    else:
        actual = max(float(guard["limit"]), 1.0)
    p99 = actual if guard["metric"] in {"p99_ms", "reader_p99_ms"} else 1.0
    resources = expected["resources"]
    return {
        "schema": "staxtrace.performance-receipt",
        "run_id": run_id,
        "policy_digest": digest,
        "scenario": scenario,
        "profile": "release",
        "runner": identity,
        "fixture": copy.deepcopy(expected["fixture"]),
        "samples": {
            "count": expected["samples"] or 1,
            "p50_ms": p99,
            "p95_ms": p99,
            "p99_ms": p99,
        },
        "throughput_per_second": (
            actual if guard["metric"] == "throughput_per_second" else None
        ),
        "setup_wall_seconds": 0.25,
        "copy_wall_seconds": 0.0,
        "timed_wall_seconds": 0.5,
        "scenario_wall_seconds": 1.0,
        "timeout_seconds": policy["scenario_timeout_seconds"],
        "resources": {
            "measurement_mode": resources["mode"],
            "peak_rss_bytes": 1,
            "peak_open_files": 1,
            "child_peak_rss_bytes": 1 if resources["child_process_samples"] else 0,
            "child_peak_open_files": 1 if resources["child_process_samples"] else 0,
            "rss_sample_count": resources["minimum_rss_samples"],
            "open_file_sample_count": resources["minimum_open_file_samples"],
            "child_process_sample_count": resources["child_process_samples"],
            "observer_wall_seconds": 0.01,
            "observer_wall_fraction": 0.01,
            "observer_wall_seconds_limit": policy["resource_observation"][
                "observer_wall_seconds_limit"
            ],
            "peak_rss_limit_bytes": policy["resource_limits"]["peak_rss_bytes"],
            "open_file_limit": policy["resource_limits"]["open_files"],
        },
        "guard": {
            "metric": guard["metric"],
            "comparison": guard["comparison"],
            "limit": guard["limit"],
            "actual": actual,
            "errors": guard.get("errors"),
            "passed": True,
        },
        "status": "pass",
    }


def valid_suite(policy: dict, digest: str, run_id: str, identity: dict) -> dict:
    receipts = [
        valid_receipt(policy, digest, scenario, run_id, identity)
        for scenario in policy["scenarios"]
    ]
    return {
        "schema": "staxtrace.performance-suite-receipt",
        "run_id": run_id,
        "policy_digest": digest,
        "runner": identity,
        "scenario_count": len(receipts),
        "scenario_digests": {
            receipt["scenario"]: runner.digest_value(receipt) for receipt in receipts
        },
        "scenarios": receipts,
        "baseline_comparison": {
            "mode": "absolute_guards",
            "baseline_file": None,
            "comparisons": [],
        },
        "status": "pass",
    }


def identity_with_class(identity: dict, runner_class: str) -> dict:
    result = copy.deepcopy(identity)
    result["runner_class"] = runner_class
    result["key"] = (
        f"{result['os']}-{result['architecture']}-{runner_class}-{result['toolchain']}"
    )
    return result


def harness_environment(
    policy: dict,
    digest: str,
    receipt_dir: str | Path,
    run_id: str,
    identity: dict,
    *,
    suite_start: float | None = None,
) -> dict[str, str]:
    start = time.monotonic() if suite_start is None else suite_start
    environment = {
        "STAX_PERF_HARNESS": "1",
        "STAX_PERF_RECEIPT_DIR": str(receipt_dir),
        "STAX_PERF_RUN_ID": run_id,
        "STAX_PERF_POLICY_DIGEST": digest,
        runner.RUNNER_DIGEST_ENV: runner.digest_value(identity),
        runner.SUITE_START_ENV: repr(start),
        runner.SUITE_TIMEOUT_ENV: repr(float(policy["suite_timeout_seconds"])),
    }
    environment.update(
        {
            environment_name: identity[field]
            for environment_name, field in runner.RUNNER_ENV_FIELDS.items()
        }
    )
    return environment


def finalizer_arguments(
    policy: dict,
    receipt_dir: str | Path,
    suite_start: float,
    *,
    baseline: str | Path | None = None,
) -> argparse.Namespace:
    return argparse.Namespace(
        receipt_dir=str(receipt_dir),
        baseline=str(baseline) if baseline is not None else None,
        suite_timeout=policy["suite_timeout_seconds"],
        suite_start=suite_start,
    )


def valid_baseline(policy: dict, digest: str, identity: dict) -> dict:
    suites = [
        valid_suite(policy, digest, f"{index + 1:032x}", identity)
        for index in range(policy["baseline_policy"]["clean_run_count"])
    ]
    receipts = [
        runner.validate_suite_receipt(suite, policy, digest) for suite in suites
    ]
    baseline = {
        "schema": "staxtrace.performance-baseline",
        "policy_digest": digest,
        "runner": identity,
        "clean_run_count": len(suites),
        "run_ids": [suite["run_id"] for suite in suites],
        "suite_digests": [runner.digest_value(suite) for suite in suites],
        "source_suites": suites,
        "scenarios": runner.derive_baseline_scenarios(receipts, policy),
        "status": "pass",
    }
    runner.validate_baseline(baseline, policy, digest)
    return baseline


def directory_snapshot(root: Path) -> dict[str, tuple[bytes, int]]:
    return {
        str(path.relative_to(root)): (path.read_bytes(), path.stat().st_mtime_ns)
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def write_valid_scenario_receipts(
    root: Path, policy: dict, digest: str, run_id: str, identity: dict
) -> None:
    for scenario in policy["scenarios"]:
        (root / f"{scenario}.json").write_text(
            json.dumps(valid_receipt(policy, digest, scenario, run_id, identity)),
            encoding="utf-8",
        )


def write_accepted_suite(
    root: Path,
    policy: dict,
    digest: str,
    run_id: str,
    identity: dict,
    *,
    baseline: Path | None = None,
) -> tuple[Path, bytes, int]:
    write_valid_scenario_receipts(root, policy, digest, run_id, identity)
    runner.finalize_suite(
        root,
        baseline,
        policy,
        digest,
        run_id,
        identity,
    )
    suite_path = root / runner.SUITE_RECEIPT_NAME
    return suite_path, suite_path.read_bytes(), suite_path.stat().st_mtime_ns


class AdditiveScenarioTests(unittest.TestCase):
    def setUp(self) -> None:
        self.core = runner.load_core_policy()
        self.fixtures = Path(__file__).parent / "testdata" / "scenarios"

    def test_positive_fragment_is_sorted_and_additive(self) -> None:
        combined = runner.merge_scenario_fragments(
            self.core, self.fixtures / "positive"
        )
        self.assertIn("fixture_extension", combined["scenarios"])
        self.assertNotIn("fixture_extension", self.core["scenarios"])
        self.assertNotEqual(
            runner.digest_value(combined), runner.digest_value(self.core)
        )

    def test_duplicate_fragment_identifier_is_rejected(self) -> None:
        with self.assertRaisesRegex(runner.ValidationError, "collides"):
            runner.merge_scenario_fragments(self.core, self.fixtures / "duplicate")

    def test_core_replacement_is_rejected(self) -> None:
        with self.assertRaisesRegex(runner.ValidationError, "collides"):
            runner.merge_scenario_fragments(self.core, self.fixtures / "core-mutation")


class StrictEvidenceTests(unittest.TestCase):
    def test_all_schema_digests_are_independently_pinned(self) -> None:
        self.assertEqual(set(runner.EXPECTED_SCHEMA_DIGESTS), set(runner.SCHEMA_PATHS))
        runner.verify_schema_pins()
        with tempfile.TemporaryDirectory() as directory:
            changed = Path(directory) / "validation.schema.json"
            schema = runner.load_json(runner.SCHEMA_PATHS["validation"])
            schema["properties"]["outcome"]["enum"].append("unchecked")
            changed.write_text(json.dumps(schema), encoding="utf-8")
            paths = dict(runner.SCHEMA_PATHS)
            paths["validation"] = changed
            with mock.patch.object(runner, "SCHEMA_PATHS", paths):
                with self.assertRaisesRegex(
                    runner.ValidationError, "independent digest"
                ):
                    runner.verify_schema_pins()

    def test_runner_class_must_come_from_checked_in_allowlist(self) -> None:
        policy, _ = runner.load_policy()
        self.assertEqual(policy["runner_classes"], ["local", "dedicated", "hosted"])
        self.assertEqual(
            policy["scenarios"]["hook_execution"]["fixture"],
            {"hook_invocations": 200, "recorded_events": 200},
        )
        self.assertEqual(
            policy["scenarios"]["hook_execution"]["resources"]["mode"],
            "continuous",
        )
        with mock.patch.dict(
            os.environ, {"STAX_PERF_RUNNER_CLASS": "unlisted"}, clear=False
        ):
            with self.assertRaisesRegex(runner.ValidationError, "allowlist"):
                runner.runner_identity(policy)
        invalid = identity_with_class(runner.runner_identity(policy), "unlisted")
        with self.assertRaisesRegex(runner.ValidationError, "allowlist"):
            runner.validate_runner(invalid, "runner", policy)

    def test_cached_runner_requires_every_field_and_an_exact_digest(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        environment = harness_environment(
            policy, digest, ".", "a" * 32, identity
        )
        with mock.patch.dict(os.environ, environment, clear=True):
            self.assertEqual(runner.cached_runner_identity(policy), identity)

        cached_names = [*runner.RUNNER_ENV_FIELDS, runner.RUNNER_DIGEST_ENV]
        for missing_name in cached_names:
            with self.subTest(missing=missing_name):
                partial = dict(environment)
                partial.pop(missing_name)
                with mock.patch.dict(os.environ, partial, clear=True):
                    with self.assertRaisesRegex(
                        runner.ValidationError, "incomplete"
                    ):
                        runner.cached_runner_identity(policy)

        alternate_class = next(
            value
            for value in policy["runner_classes"]
            if value != identity["runner_class"]
        )
        mutated_identity = identity_with_class(identity, alternate_class)
        mutated = dict(environment)
        mutated.update(
            {
                environment_name: mutated_identity[field]
                for environment_name, field in runner.RUNNER_ENV_FIELDS.items()
            }
        )
        with mock.patch.dict(os.environ, mutated, clear=True):
            with self.assertRaisesRegex(runner.ValidationError, "mutated"):
                runner.cached_runner_identity(policy)

    def test_executor_accepts_only_exact_timeout_classes_and_valid_suite_start(
        self,
    ) -> None:
        policy, _ = runner.load_policy()
        now = 1_000.0
        setup = argparse.Namespace(
            timeout=policy["setup_timeout_seconds"],
            suite_timeout=policy["suite_timeout_seconds"],
            suite_start=999.0,
        )
        clock_environment = {
            runner.SUITE_START_ENV: "999.0",
            runner.SUITE_TIMEOUT_ENV: repr(float(policy["suite_timeout_seconds"])),
        }
        with mock.patch.dict(os.environ, clock_environment, clear=False):
            self.assertEqual(
                runner.validated_execution_timeout(setup, policy, [], now=now),
                (float(policy["setup_timeout_seconds"]), 899.0),
            )
            scenario = copy.copy(setup)
            scenario.timeout = policy["scenario_timeout_seconds"]
            self.assertEqual(
                runner.validated_execution_timeout(
                    scenario, policy, ["query_cold"], now=now
                ),
                (float(policy["scenario_timeout_seconds"]), 180.0),
            )

            wrong_suite = copy.copy(setup)
            wrong_suite.suite_timeout -= 1
            with self.assertRaisesRegex(runner.ValidationError, "mutated"):
                runner.validated_execution_timeout(wrong_suite, policy, [], now=now)
            wrong_setup = copy.copy(setup)
            wrong_setup.timeout = policy["scenario_timeout_seconds"]
            with self.assertRaisesRegex(runner.ValidationError, "setup timeout"):
                runner.validated_execution_timeout(wrong_setup, policy, [], now=now)
            wrong_scenario = copy.copy(scenario)
            wrong_scenario.timeout = policy["setup_timeout_seconds"]
            with self.assertRaisesRegex(runner.ValidationError, "scenario timeout"):
                runner.validated_execution_timeout(
                    wrong_scenario, policy, ["query_cold"], now=now
                )

        for invalid_start in (math.inf, -math.inf, math.nan):
            invalid = copy.copy(setup)
            invalid.suite_start = invalid_start
            with self.subTest(suite_start=invalid_start):
                invalid_environment = {
                    runner.SUITE_START_ENV: repr(invalid_start),
                    runner.SUITE_TIMEOUT_ENV: repr(
                        float(policy["suite_timeout_seconds"])
                    ),
                }
                with mock.patch.dict(os.environ, invalid_environment, clear=False):
                    with self.assertRaisesRegex(runner.ValidationError, "finite"):
                        runner.validated_execution_timeout(invalid, policy, [], now=now)
        future = copy.copy(setup)
        future.suite_start = now + 0.001
        future_environment = {
            runner.SUITE_START_ENV: repr(future.suite_start),
            runner.SUITE_TIMEOUT_ENV: repr(float(policy["suite_timeout_seconds"])),
        }
        with mock.patch.dict(os.environ, future_environment, clear=False):
            with self.assertRaisesRegex(runner.ValidationError, "future"):
                runner.validated_execution_timeout(future, policy, [], now=now)

    def test_scenario_command_must_match_exact_plan(self) -> None:
        policy, _ = runner.load_policy()
        command = [
            "cargo",
            "test",
            "--release",
            "--locked",
            "-p",
            "stax-server",
            "--test",
            "perf_gates",
            "cold_and_warm_query_distributions",
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ]
        runner.verify_scenario_command(command, ["query_cold", "query_warm"], policy)
        with self.assertRaisesRegex(runner.ValidationError, "exact plan"):
            runner.verify_scenario_command(
                command + ["--show-output"], ["query_cold", "query_warm"], policy
            )

    def test_nested_type_confusion_is_rejected(self) -> None:
        schema = runner.load_json(runner.SCHEMA_PATHS["receipt"])
        invalid = {key: None for key in schema["required"]}
        invalid["schema"] = "staxtrace.performance-receipt"
        invalid["samples"] = {
            "count": True,
            "p50_ms": 1.0,
            "p95_ms": 1.0,
            "p99_ms": 1.0,
        }
        with self.assertRaises(runner.ValidationError):
            runner.validate_schema(invalid, schema, "receipt")

    def test_private_absolute_path_is_rejected(self) -> None:
        private_path = "/" + "Users/example/private"
        with self.assertRaisesRegex(runner.ValidationError, "absolute path"):
            runner.validate_privacy({"value": private_path})

    def test_private_network_path_url_and_command_strings_are_rejected(self) -> None:
        hostile_values = {
            "hostname": "build-node.example",
            "unlisted_dns_suffix": "worker.secretzone",
            "ipv6": "2001:db8::7",
            "url": "https://invalid.example/evidence",
            "absolute_path": "/tmp/private-evidence.json",
            "relative_traversal": "receipts/../private.json",
            "command": "sh -c whoami",
        }
        for label, value in hostile_values.items():
            with self.subTest(label=label):
                with self.assertRaises(runner.ValidationError):
                    runner.validate_privacy({"value": value})

    def test_private_and_command_shaped_object_keys_are_rejected(self) -> None:
        for key in ("worker.secretzone", "sh -c whoami"):
            with self.subTest(key=key):
                with self.assertRaises(runner.ValidationError):
                    runner.validate_privacy({key: 1})

    def test_reserved_local_hostname_variants_are_rejected_recursively(self) -> None:
        variants = (
            "localhost",
            "localhost.",
            "localhost:8123",
            "localhost.:8123",
            "localhost/private",
            "localhost\\private",
        )
        for variant in variants:
            with self.subTest(kind="value", variant=variant):
                with self.assertRaisesRegex(runner.ValidationError, "hostname"):
                    runner.validate_privacy({"outer": [{"value": variant}]})
            with self.subTest(kind="key", variant=variant):
                with self.assertRaisesRegex(runner.ValidationError, "hostname"):
                    runner.validate_privacy({"outer": [{variant: 1}]})

    def test_fixture_keys_and_baseline_filename_use_strict_vocabularies(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        receipt = valid_receipt(policy, digest, "query_cold", "a" * 32, identity)
        receipt["fixture"] = {"sh -c whoami": 1}
        with self.assertRaises(runner.ValidationError):
            runner.validate_receipt(
                receipt,
                "query_cold",
                policy["scenarios"]["query_cold"],
                policy,
                digest,
                require_pass=True,
            )

        suite = valid_suite(policy, digest, "b" * 32, identity)
        suite["baseline_comparison"]["baseline_file"] = "sh -c whoami.json"
        with self.assertRaises(runner.ValidationError):
            runner.validate_suite_receipt(suite, policy, digest)

    def test_invalid_utf8_standalone_validation_is_typed_and_path_free(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            receipt = root / "private-suite.json"
            receipt.write_bytes(b"\xff\xfe\x80")
            result = subprocess.run(
                [
                    sys.executable,
                    str(runner.PERF / "run.py"),
                    "validate-suite",
                    "--receipt",
                    str(receipt),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertEqual(
                json.loads(result.stdout),
                {
                    "schema": "staxtrace.performance-validation",
                    "artifact": "suite",
                    "outcome": "invalid",
                },
            )
            self.assertNotIn(str(root), result.stdout + result.stderr)
            self.assertNotIn("Traceback", result.stdout + result.stderr)

    def test_timeout_failure_receipt_is_typed_and_sanitized(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            receipt_dir = Path(directory)
            runner.write_failure_receipts(
                ["query_cold"],
                "timeout",
                0.01,
                policy["scenario_timeout_seconds"],
                None,
                policy,
                digest,
                receipt_dir,
                "a" * 32,
                identity,
            )
            receipt = runner.load_json(receipt_dir / "query_cold.failure.json")
            runner.validate_failure_receipt(receipt, "query_cold", policy, digest)
            encoded = runner.canonical_bytes(receipt)
            self.assertNotIn(str(Path.home()).encode(), encoded)
            self.assertNotIn(b"argv", encoded)
            self.assertNotIn(b"command", encoded)

    def test_failure_kinds_are_closed_and_finalization_is_suite_scoped(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            receipt_dir = Path(directory)
            runner.write_failure_receipts(
                [runner.SUITE_FINALIZATION_SCENARIO],
                "evidence_validation_failure",
                0.0,
                policy["suite_timeout_seconds"],
                None,
                policy,
                digest,
                receipt_dir,
                "d" * 32,
                identity,
            )
            path = receipt_dir / "suite_finalization.failure.json"
            receipt = runner.load_json(path)
            self.assertEqual(
                receipt["fixture"],
                {"registered_scenarios": len(policy["scenarios"])},
            )
            self.assertEqual(
                receipt["timeout_seconds"], policy["suite_timeout_seconds"]
            )
            runner.validate_failure_receipt(
                receipt,
                runner.SUITE_FINALIZATION_SCENARIO,
                policy,
                digest,
                expected_run_id="d" * 32,
                expected_runner=identity,
            )

            unknown = copy.deepcopy(receipt)
            unknown["failure_kind"] = "unchecked_failure"
            with self.assertRaises(runner.ValidationError):
                runner.validate_failure_receipt(
                    unknown,
                    runner.SUITE_FINALIZATION_SCENARIO,
                    policy,
                    digest,
                )
            scenario_scoped = copy.deepcopy(receipt)
            scenario_scoped["scenario"] = "query_cold"
            scenario_scoped["fixture"] = copy.deepcopy(
                policy["scenarios"]["query_cold"]["fixture"]
            )
            scenario_scoped["timeout_seconds"] = policy["scenario_timeout_seconds"]
            scenario_scoped["failure_kind"] = "comparison_failure"
            with self.assertRaisesRegex(runner.ValidationError, "suite-scoped"):
                runner.validate_failure_receipt(
                    scenario_scoped, "query_cold", policy, digest
                )

    def test_throughput_presence_is_exact_and_baseline_tampering_fails(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        for scenario, expected in policy["scenarios"].items():
            receipt = valid_receipt(policy, digest, scenario, "a" * 32, identity)
            expects_throughput = expected["guard"]["metric"] == "throughput_per_second"
            self.assertEqual(
                receipt["throughput_per_second"] is not None,
                expects_throughput,
            )
            runner.validate_receipt(
                receipt,
                scenario,
                expected,
                policy,
                digest,
                require_pass=True,
            )
            tampered = copy.deepcopy(receipt)
            tampered["throughput_per_second"] = None if expects_throughput else 1.0
            with self.assertRaises(runner.ValidationError):
                runner.validate_receipt(
                    tampered,
                    scenario,
                    expected,
                    policy,
                    digest,
                    require_pass=True,
                )

        baseline = valid_baseline(policy, digest, identity)
        self.assertIsNone(baseline["scenarios"]["query_cold"]["throughput_per_second"])
        self.assertIsNotNone(
            baseline["scenarios"]["one_file_append"]["throughput_per_second"]
        )
        surplus = copy.deepcopy(baseline)
        surplus["scenarios"]["query_cold"]["throughput_per_second"] = 1.0
        with self.assertRaisesRegex(runner.ValidationError, "stored medians"):
            runner.validate_baseline(surplus, policy, digest)
        missing = copy.deepcopy(baseline)
        missing["scenarios"]["one_file_append"]["throughput_per_second"] = None
        with self.assertRaisesRegex(runner.ValidationError, "stored medians"):
            runner.validate_baseline(missing, policy, digest)

    def test_child_resource_evidence_is_required_only_for_spawned_workloads(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        hook = valid_receipt(policy, digest, "hook_execution", "a" * 32, identity)
        runner.validate_receipt(
            hook,
            "hook_execution",
            policy["scenarios"]["hook_execution"],
            policy,
            digest,
            require_pass=True,
        )
        missing_child = copy.deepcopy(hook)
        missing_child["resources"]["child_peak_rss_bytes"] = 0
        with self.assertRaisesRegex(runner.ValidationError, "child resource"):
            runner.validate_receipt(
                missing_child,
                "hook_execution",
                policy["scenarios"]["hook_execution"],
                policy,
                digest,
                require_pass=True,
            )

        impossible_child = copy.deepcopy(hook)
        impossible_child["resources"]["child_peak_rss_bytes"] = (
            impossible_child["resources"]["peak_rss_bytes"] + 1
        )
        with self.assertRaisesRegex(runner.ValidationError, "process-tree peaks"):
            runner.validate_receipt(
                impossible_child,
                "hook_execution",
                policy["scenarios"]["hook_execution"],
                policy,
                digest,
                require_pass=True,
            )

        query = valid_receipt(policy, digest, "query_cold", "b" * 32, identity)
        query["resources"]["child_peak_rss_bytes"] = 1
        query["resources"]["child_peak_open_files"] = 1
        with self.assertRaisesRegex(runner.ValidationError, "child resource"):
            runner.validate_receipt(
                query,
                "query_cold",
                policy["scenarios"]["query_cold"],
                policy,
                digest,
                require_pass=True,
            )

    def test_suite_setup_timeout_is_atomic_and_typed(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            suite_start = time.monotonic() - policy["suite_timeout_seconds"] - 1.0
            environment = harness_environment(
                policy,
                digest,
                directory,
                "b" * 32,
                identity,
                suite_start=suite_start,
            )
            args = argparse.Namespace(
                timeout=policy["setup_timeout_seconds"],
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=suite_start,
                scenario=None,
                command=["--", "sh", "-c", "sleep 60"],
            )
            with mock.patch.dict(os.environ, environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("late runner discovery"),
                ):
                    with self.assertRaisesRegex(runner.ValidationError, "timeout"):
                        runner.execute(args)
            receipt = runner.load_json(Path(directory) / "suite_setup.failure.json")
            runner.validate_failure_receipt(
                receipt, runner.SUITE_SETUP_SCENARIO, policy, digest
            )

    def test_finalizer_clock_failures_are_typed_and_elapsed_is_truthful(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "d" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            now = time.monotonic()
            cases = (
                (
                    "expired",
                    now - policy["suite_timeout_seconds"] - 1.0,
                    None,
                    None,
                    "timeout",
                ),
                ("future", now + 60.0, None, None, "evidence_validation_failure"),
                ("nonfinite", math.nan, None, None, "evidence_validation_failure"),
                (
                    "wrong-timeout",
                    now,
                    float(policy["suite_timeout_seconds"] - 1),
                    None,
                    "evidence_validation_failure",
                ),
                (
                    "mutated-start",
                    now,
                    None,
                    now - 1.0,
                    "evidence_validation_failure",
                ),
            )
            for label, cached_start, cached_timeout, supplied_start, expected_kind in cases:
                with self.subTest(case=label):
                    receipt_dir = root / label
                    receipt_dir.mkdir()
                    environment = harness_environment(
                        policy,
                        digest,
                        receipt_dir,
                        run_id,
                        identity,
                        suite_start=cached_start,
                    )
                    if cached_timeout is not None:
                        environment[runner.SUITE_TIMEOUT_ENV] = repr(cached_timeout)
                    args = finalizer_arguments(
                        policy,
                        receipt_dir,
                        cached_start if supplied_start is None else supplied_start,
                    )
                    if cached_timeout is not None:
                        args.suite_timeout = cached_timeout
                    with mock.patch.dict(os.environ, environment, clear=True):
                        with mock.patch.object(
                            runner,
                            "runner_identity",
                            side_effect=AssertionError(
                                "finalizer rediscovered runner identity"
                            ),
                        ):
                            self.assertEqual(runner.validate(args), 2)
                    receipt = runner.load_json(
                        receipt_dir / "suite_finalization.failure.json"
                    )
                    self.assertEqual(receipt["failure_kind"], expected_kind)
                    self.assertIsNone(receipt["exit_code"])
                    runner.validate_failure_receipt(
                        receipt,
                        runner.SUITE_FINALIZATION_SCENARIO,
                        policy,
                        digest,
                        expected_run_id=run_id,
                        expected_runner=identity,
                    )
                    if label == "expired":
                        self.assertGreaterEqual(
                            receipt["elapsed_seconds"],
                            policy["suite_timeout_seconds"],
                        )
                        self.assertLess(
                            receipt["elapsed_seconds"],
                            policy["suite_timeout_seconds"] + 5.0,
                        )
                    elif label in {"future", "nonfinite"}:
                        self.assertEqual(receipt["elapsed_seconds"], 0.0)

    def test_safe_suite_elapsed_rejects_invalid_clock_values(self) -> None:
        self.assertEqual(runner.safe_suite_elapsed(-1.0, now=10.0), 0.0)
        self.assertEqual(runner.safe_suite_elapsed("11", now=10.0), 0.0)
        self.assertEqual(runner.safe_suite_elapsed("nan", now=10.0), 0.0)
        self.assertEqual(runner.safe_suite_elapsed("8.5", now=10.0), 1.5)

    def test_private_finalizer_preflight_has_only_closed_exit_codes(self) -> None:
        finalize_args = argparse.Namespace(
            receipt_dir="ignored",
            baseline=None,
            suite_timeout=900.0,
            suite_start=1.0,
        )
        verify_args = argparse.Namespace(
            **vars(finalize_args), suite_name=runner.SUITE_RECEIPT_NAME
        )
        with mock.patch.object(
            runner, "load_policy", side_effect=RuntimeError(str(Path.home()))
        ):
            self.assertEqual(
                runner.finalize_worker(finalize_args), runner.FINALIZER_EVIDENCE_EXIT
            )
            self.assertEqual(
                runner.verify_final_suite_worker(verify_args),
                runner.FINALIZER_EVIDENCE_EXIT,
            )

    def test_child_receives_only_the_cached_runner_identity(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "child-environment.json"
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                directory,
                "c" * 32,
                identity,
                suite_start=suite_start,
            )
            script = (
                "import json,os,pathlib,sys;"
                "names=json.loads(sys.argv[2]);"
                "pathlib.Path(sys.argv[1]).write_text("
                "json.dumps({name:os.environ[name] for name in names}),encoding='utf-8')"
            )
            args = argparse.Namespace(
                timeout=policy["setup_timeout_seconds"],
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=suite_start,
                scenario=None,
                command=[
                    "--",
                    sys.executable,
                    "-c",
                    script,
                    str(output),
                    json.dumps(
                        [*runner.RUNNER_ENV_FIELDS, runner.RUNNER_DIGEST_ENV]
                    ),
                ],
            )
            with mock.patch.dict(os.environ, environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("late runner discovery"),
                ):
                    self.assertEqual(runner.execute(args), 0)
            observed = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(
                observed,
                {
                    name: identity[field]
                    for name, field in runner.RUNNER_ENV_FIELDS.items()
                }
                | {runner.RUNNER_DIGEST_ENV: runner.digest_value(identity)},
            )

    def test_exit_zero_validates_every_declared_output_and_replaces_foreign_failures(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        foreign_class = (
            "dedicated" if identity["runner_class"] != "dedicated" else "hosted"
        )
        foreign_identity = identity_with_class(identity, foreign_class)
        run_id = "e" * 32
        scenarios = [
            "query_cold",
            "one_file_append",
            "batch_ingest",
            "noop_refresh",
            "incremental_reindex",
            "full_reindex",
            "concurrent_read_write",
            "hook_execution",
        ]
        with tempfile.TemporaryDirectory() as directory:
            receipt_dir = Path(directory)

            # query_cold is missing. Its schema-valid foreign terminal receipt
            # must be atomically replaced with current-run failure evidence.
            runner.write_failure_receipts(
                ["query_cold"],
                "timeout",
                1.0,
                policy["scenario_timeout_seconds"],
                None,
                policy,
                digest,
                receipt_dir,
                "f" * 32,
                identity,
            )
            (receipt_dir / "one_file_append.json").write_text(
                "{malformed", encoding="utf-8"
            )
            (receipt_dir / "batch_ingest.json").write_bytes(b"\xff\xfe\x80")

            schema_invalid = valid_receipt(
                policy, digest, "noop_refresh", run_id, identity
            )
            schema_invalid["unexpected"] = {
                "error": str(Path.home()),
                "argv": "sh -c private",
                "hostname": "worker.secretzone",
            }
            (receipt_dir / "noop_refresh.json").write_text(
                json.dumps(schema_invalid), encoding="utf-8"
            )

            semantic_invalid = valid_receipt(
                policy, digest, "incremental_reindex", run_id, identity
            )
            semantic_invalid["fixture"]["changed_rows"] += 1
            (receipt_dir / "incremental_reindex.json").write_text(
                json.dumps(semantic_invalid), encoding="utf-8"
            )

            wrong_run = valid_receipt(
                policy, digest, "full_reindex", "0" * 32, identity
            )
            (receipt_dir / "full_reindex.json").write_text(
                json.dumps(wrong_run), encoding="utf-8"
            )
            unexpected_throughput = valid_receipt(
                policy, digest, "concurrent_read_write", run_id, identity
            )
            unexpected_throughput["throughput_per_second"] = 1.0
            (receipt_dir / "concurrent_read_write.json").write_text(
                json.dumps(unexpected_throughput), encoding="utf-8"
            )
            wrong_runner = valid_receipt(
                policy, digest, "hook_execution", run_id, foreign_identity
            )
            (receipt_dir / "hook_execution.json").write_text(
                json.dumps(wrong_runner), encoding="utf-8"
            )

            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                directory,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = argparse.Namespace(
                timeout=policy["scenario_timeout_seconds"],
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=suite_start,
                scenario=scenarios,
                command=["--", sys.executable, "-c", "pass"],
            )
            with mock.patch.dict(os.environ, environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("executor rediscovered runner identity"),
                ):
                    with mock.patch.object(runner, "verify_scenario_command"):
                        self.assertEqual(runner.execute(args), 2)

            for scenario in scenarios:
                with self.subTest(scenario=scenario):
                    failure = runner.load_json(receipt_dir / f"{scenario}.failure.json")
                    self.assertEqual(
                        failure["failure_kind"], "evidence_validation_failure"
                    )
                    runner.validate_failure_receipt(
                        failure,
                        scenario,
                        policy,
                        digest,
                        expected_run_id=run_id,
                        expected_runner=identity,
                    )
                    encoded = runner.canonical_bytes(failure)
                    self.assertNotIn(str(Path.home()).encode(), encoded)
                    self.assertNotIn(b"argv", encoded)
                    self.assertNotIn(b"error", encoded)
                    self.assertNotIn(b"sh -c", encoded)
                    self.assertNotIn(b"worker.secretzone", encoded)

            current_failure_path = receipt_dir / "hook_execution.failure.json"
            current_failure_before = current_failure_path.read_bytes()
            current_failure_mtime = current_failure_path.stat().st_mtime_ns
            runner.write_failure_receipts(
                ["hook_execution"],
                "command_failure",
                2.0,
                policy["scenario_timeout_seconds"],
                1,
                policy,
                digest,
                receipt_dir,
                run_id,
                identity,
            )
            self.assertEqual(current_failure_path.read_bytes(), current_failure_before)
            self.assertEqual(
                current_failure_path.stat().st_mtime_ns, current_failure_mtime
            )
            self.assertEqual(list(receipt_dir.glob(".*.tmp")), [])

    def test_exit_zero_preserves_a_valid_failing_receipt_as_terminal(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "9" * 32
        with tempfile.TemporaryDirectory() as directory:
            receipt_dir = Path(directory)
            failing = valid_receipt(policy, digest, "query_cold", run_id, identity)
            failing["samples"]["p99_ms"] = 2_000.0
            failing["guard"]["actual"] = 2_000.0
            failing["guard"]["passed"] = False
            failing["status"] = "fail"
            passing = valid_receipt(policy, digest, "query_warm", run_id, identity)
            fail_path = receipt_dir / "query_cold.json"
            pass_path = receipt_dir / "query_warm.json"
            fail_path.write_text(json.dumps(failing), encoding="utf-8")
            pass_path.write_text(json.dumps(passing), encoding="utf-8")
            before = fail_path.read_bytes()
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                directory,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = argparse.Namespace(
                timeout=policy["scenario_timeout_seconds"],
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=suite_start,
                scenario=["query_cold", "query_warm"],
                command=["--", sys.executable, "-c", "pass"],
            )
            with mock.patch.dict(os.environ, environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("executor rediscovered runner identity"),
                ):
                    with mock.patch.object(runner, "verify_scenario_command"):
                        self.assertEqual(runner.execute(args), 2)
            self.assertEqual(fail_path.read_bytes(), before)
            self.assertEqual(list(receipt_dir.glob("*.failure.json")), [])

    def test_finalizer_uses_trusted_destination_for_invalid_evidence(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "8" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trusted = root / "trusted"
            requested = root / "requested"
            trusted.mkdir()
            requested.mkdir()
            (requested / "private.json").write_text(
                json.dumps({"error": str(Path.home()), "argv": "sh -c private"}),
                encoding="utf-8",
            )
            requested_before = directory_snapshot(requested)
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                trusted,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = argparse.Namespace(
                receipt_dir=str(requested),
                baseline=None,
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=suite_start,
            )
            with mock.patch.dict(os.environ, environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("finalizer rediscovered runner identity"),
                ):
                    self.assertEqual(runner.validate(args), 2)
            self.assertEqual(directory_snapshot(requested), requested_before)
            failure = runner.load_json(trusted / "suite_finalization.failure.json")
            self.assertEqual(failure["failure_kind"], "evidence_validation_failure")
            runner.validate_failure_receipt(
                failure,
                runner.SUITE_FINALIZATION_SCENARIO,
                policy,
                digest,
                expected_run_id=run_id,
                expected_runner=identity,
            )
            encoded = runner.canonical_bytes(failure)
            self.assertNotIn(str(Path.home()).encode(), encoded)
            self.assertNotIn(b"argv", encoded)
            self.assertNotIn(b"error", encoded)
            self.assertEqual(list(trusted.glob(".*.tmp")), [])

            missing = root / "missing"
            missing.mkdir()
            missing_start = time.monotonic()
            missing_environment = harness_environment(
                policy,
                digest,
                missing,
                run_id,
                identity,
                suite_start=missing_start,
            )
            missing_args = argparse.Namespace(
                receipt_dir=str(missing),
                baseline=None,
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=missing_start,
            )
            with mock.patch.dict(os.environ, missing_environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("finalizer rediscovered runner identity"),
                ):
                    self.assertEqual(runner.validate(missing_args), 2)
            missing_failure = runner.load_json(
                missing / "suite_finalization.failure.json"
            )
            self.assertEqual(
                missing_failure["failure_kind"], "evidence_validation_failure"
            )

    def test_finalizer_promotes_only_after_cached_identity_verification(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "4" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_valid_scenario_receipts(root, policy, digest, run_id, identity)
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                root,
                run_id,
                identity,
                suite_start=suite_start,
            )
            environment["PATH"] = ""
            environment["RUSTC"] = str(root / "unavailable-rustc")
            args = finalizer_arguments(policy, root, suite_start)
            with mock.patch.dict(os.environ, environment, clear=True):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError(
                        "finalizer rediscovered runner/toolchain identity"
                    ),
                ):
                    self.assertEqual(runner.validate(args), 0)
            suite = runner.load_json(root / runner.SUITE_RECEIPT_NAME)
            runner.validate_suite_receipt(suite, policy, digest)
            self.assertEqual(suite["runner"], identity)
            self.assertEqual(list(root.glob(".suite-receipt.*.pending")), [])

    def test_deadline_after_assembly_discards_candidate_without_pass_claim(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "3" * 32
        suite_start = 100.0
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_valid_scenario_receipts(root, policy, digest, run_id, identity)
            environment = harness_environment(
                policy,
                digest,
                root,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = finalizer_arguments(policy, root, suite_start)

            def assemble_then_return(_: list[str], __: float) -> int:
                suite = runner.assemble_suite(
                    root, None, policy, digest, run_id, identity
                )
                runner.atomic_write_json(
                    root / runner.staged_suite_receipt_name(run_id), suite
                )
                return 0

            with mock.patch.dict(os.environ, environment, clear=True):
                with mock.patch.object(
                    runner,
                    "run_finalizer_child",
                    side_effect=assemble_then_return,
                ):
                    with mock.patch.object(
                        runner.time,
                        "monotonic",
                        side_effect=[100.0, 1001.0, 1001.0],
                    ):
                        with mock.patch("builtins.print") as output:
                            self.assertEqual(runner.validate(args), 2)
            self.assertFalse((root / runner.SUITE_RECEIPT_NAME).exists())
            self.assertEqual(list(root.glob(".suite-receipt.*.pending")), [])
            failure = runner.load_json(root / "suite_finalization.failure.json")
            self.assertEqual(failure["failure_kind"], "timeout")
            self.assertEqual(failure["elapsed_seconds"], 901.0)
            self.assertFalse(
                any(
                    "validated" in " ".join(str(value) for value in call.args)
                    for call in output.call_args_list
                )
            )

    def test_verifier_timeout_and_cleanup_error_never_publish_candidate(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "2" * 32
        suite_start = 100.0
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for label, second_result, expected_kind in (
                ("verifier-timeout", None, "timeout"),
                (
                    "cleanup-error",
                    runner.ValidationError("sanitized cleanup failure"),
                    "evidence_validation_failure",
                ),
            ):
                with self.subTest(case=label):
                    receipt_dir = root / label
                    receipt_dir.mkdir()
                    write_valid_scenario_receipts(
                        receipt_dir, policy, digest, run_id, identity
                    )
                    environment = harness_environment(
                        policy,
                        digest,
                        receipt_dir,
                        run_id,
                        identity,
                        suite_start=suite_start,
                    )
                    args = finalizer_arguments(policy, receipt_dir, suite_start)
                    calls = 0

                    def child_result(_: list[str], __: float) -> int | None:
                        nonlocal calls
                        calls += 1
                        if calls == 1:
                            suite = runner.assemble_suite(
                                receipt_dir,
                                None,
                                policy,
                                digest,
                                run_id,
                                identity,
                            )
                            runner.atomic_write_json(
                                receipt_dir
                                / runner.staged_suite_receipt_name(run_id),
                                suite,
                            )
                            return 0
                        if isinstance(second_result, Exception):
                            raise second_result
                        return second_result

                    with mock.patch.dict(os.environ, environment, clear=True):
                        with mock.patch.object(
                            runner, "run_finalizer_child", side_effect=child_result
                        ):
                            with mock.patch.object(
                                runner.time, "monotonic", return_value=101.0
                            ):
                                self.assertEqual(runner.validate(args), 2)
                    self.assertFalse(
                        (receipt_dir / runner.SUITE_RECEIPT_NAME).exists()
                    )
                    self.assertEqual(
                        list(receipt_dir.glob(".suite-receipt.*.pending")), []
                    )
                    failure = runner.load_json(
                        receipt_dir / "suite_finalization.failure.json"
                    )
                    self.assertEqual(failure["failure_kind"], expected_kind)

    def test_deadline_is_rechecked_immediately_before_atomic_promotion(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "a" * 32
        suite_start = 100.0
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_valid_scenario_receipts(root, policy, digest, run_id, identity)
            environment = harness_environment(
                policy,
                digest,
                root,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = finalizer_arguments(policy, root, suite_start)
            calls = 0

            def successful_children(_: list[str], __: float) -> int:
                nonlocal calls
                calls += 1
                if calls == 1:
                    suite = runner.assemble_suite(
                        root, None, policy, digest, run_id, identity
                    )
                    runner.atomic_write_json(
                        root / runner.staged_suite_receipt_name(run_id), suite
                    )
                return 0

            with mock.patch.dict(os.environ, environment, clear=True):
                with mock.patch.object(
                    runner, "run_finalizer_child", side_effect=successful_children
                ):
                    with mock.patch.object(
                        runner.time,
                        "monotonic",
                        side_effect=[100.0, 101.0, 102.0, 1001.0, 1001.0],
                    ):
                        self.assertEqual(runner.validate(args), 2)
            self.assertEqual(calls, 2)
            self.assertFalse((root / runner.SUITE_RECEIPT_NAME).exists())
            self.assertEqual(list(root.glob(".suite-receipt.*.pending")), [])
            failure = runner.load_json(root / "suite_finalization.failure.json")
            self.assertEqual(failure["failure_kind"], "timeout")
            self.assertEqual(failure["elapsed_seconds"], 901.0)

    def test_verifier_recomputes_staged_suite_from_source_receipts(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "1" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_valid_scenario_receipts(root, policy, digest, run_id, identity)
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                root,
                run_id,
                identity,
                suite_start=suite_start,
            )
            finalize_args = finalizer_arguments(policy, root, suite_start)
            verify_args = argparse.Namespace(
                **vars(finalize_args),
                suite_name=runner.staged_suite_receipt_name(run_id),
            )
            with mock.patch.dict(os.environ, environment, clear=True):
                self.assertEqual(runner.finalize_worker(finalize_args), 0)
                changed = runner.load_json(root / "query_cold.json")
                changed["samples"]["p50_ms"] = 2.0
                changed["samples"]["p95_ms"] = 2.0
                changed["samples"]["p99_ms"] = 2.0
                changed["guard"]["actual"] = 2.0
                (root / "query_cold.json").write_text(
                    json.dumps(changed), encoding="utf-8"
                )
                self.assertEqual(
                    runner.verify_final_suite_worker(verify_args),
                    runner.FINALIZER_EVIDENCE_EXIT,
                )
            self.assertFalse((root / runner.SUITE_RECEIPT_NAME).exists())

    def test_stale_and_foreign_staging_candidates_are_rejected(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "0" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for label, candidate_run in (
                ("stale", run_id),
                ("foreign", "f" * 32),
            ):
                with self.subTest(case=label):
                    receipt_dir = root / label
                    receipt_dir.mkdir()
                    candidate = (
                        receipt_dir / runner.staged_suite_receipt_name(candidate_run)
                    )
                    candidate.write_text("foreign pending data", encoding="utf-8")
                    suite_start = time.monotonic()
                    environment = harness_environment(
                        policy,
                        digest,
                        receipt_dir,
                        run_id,
                        identity,
                        suite_start=suite_start,
                    )
                    args = finalizer_arguments(policy, receipt_dir, suite_start)
                    with mock.patch.dict(os.environ, environment, clear=True):
                        with mock.patch.object(
                            runner, "run_finalizer_child"
                        ) as child:
                            self.assertEqual(runner.validate(args), 2)
                    child.assert_not_called()
                    self.assertFalse(candidate.exists())
                    failure = runner.load_json(
                        receipt_dir / "suite_finalization.failure.json"
                    )
                    self.assertEqual(
                        failure["failure_kind"], "evidence_validation_failure"
                    )

    def test_invalid_baseline_is_evidence_failure_but_regression_is_comparison_failure(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "7" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            baseline_path = root / "baseline.json"
            invalid_dir = root / "invalid-baseline"
            invalid_dir.mkdir()
            write_valid_scenario_receipts(invalid_dir, policy, digest, run_id, identity)
            baseline_path.write_text("{malformed", encoding="utf-8")
            invalid_start = time.monotonic()
            invalid_environment = harness_environment(
                policy,
                digest,
                invalid_dir,
                run_id,
                identity,
                suite_start=invalid_start,
            )
            args = argparse.Namespace(
                receipt_dir=str(invalid_dir),
                baseline=str(baseline_path),
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=invalid_start,
            )
            with mock.patch.dict(os.environ, invalid_environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("finalizer rediscovered runner identity"),
                ):
                    self.assertEqual(runner.validate(args), 2)
            invalid_failure = runner.load_json(
                invalid_dir / "suite_finalization.failure.json"
            )
            self.assertEqual(
                invalid_failure["failure_kind"], "evidence_validation_failure"
            )

            regression_dir = root / "regression"
            regression_dir.mkdir()
            write_valid_scenario_receipts(
                regression_dir, policy, digest, run_id, identity
            )
            current = runner.load_json(regression_dir / "query_cold.json")
            current["samples"]["p50_ms"] = 1.3
            current["samples"]["p95_ms"] = 1.3
            current["samples"]["p99_ms"] = 1.3
            current["guard"]["actual"] = 1.3
            (regression_dir / "query_cold.json").write_text(
                json.dumps(current), encoding="utf-8"
            )
            baseline_path.write_text(
                json.dumps(valid_baseline(policy, digest, identity)),
                encoding="utf-8",
            )
            regression_start = time.monotonic()
            regression_environment = harness_environment(
                policy,
                digest,
                regression_dir,
                run_id,
                identity,
                suite_start=regression_start,
            )
            args = argparse.Namespace(
                receipt_dir=str(regression_dir),
                baseline=str(baseline_path),
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=regression_start,
            )
            with mock.patch.dict(os.environ, regression_environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("finalizer rediscovered runner identity"),
                ):
                    self.assertEqual(runner.validate(args), 2)
            regression_failure = runner.load_json(
                regression_dir / "suite_finalization.failure.json"
            )
            self.assertEqual(regression_failure["failure_kind"], "comparison_failure")

    def test_finalizer_never_replaces_existing_pass_suite_evidence(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "6" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_valid_scenario_receipts(root, policy, digest, run_id, identity)
            self.assertEqual(
                runner.finalize_suite(root, None, policy, digest, run_id, identity),
                0,
            )
            suite_path = root / "suite-receipt.json"
            before = suite_path.read_bytes()
            before_mtime = suite_path.stat().st_mtime_ns
            with mock.patch.object(runner, "atomic_write_json") as atomic_write:
                self.assertEqual(
                    runner.finalize_suite(root, None, policy, digest, run_id, identity),
                    0,
                )
            atomic_write.assert_not_called()
            self.assertEqual(suite_path.read_bytes(), before)
            self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)

            suite_start = time.monotonic()
            matching_environment = harness_environment(
                policy,
                digest,
                directory,
                run_id,
                identity,
                suite_start=suite_start,
            )
            matching_args = finalizer_arguments(policy, directory, suite_start)
            with mock.patch.dict(os.environ, matching_environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("finalizer rediscovered runner identity"),
                ):
                    self.assertEqual(runner.validate(matching_args), 0)
            self.assertEqual(suite_path.read_bytes(), before)
            self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)

            foreign_suite = valid_suite(policy, digest, "5" * 32, identity)
            suite_path.write_text(json.dumps(foreign_suite), encoding="utf-8")
            foreign_before = suite_path.read_bytes()
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                directory,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = argparse.Namespace(
                receipt_dir=directory,
                baseline=None,
                suite_timeout=policy["suite_timeout_seconds"],
                suite_start=suite_start,
            )
            with mock.patch.dict(os.environ, environment, clear=False):
                with mock.patch.object(
                    runner,
                    "runner_identity",
                    side_effect=AssertionError("finalizer rediscovered runner identity"),
                ):
                    self.assertEqual(runner.validate(args), 2)
            self.assertEqual(suite_path.read_bytes(), foreign_before)
            failure = runner.load_json(root / "suite_finalization.failure.json")
            self.assertEqual(failure["failure_kind"], "evidence_validation_failure")

    def test_accepted_suite_rejects_malformed_or_different_baseline_immutably(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "b" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            malformed = root / "malformed-baseline.json"
            malformed.write_text("{malformed", encoding="utf-8")
            different = root / "different-baseline.json"
            different.write_text(
                json.dumps(valid_baseline(policy, digest, identity)),
                encoding="utf-8",
            )
            for label, baseline in (
                ("malformed", malformed),
                ("different", different),
            ):
                with self.subTest(case=label):
                    receipt_dir = root / label
                    receipt_dir.mkdir()
                    suite_path, before, before_mtime = write_accepted_suite(
                        receipt_dir, policy, digest, run_id, identity
                    )
                    suite_start = time.monotonic()
                    environment = harness_environment(
                        policy,
                        digest,
                        receipt_dir,
                        run_id,
                        identity,
                        suite_start=suite_start,
                    )
                    args = finalizer_arguments(
                        policy, receipt_dir, suite_start, baseline=baseline
                    )
                    with mock.patch.dict(os.environ, environment, clear=True):
                        self.assertEqual(runner.validate(args), 2)
                    self.assertEqual(suite_path.read_bytes(), before)
                    self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)
                    self.assertEqual(list(receipt_dir.glob("*.failure.json")), [])
                    self.assertEqual(
                        list(receipt_dir.glob(".suite-receipt.*.pending")), []
                    )

    def test_accepted_suite_rejects_invalid_clocks_without_companion_failure(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "c" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            now = time.monotonic()
            for label, suite_start in (
                (
                    "expired",
                    now - float(policy["suite_timeout_seconds"]) - 1.0,
                ),
                ("future", now + 60.0),
                ("nonfinite", math.nan),
            ):
                with self.subTest(case=label):
                    receipt_dir = root / label
                    receipt_dir.mkdir()
                    suite_path, before, before_mtime = write_accepted_suite(
                        receipt_dir, policy, digest, run_id, identity
                    )
                    environment = harness_environment(
                        policy,
                        digest,
                        receipt_dir,
                        run_id,
                        identity,
                        suite_start=suite_start,
                    )
                    args = finalizer_arguments(policy, receipt_dir, suite_start)
                    with mock.patch.dict(os.environ, environment, clear=True):
                        with mock.patch.object(
                            runner, "run_finalizer_child"
                        ) as child:
                            self.assertEqual(runner.validate(args), 2)
                    child.assert_not_called()
                    self.assertEqual(suite_path.read_bytes(), before)
                    self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)
                    self.assertEqual(list(receipt_dir.glob("*.failure.json")), [])

    def test_accepted_suite_cleans_stale_pending_without_mutating_evidence(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "d" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite_path, before, before_mtime = write_accepted_suite(
                root, policy, digest, run_id, identity
            )
            for candidate_run in (run_id, "e" * 32):
                (root / runner.staged_suite_receipt_name(candidate_run)).write_text(
                    "transient pending data", encoding="utf-8"
                )
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                root,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = finalizer_arguments(policy, root, suite_start)
            with mock.patch.dict(os.environ, environment, clear=True):
                with mock.patch.object(runner, "run_finalizer_child") as child:
                    self.assertEqual(runner.validate(args), 2)
            child.assert_not_called()
            self.assertEqual(suite_path.read_bytes(), before)
            self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)
            self.assertEqual(list(root.glob("*.failure.json")), [])
            self.assertEqual(list(root.glob(".suite-receipt.*.pending")), [])

    def test_accepted_suite_survives_verifier_timeout_error_and_source_mismatch(
        self,
    ) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "e" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for label, second_result in (
                ("verifier-timeout", None),
                (
                    "verifier-error",
                    runner.ValidationError("sanitized verifier failure"),
                ),
            ):
                with self.subTest(case=label):
                    receipt_dir = root / label
                    receipt_dir.mkdir()
                    suite_path, before, before_mtime = write_accepted_suite(
                        receipt_dir, policy, digest, run_id, identity
                    )
                    suite_start = time.monotonic()
                    environment = harness_environment(
                        policy,
                        digest,
                        receipt_dir,
                        run_id,
                        identity,
                        suite_start=suite_start,
                    )
                    args = finalizer_arguments(policy, receipt_dir, suite_start)
                    with mock.patch.dict(os.environ, environment, clear=True):
                        with mock.patch.object(
                            runner,
                            "run_finalizer_child",
                            side_effect=[0, second_result],
                        ):
                            self.assertEqual(runner.validate(args), 2)
                    self.assertEqual(suite_path.read_bytes(), before)
                    self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)
                    self.assertEqual(list(receipt_dir.glob("*.failure.json")), [])
                    self.assertEqual(
                        list(receipt_dir.glob(".suite-receipt.*.pending")), []
                    )

            mismatch_dir = root / "source-mismatch"
            mismatch_dir.mkdir()
            suite_path, before, before_mtime = write_accepted_suite(
                mismatch_dir, policy, digest, run_id, identity
            )
            changed = runner.load_json(mismatch_dir / "query_cold.json")
            changed["samples"]["p50_ms"] = 2.0
            changed["samples"]["p95_ms"] = 2.0
            changed["samples"]["p99_ms"] = 2.0
            changed["guard"]["actual"] = 2.0
            (mismatch_dir / "query_cold.json").write_text(
                json.dumps(changed), encoding="utf-8"
            )
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                mismatch_dir,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = finalizer_arguments(policy, mismatch_dir, suite_start)
            with mock.patch.dict(os.environ, environment, clear=True):
                self.assertEqual(runner.validate(args), 2)
            self.assertEqual(suite_path.read_bytes(), before)
            self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)
            self.assertEqual(list(mismatch_dir.glob("*.failure.json")), [])

    def test_accepted_baseline_suite_refinalizes_only_with_exact_baseline(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        run_id = "f" * 32
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            baseline = root / "baseline.json"
            baseline.write_text(
                json.dumps(valid_baseline(policy, digest, identity)),
                encoding="utf-8",
            )
            receipt_dir = root / "receipts"
            receipt_dir.mkdir()
            suite_path, before, before_mtime = write_accepted_suite(
                receipt_dir,
                policy,
                digest,
                run_id,
                identity,
                baseline=baseline,
            )
            suite_start = time.monotonic()
            environment = harness_environment(
                policy,
                digest,
                receipt_dir,
                run_id,
                identity,
                suite_start=suite_start,
            )
            args = finalizer_arguments(
                policy, receipt_dir, suite_start, baseline=baseline
            )
            with mock.patch.dict(os.environ, environment, clear=True):
                self.assertEqual(runner.validate(args), 0)
            self.assertEqual(suite_path.read_bytes(), before)
            self.assertEqual(suite_path.stat().st_mtime_ns, before_mtime)
            self.assertEqual(list(receipt_dir.glob("*.failure.json")), [])

    def test_atomic_write_cleans_up_an_unpublished_temporary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            destination = root / "evidence.json"
            with mock.patch.object(Path, "replace", side_effect=OSError("private")):
                with self.assertRaisesRegex(
                    runner.ValidationError, "unable to publish"
                ):
                    runner.atomic_write_json(destination, {"status": "pass"})
            self.assertFalse(destination.exists())
            self.assertEqual(list(root.iterdir()), [])

    def test_duplicate_baseline_input_paths_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "suite-receipt.json"
            args = argparse.Namespace(
                output=str(Path(directory) / "baseline.json"),
                receipts=[str(path), str(path), str(path)],
            )
            with self.assertRaisesRegex(runner.ValidationError, "distinct files"):
                runner.create_baseline(args)

    def test_duplicate_baseline_run_id_and_source_suite_are_rejected(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            run_ids = ["1" * 32, "1" * 32, "2" * 32]
            for index, run_id in enumerate(run_ids):
                path = root / f"suite-{index}.json"
                path.write_text(
                    json.dumps(valid_suite(policy, digest, run_id, identity)),
                    encoding="utf-8",
                )
                paths.append(path)
            with self.assertRaisesRegex(
                runner.ValidationError, "unique run identities"
            ):
                runner.create_baseline(
                    argparse.Namespace(
                        output=str(root / "baseline.json"),
                        receipts=[str(path) for path in paths],
                    )
                )

            suites = [runner.load_json(path) for path in paths]
            suites[1] = copy.deepcopy(suites[0])
            baseline = {
                "schema": "staxtrace.performance-baseline",
                "policy_digest": digest,
                "runner": identity,
                "clean_run_count": 3,
                "run_ids": [suite["run_id"] for suite in suites],
                "suite_digests": [runner.digest_value(suite) for suite in suites],
                "source_suites": suites,
                "scenarios": runner.derive_baseline_scenarios(
                    [
                        runner.validate_suite_receipt(suite, policy, digest)
                        for suite in suites
                    ],
                    policy,
                ),
                "status": "pass",
            }
            with self.assertRaises(runner.ValidationError):
                runner.validate_baseline(baseline, policy, digest)

    def test_three_distinct_suites_create_a_strict_median_baseline(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite_paths = []
            for index in range(3):
                path = root / f"suite-{index}.json"
                path.write_text(
                    json.dumps(
                        valid_suite(policy, digest, f"{index + 1:032x}", identity)
                    ),
                    encoding="utf-8",
                )
                suite_paths.append(path)
            output = root / "baseline.json"
            runner.create_baseline(
                argparse.Namespace(
                    output=str(output), receipts=[str(path) for path in suite_paths]
                )
            )
            baseline = runner.load_json(output)
            source_suites = [runner.load_json(path) for path in suite_paths]
            self.assertEqual(baseline["source_suites"], source_suites)
            self.assertEqual(
                baseline["suite_digests"],
                [runner.digest_value(suite) for suite in source_suites],
            )
            runner.validate_baseline(baseline, policy, digest)

            tampered_metric = copy.deepcopy(baseline)
            tampered_metric["scenarios"]["query_cold"]["p99_ms"] += 0.25
            with self.assertRaisesRegex(runner.ValidationError, "stored medians"):
                runner.validate_baseline(tampered_metric, policy, digest)
            tampered_digest = copy.deepcopy(baseline)
            tampered_digest["suite_digests"][0] = "f" * 64
            with self.assertRaisesRegex(runner.ValidationError, "stored suite digests"):
                runner.validate_baseline(tampered_digest, policy, digest)
            nonfinite = copy.deepcopy(baseline)
            nonfinite["scenarios"]["query_cold"]["p99_ms"] = math.inf
            with self.assertRaises(runner.ValidationError):
                runner.validate_baseline(nonfinite, policy, digest)
            tampered_source = copy.deepcopy(baseline)
            source_receipt = tampered_source["source_suites"][0]["scenarios"][0]
            source_receipt["samples"]["p99_ms"] = math.inf
            source_name = source_receipt["scenario"]
            tampered_source["source_suites"][0]["scenario_digests"][source_name] = (
                runner.digest_value(source_receipt)
            )
            tampered_source["suite_digests"][0] = runner.digest_value(
                tampered_source["source_suites"][0]
            )
            with self.assertRaises(runner.ValidationError):
                runner.validate_baseline(tampered_source, policy, digest)

            output.write_text(json.dumps(tampered_metric), encoding="utf-8")
            with self.assertRaisesRegex(runner.ValidationError, "stored medians"):
                runner.compare_baseline(
                    source_suites[0]["scenarios"], output, policy, digest
                )
            output.write_text(json.dumps(baseline), encoding="utf-8")
            before = output.read_bytes()
            before_mtime = output.stat().st_mtime_ns
            before_tree = directory_snapshot(root)
            runner.validate_baseline_command(
                argparse.Namespace(receipt=str(output), require_current_runner=True)
            )
            command = [
                "python3",
                str(MODULE_PATH),
                "validate-baseline",
                "--receipt",
                str(output),
                "--require-current-runner",
            ]
            valid = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertEqual(valid.returncode, 0)
            self.assertEqual(json.loads(valid.stdout)["outcome"], "comparable")
            self.assertNotIn(str(root), valid.stdout + valid.stderr)
            self.assertEqual(output.read_bytes(), before)
            self.assertEqual(output.stat().st_mtime_ns, before_mtime)
            self.assertEqual(directory_snapshot(root), before_tree)

            hosted_identity = identity_with_class(identity, "hosted")
            for index, path in enumerate(suite_paths):
                path.write_text(
                    json.dumps(
                        valid_suite(
                            policy, digest, f"{index + 4:032x}", hosted_identity
                        )
                    ),
                    encoding="utf-8",
                )
            runner.create_baseline(
                argparse.Namespace(
                    output=str(output), receipts=[str(path) for path in suite_paths]
                )
            )
            mismatch_before = directory_snapshot(root)
            mismatch = subprocess.run(
                command, check=False, capture_output=True, text=True
            )
            self.assertEqual(mismatch.returncode, 3)
            self.assertEqual(json.loads(mismatch.stdout)["outcome"], "incompatible")
            self.assertEqual(directory_snapshot(root), mismatch_before)

            output.write_text("{}", encoding="utf-8")
            invalid_before = directory_snapshot(root)
            invalid = subprocess.run(
                command, check=False, capture_output=True, text=True
            )
            self.assertEqual(invalid.returncode, 2)
            self.assertEqual(json.loads(invalid.stdout)["outcome"], "invalid")
            self.assertEqual(directory_snapshot(root), invalid_before)

    def test_standalone_validation_has_typed_outcomes(self) -> None:
        policy, digest = runner.load_policy()
        identity = runner.runner_identity(policy)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite = root / "suite.json"
            suite.write_text(
                json.dumps(valid_suite(policy, digest, "1" * 32, identity)),
                encoding="utf-8",
            )
            command = [
                "python3",
                str(MODULE_PATH),
                "validate-suite",
                "--receipt",
                str(suite),
                "--require-current-runner",
            ]
            before = suite.read_bytes()
            before_mtime = suite.stat().st_mtime_ns
            before_tree = directory_snapshot(root)
            valid = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertEqual(valid.returncode, 0)
            self.assertEqual(json.loads(valid.stdout)["outcome"], "comparable")
            self.assertNotIn(str(root), valid.stdout + valid.stderr)
            self.assertEqual(suite.read_bytes(), before)
            self.assertEqual(suite.stat().st_mtime_ns, before_mtime)
            self.assertEqual(directory_snapshot(root), before_tree)

            incompatible = copy.deepcopy(runner.load_json(suite))
            incompatible["runner"] = identity_with_class(identity, "hosted")
            for receipt in incompatible["scenarios"]:
                receipt["runner"] = copy.deepcopy(incompatible["runner"])
                incompatible["scenario_digests"][receipt["scenario"]] = (
                    runner.digest_value(receipt)
                )
            suite.write_text(json.dumps(incompatible), encoding="utf-8")
            mismatch_before = directory_snapshot(root)
            mismatch = subprocess.run(
                command, check=False, capture_output=True, text=True
            )
            self.assertEqual(mismatch.returncode, 3)
            self.assertEqual(json.loads(mismatch.stdout)["outcome"], "incompatible")
            self.assertEqual(directory_snapshot(root), mismatch_before)

            suite.write_text("{}", encoding="utf-8")
            invalid_before = directory_snapshot(root)
            invalid = subprocess.run(
                command, check=False, capture_output=True, text=True
            )
            self.assertEqual(invalid.returncode, 2)
            self.assertEqual(json.loads(invalid.stdout)["outcome"], "invalid")
            self.assertEqual(directory_snapshot(root), invalid_before)

    def test_process_group_is_reaped(self) -> None:
        process = subprocess.Popen(
            ["sh", "-c", "trap '' TERM; sleep 60 & wait"],
            start_new_session=True,
        )
        runner.terminate_process_group(process)
        self.assertIsNotNone(process.poll())
        self.assertFalse(runner.process_group_exists(process.pid))

    def test_finalizer_timeout_kills_sigterm_ignoring_process_tree(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            process_group_file = Path(directory) / "process-group.txt"
            descendant = (
                "import signal,time;"
                "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
                "time.sleep(60)"
            )
            parent = (
                "import os,pathlib,signal,subprocess,sys,time;"
                "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
                "subprocess.Popen([sys.executable,'-c',sys.argv[2]]);"
                "pathlib.Path(sys.argv[1]).write_text(str(os.getpid()),encoding='utf-8');"
                "time.sleep(60)"
            )
            self.assertIsNone(
                runner.run_finalizer_child(
                    [
                        sys.executable,
                        "-c",
                        parent,
                        str(process_group_file),
                        descendant,
                    ],
                    0.5,
                )
            )
            process_group = int(process_group_file.read_text(encoding="utf-8"))
            self.assertFalse(runner.process_group_exists(process_group))


if __name__ == "__main__":
    unittest.main()
