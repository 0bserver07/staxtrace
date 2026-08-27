#!/usr/bin/env python3
"""Deterministic control-flow tests for hosted performance qualification."""

from __future__ import annotations

import argparse
import copy
import importlib.util
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile

PERF = Path(__file__).resolve().parent
CI_SPEC = importlib.util.spec_from_file_location("stax_performance_ci", PERF / "ci.py")
assert CI_SPEC is not None and CI_SPEC.loader is not None
ci = importlib.util.module_from_spec(CI_SPEC)
CI_SPEC.loader.exec_module(ci)

RUN_SPEC = importlib.util.spec_from_file_location(
    "stax_performance_test_helpers", PERF / "test_run.py"
)
assert RUN_SPEC is not None and RUN_SPEC.loader is not None
run_helpers = importlib.util.module_from_spec(RUN_SPEC)
RUN_SPEC.loader.exec_module(run_helpers)

FIXTURES = PERF / "testdata" / "ci"
COMMIT = "a" * 40
TEST_RUNNER_LABEL = "hosted-performance"


def write_json(path: Path, value: dict) -> None:
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def source_rows() -> list[dict]:
    rows = []
    for index in range(3):
        run_id = 11 + index
        rows.append(
            {
                "workflow_path": ci.WORKFLOW_PATH,
                "runner_label": TEST_RUNNER_LABEL,
                "workflow_id": 7,
                "workflow_run_id": run_id,
                "workflow_run_attempt": 1,
                "source_commit": COMMIT,
                "artifact_id": 101 + index,
                "artifact_name": ci.artifact_name(run_id, 1),
                "artifact_size": 100 + index,
                "artifact_digest": f"sha256:{index + 1:064x}",
                "artifact_tree_digest": f"{index + 4:064x}",
                "step_proof_digest": "f" * 64,
            }
        )
    return rows


def protected_resolution() -> dict:
    sources = source_rows()
    manifest = ci.protected_manifest_from_sources(sources)
    result = ci.resolution_template("protected_candidates", "protected_manifest")
    result["protected_manifest_digest"] = ci.hashlib.sha256(
        manifest.encode("utf-8")
    ).hexdigest()
    result["protected_sources"] = sources
    result["resolution_proof_digest"] = ci.evidence.digest_value(
        {
            "protected_manifest_digest": result["protected_manifest_digest"],
            "protected_sources": sources,
        }
    )
    ci.validate_resolution(result)
    return result


def selected_resolution(
    run_id: int = 50, attempt: int = 1, tree_digest: str | None = None
) -> dict:
    result = ci.resolution_template("selected", "prior_run")
    result.update(
        {
            "selector": f"{run_id}:{attempt}",
            "workflow_id": 7,
            "workflow_run_id": run_id,
            "workflow_run_attempt": attempt,
            "source_commit": COMMIT,
            "artifact_id": 900,
            "artifact_name": ci.artifact_name(run_id, attempt),
            "artifact_size": 1000,
            "artifact_digest": f"sha256:{9:064x}",
            "artifact_tree_digest": tree_digest or "e" * 64,
        }
    )
    result["resolution_proof_digest"] = ci.evidence.digest_value(
        ci.ordinary_resolution_projection(result)
    )
    ci.validate_resolution(result)
    return result


def baseline_fixture(policy: dict, digest: str, identity: dict) -> dict:
    return run_helpers.valid_baseline(policy, digest, identity)


def candidate_stage(
    root: Path,
    policy: dict,
    digest: str,
    identity: dict,
    *,
    workflow_run: int = 11,
) -> Path:
    suite = run_helpers.valid_suite(policy, digest, f"{workflow_run:032x}", identity)
    resolution = ci.resolution_template("no_selector", "none")
    qualification = ci.qualification_value(
        state="baseline_candidate",
        reason="no_selector",
        source="none",
        selector=None,
        manifest_digest=None,
        resolution=resolution,
        current_run=workflow_run,
        current_attempt=1,
        current_commit=COMMIT,
        absolute_suite=suite,
        suite=suite,
        policy=policy,
        policy_digest=digest,
        baseline=None,
        provenance=None,
        baseline_resolution=None,
    )
    stage = root / f"candidate-{workflow_run}"
    ci.publish_stage(stage, qualification, resolution, suite, None, None, None)
    return stage


def qualified_stage(
    root: Path,
    policy: dict,
    digest: str,
    identity: dict,
    *,
    workflow_run: int = 50,
) -> Path:
    baseline = baseline_fixture(policy, digest, identity)
    resolution = protected_resolution()
    provenance = ci.create_provenance(
        baseline,
        policy,
        digest,
        workflow_run_id=workflow_run,
        workflow_run_attempt=1,
        source_commit=COMMIT,
        creation_manifest_digest=resolution["protected_manifest_digest"],
        creation_resolution_proof_digest=resolution["resolution_proof_digest"],
        protected_sources=resolution["protected_sources"],
    )
    absolute = run_helpers.valid_suite(policy, digest, f"{99:032x}", identity)
    baseline_root = root / f"baseline-source-{workflow_run}"
    baseline_root.mkdir()
    baseline_path = baseline_root / ci.BASELINE_NAME
    write_json(baseline_path, baseline)
    suite = ci.exact_comparison_suite(absolute, baseline, baseline_path, policy, digest)
    qualification = ci.qualification_value(
        state="qualified",
        reason="comparable",
        source="protected_manifest",
        selector=None,
        manifest_digest=resolution["protected_manifest_digest"],
        resolution=resolution,
        current_run=workflow_run,
        current_attempt=1,
        current_commit=COMMIT,
        absolute_suite=absolute,
        suite=suite,
        policy=policy,
        policy_digest=digest,
        baseline=baseline,
        provenance=provenance,
        baseline_resolution=resolution,
    )
    stage = root / f"qualified-{workflow_run}"
    ci.publish_stage(
        stage,
        qualification,
        resolution,
        suite,
        baseline,
        provenance,
        resolution,
    )
    return stage


def zip_tree(root: Path) -> bytes:
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(root.iterdir()):
            archive.writestr(path.name, path.read_bytes())
    return stream.getvalue()


def ci_cli(
    *arguments: str, environment: dict[str, str] | None = None
) -> subprocess.CompletedProcess:
    child_environment = ci.os.environ.copy()
    if environment:
        child_environment.update(environment)
    return subprocess.run(
        [sys.executable, str(PERF / "ci.py"), *arguments],
        cwd=PERF.parent,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
        env=child_environment,
    )


class CiFixtureCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.policy, cls.digest = ci.evidence.load_policy()
        cls.identity = ci.evidence.runner_identity(cls.policy)

    def setUp(self) -> None:
        self.runner_label_environment = mock.patch.dict(
            ci.os.environ, {ci.RUNNER_LABEL_ENV: TEST_RUNNER_LABEL}, clear=False
        )
        self.runner_label_environment.start()
        self.addCleanup(self.runner_label_environment.stop)

    def current_run_fixture(self) -> dict:
        return json.loads((FIXTURES / "run-current.json").read_text())

    def workflow_fixture(self) -> dict:
        return json.loads((FIXTURES / "workflow-current.json").read_text())


class SchemaAndStateTests(CiFixtureCase):
    def test_schema_pins_and_strict_runtime_runner_label(self) -> None:
        ci.verify_ci_schema_pins()
        self.assertEqual(ci.configured_runner_label(), TEST_RUNNER_LABEL)
        for value in ("", "bad label", "../runner", "x" * 129):
            with (
                self.subTest(value=value[:16]),
                mock.patch.dict(
                    ci.os.environ, {ci.RUNNER_LABEL_ENV: value}, clear=False
                ),
                self.assertRaises(ci.CiError),
            ):
                ci.configured_runner_label()

    def test_stage_commands_independently_enforce_schema_pins(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stage = candidate_stage(root, self.policy, self.digest, self.identity)
            changed_schema = copy.deepcopy(
                ci.evidence.load_json(ci.CI_SCHEMA_PATHS["qualification"])
            )
            changed_schema["title"] += " drift"
            schema_path = root / "changed-qualification-schema.json"
            write_json(schema_path, changed_schema)
            with mock.patch.dict(
                ci.CI_SCHEMA_PATHS, {"qualification": schema_path}, clear=False
            ):
                commands = (
                    lambda: ci.enforce_command(
                        argparse.Namespace(staging_dir=str(stage))
                    ),
                    lambda: ci.validate_stage_command(
                        argparse.Namespace(
                            staging_dir=str(stage), require_qualified=False
                        )
                    ),
                )
                for command in commands:
                    with (
                        self.subTest(command=command),
                        self.assertRaisesRegex(ci.CiError, "independent digest pin"),
                    ):
                        command()

    def test_no_selector_candidate_is_valid_but_enforcement_is_nonzero(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            stage = candidate_stage(
                Path(directory), self.policy, self.digest, self.identity
            )
            qualification, _, baseline, provenance, baseline_resolution = (
                ci.validate_stage(stage)
            )
            self.assertEqual(qualification["qualification"], "baseline_candidate")
            self.assertIsNone(baseline)
            self.assertIsNone(provenance)
            self.assertIsNone(baseline_resolution)
            self.assertEqual(
                ci.enforce_command(argparse.Namespace(staging_dir=str(stage))), 4
            )

    def test_candidate_and_pending_are_rejected_as_baselines(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            candidate = candidate_stage(root, self.policy, self.digest, self.identity)
            with self.assertRaisesRegex(ci.CiError, "not qualified"):
                ci.validate_stage(candidate, require_qualified=True)

            suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{77:032x}", self.identity
            )
            resolution = selected_resolution()
            qualification = ci.qualification_value(
                state="pending_comparable_baseline",
                reason="incompatible_runner",
                source="prior_run",
                selector=resolution["selector"],
                manifest_digest=None,
                resolution=resolution,
                current_run=60,
                current_attempt=1,
                current_commit=COMMIT,
                absolute_suite=suite,
                suite=suite,
                policy=self.policy,
                policy_digest=self.digest,
                baseline=None,
                provenance=None,
                baseline_resolution=None,
            )
            pending = root / "pending"
            ci.publish_stage(
                pending, qualification, resolution, suite, None, None, None
            )
            with self.assertRaisesRegex(ci.CiError, "not qualified"):
                ci.validate_stage(pending, require_qualified=True)

    def test_state_matrix_rejects_comparison_suite_as_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            qualified = qualified_stage(root, self.policy, self.digest, self.identity)
            suite = ci.evidence.load_json(qualified / ci.SUITE_NAME)
            resolution = ci.resolution_template("no_selector", "none")
            with self.assertRaisesRegex(ci.CiError, "non-qualified evidence"):
                ci.qualification_value(
                    state="baseline_candidate",
                    reason="no_selector",
                    source="none",
                    selector=None,
                    manifest_digest=None,
                    resolution=resolution,
                    current_run=70,
                    current_attempt=1,
                    current_commit=COMMIT,
                    absolute_suite=ci.absolute_suite_from_comparison(
                        suite, self.policy, self.digest
                    ),
                    suite=suite,
                    policy=self.policy,
                    policy_digest=self.digest,
                    baseline=None,
                    provenance=None,
                    baseline_resolution=None,
                )

    def test_extra_field_and_state_shape_are_rejected(self) -> None:
        value = ci.resolution_template("no_selector", "none")
        value["extra"] = True
        with self.assertRaises(ci.evidence.ValidationError):
            ci.validate_resolution(value)
        value = ci.resolution_template("no_selector", "none")
        value["selector"] = "1:1"
        with self.assertRaisesRegex(ci.CiError, "inconsistent"):
            ci.validate_resolution(value)

    def test_stage_rejects_extra_symlink_and_oversize(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stage = candidate_stage(root, self.policy, self.digest, self.identity)
            (stage / "extra.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(ci.CiError, "allowlist"):
                ci.validate_stage(stage)

            clean = candidate_stage(
                root, self.policy, self.digest, self.identity, workflow_run=12
            )
            (clean / ci.RESOLUTION_NAME).unlink()
            (clean / ci.RESOLUTION_NAME).symlink_to(clean / ci.QUALIFICATION_NAME)
            with self.assertRaisesRegex(ci.CiError, "non-regular"):
                ci.validate_stage(clean)

            oversized = candidate_stage(
                root, self.policy, self.digest, self.identity, workflow_run=13
            )
            (oversized / ci.RESOLUTION_NAME).write_bytes(b"x" * (ci.MAX_FILE_BYTES + 1))
            with self.assertRaisesRegex(ci.CiError, "size"):
                ci.validate_stage(oversized)

            source = candidate_stage(
                root, self.policy, self.digest, self.identity, workflow_run=14
            )
            qualification, suite, _, _, _ = ci.validate_stage(source)
            resolution = ci.evidence.load_json(source / ci.RESOLUTION_NAME)
            real_parent = root / "real-parent"
            real_parent.mkdir()
            linked_parent = root / "linked-parent"
            linked_parent.symlink_to(real_parent, target_is_directory=True)
            with self.assertRaisesRegex(ci.CiError, "trusted directory"):
                ci.publish_stage(
                    linked_parent / "stage",
                    qualification,
                    resolution,
                    suite,
                    None,
                    None,
                    None,
                )


class SelectorAndResolverTests(CiFixtureCase):
    def resolver_args(self, root: Path, selector: str = "41:2") -> argparse.Namespace:
        return argparse.Namespace(
            selector=selector,
            repository="owner/project",
            current_run_id="90",
            current_run_attempt="1",
            current_commit=COMMIT,
            selected_dir=str(root / "selected"),
        )

    def run_fixture(self) -> dict:
        return json.loads((FIXTURES / "run-success.json").read_text())

    def test_selector_is_canonical_and_distinct(self) -> None:
        self.assertEqual(ci.parse_selector("41:2"), (41, 2))
        for value in (
            " 41:2",
            "41:2 ",
            "+41:2",
            "41/2",
            "../41:2",
            "0:1",
            "1:0",
            "9223372036854775808:1",
            "1:2147483648",
        ):
            with self.subTest(value=value), self.assertRaises(ci.CiError):
                ci.parse_selector(value)
        with tempfile.TemporaryDirectory() as directory:
            args = self.resolver_args(Path(directory), "90:1")
            with self.assertRaisesRegex(ci.CiError, "distinct"):
                ci.resolve_ordinary(args, metadata_fetcher=lambda _: self.run_fixture())

            no_selector = self.resolver_args(Path(directory), "")
            result = ci.resolve_ordinary(
                no_selector,
                metadata_fetcher=lambda _: (_ for _ in ()).throw(
                    AssertionError("no-selector resolution must be API independent")
                ),
            )
            self.assertEqual(result, ci.resolution_template("no_selector", "none"))

    def test_confirmed_zero_and_expiry_are_pending_states(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            responses = [
                self.current_run_fixture(),
                self.workflow_fixture(),
                self.run_fixture(),
                json.loads((FIXTURES / "artifacts-empty.json").read_text()),
            ]
            result = ci.resolve_ordinary(
                self.resolver_args(root), metadata_fetcher=lambda _: responses.pop(0)
            )
            self.assertEqual(result["state"], "artifact_unavailable")
            ci.validate_resolution(result)

            run = self.run_fixture()
            artifact = {
                "id": 5,
                "name": ci.artifact_name(41, 2),
                "size_in_bytes": 100,
                "expired": True,
                "digest": f"sha256:{5:064x}",
                "workflow_run": {"id": 41, "head_sha": COMMIT},
            }
            responses = [
                self.current_run_fixture(),
                self.workflow_fixture(),
                run,
                {"total_count": 1, "artifacts": [artifact]},
            ]
            expired = ci.resolve_ordinary(
                self.resolver_args(root), metadata_fetcher=lambda _: responses.pop(0)
            )
            self.assertEqual(expired["state"], "artifact_expired")
            ci.validate_resolution(expired)

    def test_ordinary_resolver_uses_exact_attempt_and_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            payload = zip_tree(
                qualified_stage(
                    root, self.policy, self.digest, self.identity, workflow_run=41
                )
            )
            artifact = {
                "id": 501,
                "name": ci.artifact_name(41, 1),
                "size_in_bytes": len(payload),
                "expired": False,
                "digest": f"sha256:{ci.hashlib.sha256(payload).hexdigest()}",
                "workflow_run": {"id": 41, "head_sha": COMMIT},
            }
            endpoints: list[str] = []

            def metadata_fetcher(endpoint: str) -> dict:
                endpoints.append(endpoint)
                if endpoint == "repos/owner/project/actions/runs/90/attempts/1":
                    return self.current_run_fixture()
                if endpoint == "repos/owner/project/actions/workflows/7":
                    return self.workflow_fixture()
                if endpoint == "repos/owner/project/actions/runs/41/attempts/1":
                    run = self.run_fixture()
                    run["run_attempt"] = 1
                    return run
                if endpoint == (
                    "repos/owner/project/actions/runs/41/artifacts?"
                    f"name={ci.artifact_name(41, 1)}&per_page=100"
                ):
                    return {"total_count": 1, "artifacts": [artifact]}
                raise AssertionError(f"unexpected metadata endpoint: {endpoint}")

            archive_endpoints: list[str] = []

            def archive_fetcher(endpoint: str) -> bytes:
                archive_endpoints.append(endpoint)
                return payload

            resolution = ci.resolve_ordinary(
                self.resolver_args(root, "41:1"),
                metadata_fetcher=metadata_fetcher,
                archive_fetcher=archive_fetcher,
            )
            ci.validate_resolution(resolution)
            ci.validate_stage(
                root / "selected", require_qualified=True, expected_run=resolution
            )
            self.assertEqual(
                endpoints,
                [
                    "repos/owner/project/actions/runs/90/attempts/1",
                    "repos/owner/project/actions/workflows/7",
                    "repos/owner/project/actions/runs/41/attempts/1",
                    "repos/owner/project/actions/runs/41/artifacts?"
                    f"name={ci.artifact_name(41, 1)}&per_page=100",
                ],
            )
            self.assertEqual(
                archive_endpoints,
                ["repos/owner/project/actions/artifacts/501/zip"],
            )
            selected_file = root / "selected" / ci.QUALIFICATION_NAME
            with self.assertRaises(PermissionError):
                selected_file.write_bytes(selected_file.read_bytes() + b"\n")
            (root / "selected").chmod(0o700)
            with self.assertRaisesRegex(ci.CiError, "sealed read-only"):
                ci.validate_stage(
                    root / "selected",
                    require_qualified=True,
                    expected_run=resolution,
                    expected_tree_digest=resolution["artifact_tree_digest"],
                    defer_selected_qualified_runner=True,
                )
            (root / "selected").chmod(0o500)
            selected_file.chmod(0o600)
            with self.assertRaisesRegex(ci.CiError, "sealed read-only"):
                ci.validate_stage(
                    root / "selected",
                    require_qualified=True,
                    expected_run=resolution,
                    expected_tree_digest=resolution["artifact_tree_digest"],
                    defer_selected_qualified_runner=True,
                )
            selected_file.write_bytes(selected_file.read_bytes() + b"\n")
            selected_file.chmod(0o400)
            with self.assertRaisesRegex(ci.CiError, "tree digest"):
                ci.validate_stage(
                    root / "selected",
                    require_qualified=True,
                    expected_run=resolution,
                    expected_tree_digest=resolution["artifact_tree_digest"],
                    defer_selected_qualified_runner=True,
                )
            ci.remove_evidence_tree(root / "selected")

    def test_duplicate_wrong_workflow_attempt_commit_and_failure_are_hard(self) -> None:
        mutations = {
            "workflow": {"path": ".github/workflows/other.yml"},
            "workflow_id": {"workflow_id": 8},
            "attempt": {"run_attempt": 3},
            "invalid_commit": {"head_sha": "not-a-commit"},
            "failure": {"conclusion": "failure"},
        }
        for label, mutation in mutations.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                run = self.run_fixture()
                run.update(mutation)
                responses = [
                    self.current_run_fixture(),
                    self.workflow_fixture(),
                    run,
                ]
                with self.assertRaises(ci.CiError):
                    ci.resolve_ordinary(
                        self.resolver_args(Path(directory)),
                        metadata_fetcher=lambda _: responses.pop(0),
                    )

        artifact = {
            "id": 1,
            "name": ci.artifact_name(41, 2),
            "size_in_bytes": 1,
            "expired": False,
            "digest": f"sha256:{1:064x}",
            "workflow_run": {"id": 41, "head_sha": COMMIT},
        }
        for label, artifacts in (
            ("duplicate", [artifact, copy.deepcopy(artifact)]),
            (
                "artifact_commit",
                [{**artifact, "workflow_run": {"id": 41, "head_sha": "b" * 40}}],
            ),
        ):
            with self.subTest(label=label), tempfile.TemporaryDirectory() as directory:
                responses = [
                    self.current_run_fixture(),
                    self.workflow_fixture(),
                    self.run_fixture(),
                    {"total_count": len(artifacts), "artifacts": artifacts},
                ]
                with self.assertRaises(ci.CiError):
                    ci.resolve_ordinary(
                        self.resolver_args(Path(directory)),
                        metadata_fetcher=lambda _: responses.pop(0),
                    )

        with tempfile.TemporaryDirectory() as directory:
            prior_commit = "b" * 40
            run = self.run_fixture()
            run["head_sha"] = prior_commit
            prior_artifact = {
                **artifact,
                "workflow_run": {"id": 41, "head_sha": prior_commit},
            }
            responses = [
                self.current_run_fixture(),
                self.workflow_fixture(),
                run,
                {"total_count": 1, "artifacts": [prior_artifact]},
            ]
            with mock.patch.object(ci, "download_and_extract", return_value="e" * 64):
                selected = ci.resolve_ordinary(
                    self.resolver_args(Path(directory)),
                    metadata_fetcher=lambda _: responses.pop(0),
                )
            self.assertEqual(selected["source_commit"], prior_commit)

    def test_current_run_and_workflow_identity_anchor_are_strict(self) -> None:
        current_mutations = [
            {"id": 91},
            {"run_attempt": 2},
            {"workflow_id": 8},
            {"path": ".github/workflows/perf.yml@refs/heads/main@other"},
            {"path": ".github/workflows/perf.yml@refs//heads/main"},
            {"head_sha": "b" * 40},
            {"status": "completed"},
            {"conclusion": "success"},
        ]
        for mutation in current_mutations:
            with (
                self.subTest(current=mutation),
                tempfile.TemporaryDirectory() as directory,
            ):
                current = self.current_run_fixture()
                current.update(mutation)
                responses = [current, self.workflow_fixture()]
                with self.assertRaises(ci.CiError):
                    ci.resolve_ordinary(
                        self.resolver_args(Path(directory)),
                        metadata_fetcher=lambda _: responses.pop(0),
                    )
        workflow_mutations = [
            {"id": 8},
            {"path": ".github/workflows/other.yml"},
            {"state": "disabled_manually"},
        ]
        for mutation in workflow_mutations:
            with (
                self.subTest(workflow=mutation),
                tempfile.TemporaryDirectory() as directory,
            ):
                workflow = self.workflow_fixture()
                workflow.update(mutation)
                responses = [self.current_run_fixture(), workflow]
                with self.assertRaises(ci.CiError):
                    ci.resolve_ordinary(
                        self.resolver_args(Path(directory)),
                        metadata_fetcher=lambda _: responses.pop(0),
                    )

    def test_uncertain_transport_and_unknown_digest_are_hard(self) -> None:
        outcomes = [
            subprocess.CompletedProcess([], 1, "", ""),
            subprocess.CompletedProcess([], 0, "{}", "uncertain"),
            subprocess.TimeoutExpired(["gh", "api"], 1),
        ]
        for outcome in outcomes:
            with self.subTest(outcome=type(outcome).__name__):
                effect = (
                    {"side_effect": outcome}
                    if isinstance(outcome, subprocess.TimeoutExpired)
                    else {"return_value": outcome}
                )
                with mock.patch.object(ci.subprocess, "run", **effect):
                    with self.assertRaises(ci.CiError):
                        ci.gh_json("repos/owner/project/actions/runs/1")
        for digest in ("md5:" + "0" * 64, "SHA256:" + "0" * 64, "0" * 64):
            with self.subTest(digest=digest), self.assertRaises(ci.CiError):
                ci.parse_api_digest(digest)

    def test_hosted_helpers_send_only_the_json_accept_header(self) -> None:
        endpoint = "repos/owner/project/actions/runs/1"
        expected = [
            "gh",
            "api",
            "--method",
            "GET",
            "-H",
            "Accept: application/vnd.github+json",
            endpoint,
        ]
        with mock.patch.object(
            ci.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, "{}", ""),
        ) as invocation:
            self.assertEqual(ci.gh_json(endpoint), {})
            self.assertEqual(invocation.call_args.args[0], expected)
        with mock.patch.object(
            ci.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, b"x", b""),
        ) as invocation:
            self.assertEqual(ci.gh_archive(endpoint), b"x")
            self.assertEqual(invocation.call_args.args[0], expected)

    def test_hosted_metadata_uses_strict_bounded_json(self) -> None:
        deep = '{"untrusted_marker":' + "[" * 70 + "null" + "]" * 70 + "}"
        for payload in (
            '{"duplicate":1,"duplicate":2}',
            '{"constant":NaN}',
            '{"overflow":1e10000}',
            deep,
        ):
            with (
                self.subTest(payload=payload[:24]),
                mock.patch.object(
                    ci.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0, payload, ""),
                ),
                self.assertRaisesRegex(ci.CiError, "metadata is malformed"),
            ):
                ci.gh_json("repos/owner/project/actions/runs/1")

    def test_resolution_proof_is_recomputed(self) -> None:
        value = selected_resolution()
        value["artifact_id"] += 1
        with self.assertRaisesRegex(ci.CiError, "proof"):
            ci.validate_resolution(value)
        value = selected_resolution()
        value["runner_label"] = "different-runner"
        value["resolution_proof_digest"] = ci.evidence.digest_value(
            ci.ordinary_resolution_projection(value)
        )
        with self.assertRaisesRegex(ci.CiError, "runtime configuration"):
            ci.validate_resolution(value)


class ProtectedBootstrapTests(CiFixtureCase):
    def test_protected_resolver_uses_exact_ids_and_revalidates_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archives: dict[int, bytes] = {}
            artifacts: dict[int, dict] = {}
            coordinates: list[dict] = []
            for offset, run_id in enumerate((11, 12, 13)):
                artifact_id = 101 + offset
                payload = zip_tree(
                    candidate_stage(
                        root,
                        self.policy,
                        self.digest,
                        self.identity,
                        workflow_run=run_id,
                    )
                )
                api_digest = f"sha256:{ci.hashlib.sha256(payload).hexdigest()}"
                archives[artifact_id] = payload
                artifacts[artifact_id] = {
                    "id": artifact_id,
                    "name": ci.artifact_name(run_id, 1),
                    "size_in_bytes": len(payload),
                    "expired": False,
                    "digest": api_digest,
                    "workflow_run": {"id": run_id, "head_sha": COMMIT},
                }
                coordinates.append(
                    {
                        "workflow_run_id": run_id,
                        "workflow_run_attempt": 1,
                        "artifact_id": artifact_id,
                        "artifact_digest": api_digest,
                    }
                )
            manifest = ci.protected_manifest_from_sources(coordinates)
            jobs = json.loads((FIXTURES / "jobs-candidate.json").read_text())
            metadata_endpoints: list[str] = []
            archive_endpoints: list[str] = []

            def metadata_fetcher(endpoint: str) -> dict:
                if "STAX_PERF_PROTECTED_MANIFEST" in ci.os.environ:
                    raise AssertionError(
                        "protected manifest remained in child environment"
                    )
                metadata_endpoints.append(endpoint)
                if endpoint == "repos/owner/project/actions/runs/90/attempts/1":
                    return self.current_run_fixture()
                if endpoint == "repos/owner/project/actions/workflows/7":
                    return self.workflow_fixture()
                for run_id in (11, 12, 13):
                    if endpoint == (
                        f"repos/owner/project/actions/runs/{run_id}/attempts/1"
                    ):
                        run = json.loads((FIXTURES / "run-candidate.json").read_text())
                        run["id"] = run_id
                        return run
                    if endpoint == (
                        f"repos/owner/project/actions/runs/{run_id}/attempts/1/"
                        "jobs?per_page=100"
                    ):
                        result = copy.deepcopy(jobs)
                        for job in result["jobs"]:
                            job["run_id"] = run_id
                        return result
                for artifact_id, value in artifacts.items():
                    if endpoint == (
                        f"repos/owner/project/actions/artifacts/{artifact_id}"
                    ):
                        return copy.deepcopy(value)
                raise AssertionError(f"unexpected metadata endpoint: {endpoint}")

            def archive_fetcher(endpoint: str) -> bytes:
                if "STAX_PERF_PROTECTED_MANIFEST" in ci.os.environ:
                    raise AssertionError(
                        "protected manifest remained in child environment"
                    )
                archive_endpoints.append(endpoint)
                artifact_id = int(endpoint.split("/")[-2])
                return archives[artifact_id]

            args = argparse.Namespace(
                selector="",
                repository="owner/project",
                current_run_id="90",
                current_run_attempt="1",
                current_commit=COMMIT,
                selected_dir=str(root / "selected"),
                protected_environment=ci.PROTECTED_ENVIRONMENT,
                protected_manifest_env="STAX_PERF_PROTECTED_MANIFEST",
            )
            with mock.patch.dict(
                ci.os.environ, {"STAX_PERF_PROTECTED_MANIFEST": manifest}, clear=False
            ):
                resolution = ci.resolve_protected(
                    args,
                    metadata_fetcher=metadata_fetcher,
                    archive_fetcher=archive_fetcher,
                )
            ci.validate_resolution(resolution)
            baseline, provenance = ci.derive_protected_baseline(
                root / "selected",
                resolution,
                self.policy,
                self.digest,
                90,
                1,
                COMMIT,
            )
            ci.validate_provenance(
                provenance, baseline, resolution, self.policy, self.digest
            )
            expected_metadata = []
            expected_archives = []
            expected_metadata.extend(
                [
                    "repos/owner/project/actions/runs/90/attempts/1",
                    "repos/owner/project/actions/workflows/7",
                ]
            )
            for offset, run_id in enumerate((11, 12, 13)):
                artifact_id = 101 + offset
                expected_metadata.extend(
                    [
                        f"repos/owner/project/actions/runs/{run_id}/attempts/1",
                        f"repos/owner/project/actions/runs/{run_id}/attempts/1/jobs?per_page=100",
                        f"repos/owner/project/actions/artifacts/{artifact_id}",
                    ]
                )
                expected_archives.append(
                    f"repos/owner/project/actions/artifacts/{artifact_id}/zip"
                )
            self.assertEqual(metadata_endpoints, expected_metadata)
            self.assertEqual(archive_endpoints, expected_archives)

            args.protected_environment = "unapproved"
            with self.assertRaisesRegex(ci.CiError, "environment selection"):
                ci.resolve_protected(args)

            candidate_file = root / "selected" / "candidate-2" / ci.SUITE_NAME
            with self.assertRaises(PermissionError):
                candidate_file.write_bytes(candidate_file.read_bytes() + b"\n")
            candidate_file.chmod(0o600)
            with self.assertRaisesRegex(ci.CiError, "sealed read-only"):
                ci.derive_protected_baseline(
                    root / "selected",
                    resolution,
                    self.policy,
                    self.digest,
                    90,
                    1,
                    COMMIT,
                )
            candidate_file.write_bytes(candidate_file.read_bytes() + b"\n")
            candidate_file.chmod(0o400)
            with self.assertRaisesRegex(ci.CiError, "tree digest"):
                ci.derive_protected_baseline(
                    root / "selected",
                    resolution,
                    self.policy,
                    self.digest,
                    90,
                    1,
                    COMMIT,
                )
            ci.remove_evidence_tree(root / "selected")

    def test_manifest_exact_shape_order_and_algorithm(self) -> None:
        sources = source_rows()
        manifest = ci.protected_manifest_from_sources(sources)
        coordinates, digest = ci.parse_protected_manifest(manifest)
        self.assertEqual(len(coordinates), 3)
        self.assertEqual(digest, ci.hashlib.sha256(manifest.encode()).hexdigest())
        invalid = [
            "",
            manifest + ";14:1:104:sha256:" + "4" * 64,
            manifest.replace("sha256:", "unknown:", 1),
            ";".join(["candidates", *manifest.split(";")[1:][::-1]]),
            manifest.replace("11:1:101", "12:1:101"),
            manifest.replace("11:1:101", "9223372036854775808:1:101"),
        ]
        for value in invalid:
            with self.subTest(value=value[:24]), self.assertRaises(ci.CiError):
                ci.parse_protected_manifest(value)

    def test_candidate_job_step_proof_and_negatives(self) -> None:
        jobs = json.loads((FIXTURES / "jobs-candidate.json").read_text())
        source_run = {
            "workflow_run_id": 11,
            "workflow_run_attempt": 1,
            "source_commit": COMMIT,
        }
        digest = ci.validate_candidate_step_proof(jobs, source_run)
        self.assertRegex(digest, r"^[a-f0-9]{64}$")
        mutations = []
        wrong_label = copy.deepcopy(jobs)
        wrong_label["jobs"][0]["labels"] = ["other"]
        mutations.append(wrong_label)
        wrong_run = copy.deepcopy(jobs)
        wrong_run["jobs"][0]["run_id"] = 12
        mutations.append(wrong_run)
        wrong_commit = copy.deepcopy(jobs)
        wrong_commit["jobs"][0]["head_sha"] = "b" * 40
        mutations.append(wrong_commit)
        total_mismatch = copy.deepcopy(jobs)
        total_mismatch["total_count"] = 3
        mutations.append(total_mismatch)
        boolean_total = copy.deepcopy(jobs)
        boolean_total["total_count"] = True
        mutations.append(boolean_total)
        early = copy.deepcopy(jobs)
        early["jobs"][0]["steps"][4]["conclusion"] = "failure"
        mutations.append(early)
        late = copy.deepcopy(jobs)
        late["jobs"][0]["steps"].insert(
            -2,
            {"name": "Unexpected work", "status": "completed", "conclusion": "success"},
        )
        mutations.append(late)
        missing_cleanup = copy.deepcopy(jobs)
        missing_cleanup["jobs"][0]["steps"].pop()
        mutations.append(missing_cleanup)
        skipped_cleanup = copy.deepcopy(jobs)
        skipped_cleanup["jobs"][0]["steps"][-2]["conclusion"] = "skipped"
        mutations.append(skipped_cleanup)
        one_job = copy.deepcopy(jobs)
        one_job["total_count"] = 1
        one_job["jobs"].pop()
        mutations.append(one_job)
        failed_sibling = copy.deepcopy(jobs)
        failed_sibling["jobs"][1]["conclusion"] = "failure"
        mutations.append(failed_sibling)
        successful_sibling = copy.deepcopy(jobs)
        successful_sibling["jobs"][1]["conclusion"] = "success"
        mutations.append(successful_sibling)
        active_sibling = copy.deepcopy(jobs)
        active_sibling["jobs"][1]["status"] = "in_progress"
        active_sibling["jobs"][1]["conclusion"] = None
        mutations.append(active_sibling)
        unexpected_sibling = copy.deepcopy(jobs)
        unexpected_sibling["jobs"][1]["name"] = "unexpected"
        mutations.append(unexpected_sibling)
        executed_sibling = copy.deepcopy(jobs)
        executed_sibling["jobs"][1]["steps"] = [
            {"name": "Set up job", "status": "completed", "conclusion": "success"}
        ]
        mutations.append(executed_sibling)
        wrong_sibling_run = copy.deepcopy(jobs)
        wrong_sibling_run["jobs"][1]["run_id"] = 12
        mutations.append(wrong_sibling_run)
        duplicate_job_id = copy.deepcopy(jobs)
        duplicate_job_id["jobs"][1]["id"] = duplicate_job_id["jobs"][0]["id"]
        mutations.append(duplicate_job_id)
        cancelled = copy.deepcopy(jobs)
        cancelled["jobs"][0]["conclusion"] = "cancelled"
        mutations.append(cancelled)
        for value in mutations:
            with self.subTest(), self.assertRaises(ci.CiError):
                ci.validate_candidate_step_proof(value, source_run)

    def test_bootstrap_resolution_requires_cohort_invariants(self) -> None:
        value = protected_resolution()
        for field, replacement in (
            ("workflow_id", 99),
            ("source_commit", "b" * 40),
            ("artifact_id", value["protected_sources"][0]["artifact_id"]),
            ("artifact_digest", value["protected_sources"][0]["artifact_digest"]),
            (
                "artifact_tree_digest",
                value["protected_sources"][0]["artifact_tree_digest"],
            ),
        ):
            with self.subTest(field=field):
                changed = copy.deepcopy(value)
                changed["protected_sources"][1][field] = replacement
                changed["resolution_proof_digest"] = ci.evidence.digest_value(
                    {
                        "protected_manifest_digest": changed[
                            "protected_manifest_digest"
                        ],
                        "protected_sources": changed["protected_sources"],
                    }
                )
                with self.assertRaises(ci.CiError):
                    ci.validate_resolution(changed)

    def test_provenance_projection_and_creation_resolution_are_bound(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            stage = qualified_stage(
                Path(directory), self.policy, self.digest, self.identity
            )
            ci.validate_stage(stage, require_qualified=True)
            original = (stage / ci.PROVENANCE_NAME).read_bytes()
            mutations = {
                "creation_source": "other",
                "workflow_path": ".github/workflows/other.yml",
                "workflow_run_id": 999,
                "workflow_run_attempt": 2,
                "source_commit": "b" * 40,
                "policy_digest": "b" * 64,
                "scenario_identity_digest": "b" * 64,
                "evidence_schema_digest": "b" * 64,
                "runner_label": "other",
                "runner_key": "other",
                "baseline_digest": "b" * 64,
                "creation_manifest_digest": "b" * 64,
                "creation_resolution_proof_digest": "b" * 64,
                "aggregate_digest": "b" * 64,
                "source_suite_digests": lambda value: list(reversed(value)),
                "source_harness_run_ids": lambda value: list(reversed(value)),
                "source_workflow_run_ids": lambda value: list(reversed(value)),
                "source_workflow_run_attempts": lambda value: [2, *value[1:]],
            }
            for field, replacement in mutations.items():
                with self.subTest(field=field):
                    provenance = ci.evidence.load_json(stage / ci.PROVENANCE_NAME)
                    provenance[field] = (
                        replacement(provenance[field])
                        if callable(replacement)
                        else replacement
                    )
                    write_json(stage / ci.PROVENANCE_NAME, provenance)
                    with self.assertRaises((ci.CiError, ci.evidence.ValidationError)):
                        ci.validate_stage(stage)
                    (stage / ci.PROVENANCE_NAME).write_bytes(original)

            qualification_original = (stage / ci.QUALIFICATION_NAME).read_bytes()
            provenance = ci.evidence.load_json(stage / ci.PROVENANCE_NAME)
            baseline_resolution = ci.evidence.load_json(
                stage / ci.BASELINE_RESOLUTION_NAME
            )
            provenance["workflow_run_id"] = baseline_resolution["protected_sources"][0][
                "workflow_run_id"
            ]
            provenance["aggregate_digest"] = ci.evidence.digest_value(
                ci.provenance_projection(provenance)
            )
            write_json(stage / ci.PROVENANCE_NAME, provenance)
            qualification = ci.evidence.load_json(stage / ci.QUALIFICATION_NAME)
            qualification["baseline_provenance_digest"] = ci.evidence.digest_value(
                provenance
            )
            write_json(stage / ci.QUALIFICATION_NAME, qualification)
            with self.assertRaisesRegex(ci.CiError, "overlaps"):
                ci.validate_stage(stage)
            (stage / ci.PROVENANCE_NAME).write_bytes(original)
            (stage / ci.QUALIFICATION_NAME).write_bytes(qualification_original)


class ValidatorAndComparisonTests(CiFixtureCase):
    def test_standalone_validator_exact_invalid_vs_incompatible(self) -> None:
        incompatible = subprocess.CompletedProcess(
            [],
            3,
            '{"artifact":"baseline","outcome":"incompatible","schema":"staxtrace.performance-validation"}\n',
            "",
        )
        with mock.patch.object(ci.subprocess, "run", return_value=incompatible):
            self.assertEqual(
                ci.standalone_baseline_outcome(Path("baseline.json")), "incompatible"
            )
        invalid = subprocess.CompletedProcess(
            [],
            2,
            '{"artifact":"baseline","outcome":"invalid","schema":"staxtrace.performance-validation"}\n',
            "",
        )
        with mock.patch.object(ci.subprocess, "run", return_value=invalid):
            with self.assertRaisesRegex(ci.CiError, "hard failure"):
                ci.standalone_baseline_outcome(Path("baseline.json"))
        malformed = subprocess.CompletedProcess([], 3, "{}\n", "")
        with mock.patch.object(ci.subprocess, "run", return_value=malformed):
            with self.assertRaisesRegex(ci.CiError, "malformed"):
                ci.standalone_baseline_outcome(Path("baseline.json"))

    def test_each_regression_class_is_hard(self) -> None:
        baseline = baseline_fixture(self.policy, self.digest, self.identity)
        base_absolute = run_helpers.valid_suite(
            self.policy, self.digest, f"{500:032x}", self.identity
        )
        scenario_latency = next(iter(self.policy["scenarios"]))
        scenario_throughput = next(
            name
            for name, value in self.policy["scenarios"].items()
            if value["guard"]["metric"] == "throughput_per_second"
        )
        cases = [
            ("latency", scenario_latency),
            ("rss", scenario_latency),
            ("open_files", scenario_latency),
            ("throughput", scenario_throughput),
        ]
        for kind, scenario in cases:
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                current = copy.deepcopy(base_absolute)
                receipt = next(
                    row for row in current["scenarios"] if row["scenario"] == scenario
                )
                if kind == "latency":
                    receipt["samples"]["p99_ms"] = (
                        baseline["scenarios"][scenario]["p99_ms"] * 2
                    )
                    if receipt["guard"]["metric"] == "p99_ms":
                        receipt["guard"]["actual"] = receipt["samples"]["p99_ms"]
                elif kind == "rss":
                    receipt["resources"]["peak_rss_bytes"] = 2
                elif kind == "open_files":
                    receipt["resources"]["peak_open_files"] = 2
                else:
                    for suite in baseline["source_suites"]:
                        source = next(
                            row
                            for row in suite["scenarios"]
                            if row["scenario"] == scenario
                        )
                        source["throughput_per_second"] *= 2
                        source["guard"]["actual"] = source["throughput_per_second"]
                        suite["scenario_digests"][scenario] = ci.evidence.digest_value(
                            source
                        )
                    receipts = [
                        ci.evidence.validate_suite_receipt(
                            suite, self.policy, self.digest
                        )
                        for suite in baseline["source_suites"]
                    ]
                    baseline["suite_digests"] = [
                        ci.evidence.digest_value(suite)
                        for suite in baseline["source_suites"]
                    ]
                    baseline["scenarios"] = ci.evidence.derive_baseline_scenarios(
                        receipts, self.policy
                    )
                current["scenario_digests"][scenario] = ci.evidence.digest_value(
                    receipt
                )
                ci.evidence.validate_suite_receipt(current, self.policy, self.digest)
                baseline_path = Path(directory) / ci.BASELINE_NAME
                write_json(baseline_path, baseline)
                with self.assertRaises(ci.evidence.ComparisonFailure):
                    ci.exact_comparison_suite(
                        current, baseline, baseline_path, self.policy, self.digest
                    )


class ArchiveAndCommandTests(CiFixtureCase):
    def _hosted_runner_pair(self) -> tuple[dict, dict, dict[str, str]]:
        toolchain_match = re.fullmatch(
            r"rustc ([0-9]+)\.([0-9]+)\.([0-9]+)( .+)",
            self.identity["toolchain"],
        )
        self.assertIsNotNone(toolchain_match)
        assert toolchain_match is not None
        historical_toolchain = (
            f"rustc {toolchain_match.group(1)}.{toolchain_match.group(2)}."
            f"{int(toolchain_match.group(3)) + 1}{toolchain_match.group(4)}"
        )
        current = {
            "os": self.identity["os"],
            "architecture": self.identity["architecture"],
            "runner_class": "hosted",
            "toolchain": self.identity["toolchain"],
        }
        current["key"] = (
            f"{current['os']}-{current['architecture']}-"
            f"{current['runner_class']}-{current['toolchain']}"
        )
        historical = {**current, "toolchain": historical_toolchain}
        historical["key"] = (
            f"{historical['os']}-{historical['architecture']}-"
            f"{historical['runner_class']}-{historical['toolchain']}"
        )
        environment = {
            "GITHUB_ACTIONS": "true",
            "STAX_PERF_RUNNER_CLASS": "hosted",
        }
        return current, historical, environment

    def _sealed_protected_candidates(
        self, root: Path, identity: dict
    ) -> tuple[Path, dict]:
        selected = root / "protected-selected"
        selected.mkdir(mode=0o700)
        resolution = protected_resolution()
        with mock.patch.dict(ci.os.environ, {"GITHUB_ACTIONS": "false"}, clear=False):
            for index, source in enumerate(resolution["protected_sources"], start=1):
                staged = candidate_stage(
                    selected,
                    self.policy,
                    self.digest,
                    identity,
                    workflow_run=source["workflow_run_id"],
                )
                destination = selected / f"candidate-{index}"
                staged.rename(destination)
                source["artifact_tree_digest"] = ci.seal_evidence_tree(destination)
        selected.chmod(0o500)
        resolution["resolution_proof_digest"] = ci.evidence.digest_value(
            {
                "protected_manifest_digest": resolution["protected_manifest_digest"],
                "protected_sources": resolution["protected_sources"],
            }
        )
        ci.validate_resolution(resolution)
        return selected, resolution

    def test_archive_rejects_traversal_symlink_truncation_and_oversize(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cases: list[bytes] = []
            for name, data, mode in (
                ("../escape.json", b"{}", 0),
                (ci.QUALIFICATION_NAME, b"{}", 0o120777),
                (ci.QUALIFICATION_NAME, b"x" * (ci.MAX_FILE_BYTES + 1), 0),
            ):
                stream = io.BytesIO()
                with zipfile.ZipFile(stream, "w") as archive:
                    info = zipfile.ZipInfo(name)
                    info.external_attr = mode << 16
                    archive.writestr(info, data)
                cases.append(stream.getvalue())

            stream = io.BytesIO()
            payload_marker = b'{"slot":0}'
            with zipfile.ZipFile(
                stream, "w", compression=zipfile.ZIP_STORED
            ) as archive:
                for index, name in enumerate(sorted(ci.PENDING_FILES)):
                    archive.writestr(name, f'{{"slot":{index}}}'.encode("utf-8"))
            valid_archive = stream.getvalue()
            corrupt_crc = bytearray(valid_archive)
            marker_offset = corrupt_crc.find(payload_marker)
            self.assertGreaterEqual(marker_offset, 0)
            corrupt_crc[marker_offset] ^= 1
            cases.append(bytes(corrupt_crc))
            cases.append(valid_archive[:-8])
            cases.append(b"not-a-zip")
            for index, payload in enumerate(cases):
                with self.subTest(index=index), self.assertRaises(ci.CiError):
                    ci.extract_archive(payload, root / f"out-{index}")
                self.assertFalse((root / f"out-{index}").exists())

    def test_production_parser_has_no_resolver_fixture_option(self) -> None:
        root_parser = ci.parser()
        subparsers = next(
            action
            for action in root_parser._actions
            if isinstance(action, argparse._SubParsersAction)
        )
        self.assertNotIn("workflow-check", subparsers.choices)
        resolve_options = {
            option
            for action in subparsers.choices["resolve"]._actions
            for option in action.option_strings
        }
        self.assertNotIn("--fixture-mode", resolve_options)

    def test_output_channel_failure_removes_prepared_stage(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{701:032x}", self.identity
            )
            receipt = root / "suite.json"
            write_json(receipt, suite)
            snapshot = root / "snapshot.json"
            ci.snapshot_command(
                argparse.Namespace(receipt=str(receipt), output=str(snapshot))
            )
            resolution_path = root / "resolution.json"
            write_json(resolution_path, ci.resolution_template("no_selector", "none"))
            stage = root / "stage"
            with self.assertRaisesRegex(ci.CiError, "output channel"):
                ci.qualify_command(
                    argparse.Namespace(
                        receipt=str(receipt),
                        snapshot=str(snapshot),
                        resolution=str(resolution_path),
                        selected_dir=str(root / "unused"),
                        staging_dir=str(stage),
                        current_run_id="801",
                        current_run_attempt="1",
                        current_commit=COMMIT,
                        github_output=str(root / "missing-output"),
                    )
                )
            self.assertFalse(stage.exists())

    def test_selected_artifact_strict_json_failures_are_sanitized(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{730:032x}", self.identity
            )
            receipt, snapshot = self._snapshot_cli(root, suite, "strict-json")
            cases = {
                "duplicate": (b'{"schema":"duplicate","schema":"duplicate"}'),
                "deep": (
                    '{"untrusted_marker":' + "[" * 70 + "null" + "]" * 70 + "}"
                ).encode("utf-8"),
            }
            for index, (label, payload) in enumerate(cases.items()):
                with self.subTest(label=label):
                    case_root = root / label
                    case_root.mkdir()
                    selected = qualified_stage(
                        case_root,
                        self.policy,
                        self.digest,
                        self.identity,
                        workflow_run=50,
                    )
                    (selected / ci.QUALIFICATION_NAME).write_bytes(payload)
                    resolution_path = case_root / "resolution.json"
                    write_json(
                        resolution_path,
                        selected_resolution(50, 1, ci.seal_evidence_tree(selected)),
                    )
                    stage = case_root / "stage"
                    output = case_root / "github-output"
                    output.write_text("", encoding="utf-8")
                    completed = self._qualify_cli(
                        receipt=receipt,
                        snapshot=snapshot,
                        resolution=resolution_path,
                        selected=selected,
                        stage=stage,
                        output=output,
                        current_run=850 + index,
                    )
                    combined = completed.stdout + completed.stderr
                    self.assertEqual(completed.returncode, 2, combined)
                    self.assertFalse(stage.exists())
                    self.assertEqual(output.read_text(encoding="utf-8"), "")
                    self.assertNotIn("Traceback", combined)
                    self.assertNotIn(str(case_root), combined)
                    self.assertNotIn("untrusted_marker", combined)
                    ci.remove_evidence_tree(selected)

    def test_error_sanitizer_covers_absolute_path_forms_without_suffix_leaks(
        self,
    ) -> None:
        cases = {
            "failed /tmp/private folder/secret.txt: invalid": "failed <redacted>",
            r"failed C:\private folder\secret.txt: invalid": "failed <redacted>",
            r"failed \\server\share folder\secret.txt: invalid": "failed <redacted>",
            'failed "/tmp/private folder/secret.txt": invalid': (
                'failed "<redacted>": invalid'
            ),
            r"failed 'C:\private folder\secret.txt': invalid": (
                "failed '<redacted>': invalid"
            ),
            r"failed '\\server\share folder\secret.txt': invalid": (
                "failed '<redacted>': invalid"
            ),
        }
        for message, expected in cases.items():
            with self.subTest(message=message[:16]):
                self.assertEqual(
                    ci.sanitize_error_message(ci.CiError(message)), expected
                )

    def _snapshot_cli(
        self,
        root: Path,
        suite: dict,
        label: str,
        *,
        environment: dict[str, str] | None = None,
    ) -> tuple[Path, Path]:
        receipt = root / f"{label}-suite.json"
        snapshot = root / f"{label}-snapshot.json"
        write_json(receipt, suite)
        completed = ci_cli(
            "snapshot",
            "--receipt",
            str(receipt),
            "--output",
            str(snapshot),
            environment=environment,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return receipt, snapshot

    def _qualify_cli(
        self,
        *,
        receipt: Path,
        snapshot: Path,
        resolution: Path,
        selected: Path,
        stage: Path,
        output: Path,
        current_run: int,
        environment: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess:
        return ci_cli(
            "qualify",
            "--receipt",
            str(receipt),
            "--snapshot",
            str(snapshot),
            "--resolution",
            str(resolution),
            "--selected-dir",
            str(selected),
            "--staging-dir",
            str(stage),
            "--current-run-id",
            str(current_run),
            "--current-run-attempt",
            "1",
            "--current-commit",
            COMMIT,
            "--github-output",
            str(output),
            environment=environment,
        )

    def test_exact_candidate_and_same_key_cli_commands(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{700:032x}", self.identity
            )
            receipt, snapshot = self._snapshot_cli(root, suite, "candidate")
            resolution = ci.resolution_template("no_selector", "none")
            resolution_path = root / "resolution.json"
            write_json(resolution_path, resolution)
            candidate = root / "candidate"
            candidate_output = root / "candidate-output"
            candidate_output.write_text("", encoding="utf-8")
            completed = self._qualify_cli(
                receipt=receipt,
                snapshot=snapshot,
                resolution=resolution_path,
                selected=root / "unused",
                stage=candidate,
                output=candidate_output,
                current_run=800,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                candidate_output.read_text(encoding="utf-8"),
                "upload_ready=true\nqualification=baseline_candidate\n",
            )
            enforced = ci_cli("enforce", "--staging-dir", str(candidate))
            self.assertEqual(enforced.returncode, 4, enforced.stderr)

            selected = qualified_stage(
                root, self.policy, self.digest, self.identity, workflow_run=50
            )
            ordinary = selected_resolution(50, 1, ci.seal_evidence_tree(selected))
            ordinary_path = root / "ordinary-resolution.json"
            write_json(ordinary_path, ordinary)
            qualified = root / "ordinary-qualified"
            qualified_output = root / "qualified-output"
            qualified_output.write_text("", encoding="utf-8")
            completed = self._qualify_cli(
                receipt=receipt,
                snapshot=snapshot,
                resolution=ordinary_path,
                selected=selected,
                stage=qualified,
                output=qualified_output,
                current_run=801,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                qualified_output.read_text(encoding="utf-8"),
                "upload_ready=true\nqualification=qualified\n",
            )
            enforced = ci_cli("enforce", "--staging-dir", str(qualified))
            self.assertEqual(enforced.returncode, 0, enforced.stderr)
            result, _, _, _, creation_resolution = ci.validate_stage(
                qualified, require_qualified=True
            )
            self.assertEqual(result["baseline_source"], "prior_run")
            self.assertEqual(creation_resolution["state"], "protected_candidates")
            for name in (
                ci.BASELINE_NAME,
                ci.PROVENANCE_NAME,
                ci.BASELINE_RESOLUTION_NAME,
            ):
                with self.subTest(name=name):
                    self.assertEqual(
                        (qualified / name).read_bytes(), (selected / name).read_bytes()
                    )

            _, comparison, baseline, provenance, baseline_resolution = (
                ci.validate_stage(qualified, require_qualified=True)
            )
            self.assertIsNotNone(baseline)
            self.assertIsNotNone(provenance)
            self.assertIsNotNone(baseline_resolution)
            assert baseline is not None
            assert provenance is not None
            assert baseline_resolution is not None
            absolute = ci.absolute_suite_from_comparison(
                comparison, self.policy, self.digest
            )
            next_resolution = selected_resolution(
                801, 1, ci.seal_evidence_tree(qualified)
            )
            for overlapping_run in (801, provenance["workflow_run_id"]):
                with (
                    self.subTest(overlapping_run=overlapping_run),
                    self.assertRaisesRegex(ci.CiError, "overlaps"),
                ):
                    ci.qualification_value(
                        state="qualified",
                        reason="comparable",
                        source="prior_run",
                        selector=next_resolution["selector"],
                        manifest_digest=None,
                        resolution=next_resolution,
                        current_run=overlapping_run,
                        current_attempt=1,
                        current_commit=COMMIT,
                        absolute_suite=absolute,
                        suite=comparison,
                        policy=self.policy,
                        policy_digest=self.digest,
                        baseline=baseline,
                        provenance=provenance,
                        baseline_resolution=baseline_resolution,
                    )
            ci.remove_evidence_tree(qualified)
            ci.remove_evidence_tree(selected)

    def test_incompatible_historical_runner_cli_becomes_pending(self) -> None:
        current_identity, old_identity, hosted_environment = self._hosted_runner_pair()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.dict(ci.os.environ, {"GITHUB_ACTIONS": "false"}):
                selected = qualified_stage(
                    root, self.policy, self.digest, old_identity, workflow_run=50
                )
            suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{710:032x}", current_identity
            )
            receipt, snapshot = self._snapshot_cli(
                root, suite, "incompatible", environment=hosted_environment
            )
            resolution = root / "incompatible-resolution.json"
            write_json(
                resolution,
                selected_resolution(50, 1, ci.seal_evidence_tree(selected)),
            )
            stage = root / "incompatible-stage"
            output = root / "incompatible-output"
            output.write_text("", encoding="utf-8")
            completed = self._qualify_cli(
                receipt=receipt,
                snapshot=snapshot,
                resolution=resolution,
                selected=selected,
                stage=stage,
                output=output,
                current_run=802,
                environment=hosted_environment,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            qualification = ci.evidence.load_json(stage / ci.QUALIFICATION_NAME)
            self.assertEqual(
                (
                    qualification["qualification"],
                    qualification["reason"],
                    qualification["baseline_source"],
                ),
                (
                    "pending_comparable_baseline",
                    "incompatible_runner",
                    "prior_run",
                ),
            )
            self.assertEqual(
                output.read_text(encoding="utf-8"),
                "upload_ready=true\nqualification=pending_comparable_baseline\n",
            )
            enforced = ci_cli(
                "enforce",
                "--staging-dir",
                str(stage),
                environment=hosted_environment,
            )
            self.assertEqual(enforced.returncode, 4, enforced.stderr)
            ci.remove_evidence_tree(selected)

    def test_protected_incompatible_cohort_uses_only_candidate_deferral(self) -> None:
        _, historical_identity, hosted_environment = self._hosted_runner_pair()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            selected, resolution = self._sealed_protected_candidates(
                root, historical_identity
            )
            source = resolution["protected_sources"][0]
            with mock.patch.dict(ci.os.environ, hosted_environment, clear=False):
                with self.assertRaisesRegex(ci.CiError, "active workflow runner"):
                    ci.validate_stage(
                        selected / "candidate-1",
                        expected_run=source,
                        expected_tree_digest=source["artifact_tree_digest"],
                    )
                with self.assertRaisesRegex(ci.CiError, "sealed selected baseline"):
                    ci.validate_stage(
                        selected / "candidate-1",
                        expected_run=source,
                        expected_tree_digest=source["artifact_tree_digest"],
                        defer_selected_qualified_runner=True,
                    )
                baseline, provenance = ci.derive_protected_baseline(
                    selected,
                    resolution,
                    self.policy,
                    self.digest,
                    860,
                    1,
                    COMMIT,
                )
                self.assertEqual(baseline["runner"], historical_identity)
                ci.validate_provenance(
                    provenance,
                    baseline,
                    resolution,
                    self.policy,
                    self.digest,
                )
            ci.remove_evidence_tree(selected)

    def test_protected_incompatible_cohort_cli_becomes_pending(self) -> None:
        current_identity, historical_identity, hosted_environment = (
            self._hosted_runner_pair()
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            selected, resolution = self._sealed_protected_candidates(
                root, historical_identity
            )
            current_suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{740:032x}", current_identity
            )
            receipt, snapshot = self._snapshot_cli(
                root,
                current_suite,
                "protected-incompatible",
                environment=hosted_environment,
            )
            resolution_path = root / "protected-resolution.json"
            write_json(resolution_path, resolution)
            stage = root / "protected-pending"
            output = root / "protected-output"
            output.write_text("", encoding="utf-8")
            completed = self._qualify_cli(
                receipt=receipt,
                snapshot=snapshot,
                resolution=resolution_path,
                selected=selected,
                stage=stage,
                output=output,
                current_run=861,
                environment=hosted_environment,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            qualification = ci.evidence.load_json(stage / ci.QUALIFICATION_NAME)
            self.assertEqual(
                (
                    qualification["qualification"],
                    qualification["reason"],
                    qualification["baseline_source"],
                ),
                (
                    "pending_comparable_baseline",
                    "incompatible_runner",
                    "protected_manifest",
                ),
            )
            self.assertEqual(
                set(path.name for path in stage.iterdir()), ci.PENDING_FILES
            )
            self.assertEqual(
                output.read_text(encoding="utf-8"),
                "upload_ready=true\nqualification=pending_comparable_baseline\n",
            )
            enforced = ci_cli(
                "enforce",
                "--staging-dir",
                str(stage),
                environment=hosted_environment,
            )
            self.assertEqual(enforced.returncode, 4, enforced.stderr)
            ci.remove_evidence_tree(selected)

    def test_invalid_and_regression_cli_fail_without_stage_or_readiness(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{720:032x}", self.identity
            )
            receipt, snapshot = self._snapshot_cli(root, suite, "invalid")
            resolution = root / "selected-resolution.json"
            invalid_selected = candidate_stage(
                root, self.policy, self.digest, self.identity, workflow_run=50
            )
            write_json(
                resolution,
                selected_resolution(50, 1, ci.seal_evidence_tree(invalid_selected)),
            )
            invalid_stage = root / "invalid-stage"
            invalid_output = root / "invalid-output"
            invalid_output.write_text("", encoding="utf-8")
            invalid = self._qualify_cli(
                receipt=receipt,
                snapshot=snapshot,
                resolution=resolution,
                selected=invalid_selected,
                stage=invalid_stage,
                output=invalid_output,
                current_run=803,
            )
            self.assertEqual(invalid.returncode, 2)
            self.assertFalse(invalid_stage.exists())
            self.assertEqual(invalid_output.read_text(encoding="utf-8"), "")
            ci.remove_evidence_tree(invalid_selected)

            selected = qualified_stage(
                root, self.policy, self.digest, self.identity, workflow_run=50
            )
            write_json(
                resolution,
                selected_resolution(50, 1, ci.seal_evidence_tree(selected)),
            )
            baseline = ci.evidence.load_json(selected / ci.BASELINE_NAME)
            regression_suite = run_helpers.valid_suite(
                self.policy, self.digest, f"{721:032x}", self.identity
            )
            scenario = next(iter(self.policy["scenarios"]))
            scenario_receipt = next(
                row
                for row in regression_suite["scenarios"]
                if row["scenario"] == scenario
            )
            scenario_receipt["resources"]["peak_open_files"] = (
                int(baseline["scenarios"][scenario]["peak_open_files"]) * 2
            )
            regression_suite["scenario_digests"][scenario] = ci.evidence.digest_value(
                scenario_receipt
            )
            ci.evidence.validate_suite_receipt(
                regression_suite, self.policy, self.digest
            )
            regression_receipt, regression_snapshot = self._snapshot_cli(
                root, regression_suite, "regression"
            )
            regression_stage = root / "regression-stage"
            regression_output = root / "regression-output"
            regression_output.write_text("", encoding="utf-8")
            regression = self._qualify_cli(
                receipt=regression_receipt,
                snapshot=regression_snapshot,
                resolution=resolution,
                selected=selected,
                stage=regression_stage,
                output=regression_output,
                current_run=804,
            )
            self.assertEqual(regression.returncode, 2)
            self.assertFalse(regression_stage.exists())
            self.assertEqual(regression_output.read_text(encoding="utf-8"), "")
            ci.remove_evidence_tree(selected)

    def test_changed_sources_are_not_mutated_by_validation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            stage = qualified_stage(
                Path(directory), self.policy, self.digest, self.identity
            )
            before = ci.tree_snapshot(stage)
            ci.validate_stage(stage, require_qualified=True)
            self.assertEqual(ci.tree_snapshot(stage), before)


if __name__ == "__main__":
    unittest.main()
