#!/usr/bin/env python3
"""Fail-closed hosted performance qualification and artifact staging."""

from __future__ import annotations

import argparse
import contextlib
import copy
import hashlib
import io
import json
import math
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from typing import Any, Callable
import zipfile

PERF = Path(__file__).resolve().parent
RUST_ROOT = PERF.parent
sys.path.insert(0, str(PERF))
import run as evidence  # noqa: E402

WORKFLOW_PATH = ".github/workflows/perf.yml"
RUNNER_LABEL_ENV = "STAX_PERF_RUNNER_LABEL"
ORDINARY_JOB_NAME = "performance qualification"
PROTECTED_JOB_NAME = "protected performance qualification"
PROTECTED_ENVIRONMENT = "performance-baseline"
ARTIFACT_PREFIX = "staxtrace-performance-evidence"
QUALIFICATION_NAME = "qualification.json"
SUITE_NAME = "suite-receipt.json"
BASELINE_NAME = "baseline.json"
PROVENANCE_NAME = "baseline-provenance.json"
RESOLUTION_NAME = "resolution.json"
BASELINE_RESOLUTION_NAME = "baseline-resolution.json"
QUALIFICATION_FILES = {
    QUALIFICATION_NAME,
    SUITE_NAME,
    BASELINE_NAME,
    PROVENANCE_NAME,
    RESOLUTION_NAME,
    BASELINE_RESOLUTION_NAME,
}
PENDING_FILES = {QUALIFICATION_NAME, SUITE_NAME, RESOLUTION_NAME}
MAX_FILE_BYTES = 1_048_576
MAX_STAGE_BYTES = 2_097_152
MAX_ARCHIVE_BYTES = 2_097_152
MAX_METADATA_BYTES = 2_097_152
GH_TIMEOUT_SECONDS = 30
VALID_COMMIT = re.compile(r"[a-f0-9]{40}")
VALID_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
VALID_WORKFLOW_REF = re.compile(r"[A-Za-z0-9][A-Za-z0-9._/-]{0,254}")
VALID_RUNNER_LABEL = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
SELECTOR_PATTERN = re.compile(r"([1-9][0-9]{0,19}):([1-9][0-9]{0,9})")
MANIFEST_COORDINATE = re.compile(
    r"([1-9][0-9]{0,19}):([1-9][0-9]{0,9}):"
    r"([1-9][0-9]{0,19}):(sha256:[a-f0-9]{64})"
)
API_DIGEST = re.compile(r"sha256:([a-f0-9]{64})")
QUOTED_ABSOLUTE_PATH = re.compile(
    r"(?P<quote>['\"])(?:/|[A-Za-z]:[\\/]|\\\\)[^'\"]*(?P=quote)"
)
UNQUOTED_ABSOLUTE_PATH_START = re.compile(
    r"(?<![A-Za-z0-9._-])(?:/|[A-Za-z]:[\\/]|\\\\)"
)

CI_SCHEMA_PATHS = {
    "qualification": PERF / "ci-qualification.schema.json",
    "provenance": PERF / "ci-provenance.schema.json",
    "resolution": PERF / "ci-resolution.schema.json",
    "snapshot": PERF / "ci-snapshot.schema.json",
}
# Canonical JSON digests, deliberately independent of file formatting.
EXPECTED_CI_SCHEMA_DIGESTS = {
    "qualification": "fba7744b24c0a127b8d874bdfba347e0c4d67e3fd283800fd94157d6270b9483",
    "provenance": "babdd0360446685c38ac88f906200332e487f839eca8cd991aee1291c9f45259",
    "resolution": "0cd658e2c76def84920fe8d2d19e5f46d8bb46883b16690e265f17db3104a40c",
    "snapshot": "9b9dc1720364e2b53217f4ebe828cdeb6ee82b53cb0bf4ecd2f073cbdd458820",
}


class CiError(ValueError):
    """A sanitized hosted qualification error."""


def sanitize_error_message(error: BaseException) -> str:
    message = QUOTED_ABSOLUTE_PATH.sub(
        lambda match: f"{match.group('quote')}<redacted>{match.group('quote')}",
        str(error),
    )
    unquoted = UNQUOTED_ABSOLUTE_PATH_START.search(message)
    if unquoted is not None:
        return f"{message[: unquoted.start()]}<redacted>"
    return message


def configured_runner_label() -> str:
    value = os.environ.get(RUNNER_LABEL_ENV)
    if not isinstance(value, str) or VALID_RUNNER_LABEL.fullmatch(value) is None:
        raise CiError("fixed workflow runner label configuration is invalid")
    return value


def _closed_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate object key")
        result[key] = value
    return result


def _reject_json_constant(_: str) -> None:
    raise ValueError("non-standard JSON constant")


def _finite_json_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed):
        raise ValueError("non-finite JSON number")
    return parsed


def _validate_json_depth(value: Any) -> None:
    stack = [(value, 0)]
    nodes = 0
    while stack:
        item, depth = stack.pop()
        nodes += 1
        if depth > 64 or nodes > 100_000:
            raise ValueError("JSON structure is too deep")
        if isinstance(item, dict):
            stack.extend((child, depth + 1) for child in item.values())
        elif isinstance(item, list):
            stack.extend((child, depth + 1) for child in item)


def strict_json_object(payload: bytes | str, label: str) -> dict[str, Any]:
    try:
        if isinstance(payload, bytes):
            if len(payload) > MAX_METADATA_BYTES:
                raise ValueError("JSON input is oversized")
            text = payload.decode("utf-8")
        elif isinstance(payload, str):
            if len(payload.encode("utf-8")) > MAX_METADATA_BYTES:
                raise ValueError("JSON input is oversized")
            text = payload
        else:
            raise ValueError("JSON input has a wrong type")
        value = json.loads(
            text,
            object_pairs_hook=_closed_object,
            parse_constant=_reject_json_constant,
            parse_float=_finite_json_float,
        )
        _validate_json_depth(value)
    except (UnicodeError, json.JSONDecodeError, ValueError, RecursionError) as error:
        raise CiError(f"{label} is malformed") from error
    if not isinstance(value, dict):
        raise CiError(f"{label} is malformed")
    return value


def load_artifact_json(path: Path, label: str) -> dict[str, Any]:
    try:
        if path.is_symlink() or not path.is_file():
            raise OSError("non-regular artifact file")
        size = path.stat().st_size
        if size < 1 or size > MAX_FILE_BYTES:
            raise OSError("artifact file size is invalid")
        payload = path.read_bytes()
    except OSError as error:
        raise CiError(f"{label} is unreadable") from error
    return strict_json_object(payload, label)


def load_ci_schema(name: str) -> dict[str, Any]:
    try:
        path = CI_SCHEMA_PATHS[name]
    except KeyError as error:
        raise CiError("unknown CI evidence schema") from error
    return evidence.load_json(path)


def verify_ci_schema_pins() -> None:
    if set(EXPECTED_CI_SCHEMA_DIGESTS) != set(CI_SCHEMA_PATHS):
        raise CiError("CI schema pin set is incomplete")
    for name, path in CI_SCHEMA_PATHS.items():
        if (
            evidence.digest_value(evidence.load_json(path))
            != EXPECTED_CI_SCHEMA_DIGESTS[name]
        ):
            raise CiError(f"CI {name} schema failed its independent digest pin")


def validate_ci_value(value: dict[str, Any], schema_name: str, label: str) -> None:
    evidence.validate_schema(value, load_ci_schema(schema_name), label)

    def privacy_projection(item: Any, key: str | None = None) -> Any:
        if key == "workflow_path" and item == WORKFLOW_PATH:
            return "checked_workflow_path"
        if isinstance(item, dict):
            return {
                child_key: privacy_projection(child, child_key)
                for child_key, child in item.items()
            }
        if isinstance(item, list):
            return [privacy_projection(child) for child in item]
        return item

    evidence.validate_privacy(privacy_projection(value), label)


def scenario_identity_digest(policy: dict[str, Any]) -> str:
    return evidence.digest_value(policy["scenarios"])


def evidence_schema_digest() -> str:
    return evidence.digest_value(evidence.EXPECTED_SCHEMA_DIGESTS)


def artifact_name(run_id: int, attempt: int) -> str:
    if (
        run_id < 1
        or run_id > 9_223_372_036_854_775_807
        or attempt < 1
        or attempt > 2_147_483_647
    ):
        raise CiError("artifact identity is invalid")
    return f"{ARTIFACT_PREFIX}-{run_id}-{attempt}"


def positive_integer(value: Any, label: str) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 1
        or value > 9_223_372_036_854_775_807
    ):
        raise CiError(f"{label} is invalid")
    return value


def commit_identity(value: Any, label: str) -> str:
    if not isinstance(value, str) or VALID_COMMIT.fullmatch(value) is None:
        raise CiError(f"{label} is invalid")
    return value


def parse_positive_argument(value: str, label: str, maximum: int) -> int:
    if re.fullmatch(r"[1-9][0-9]{0,19}", value) is None:
        raise CiError(f"{label} is invalid")
    parsed = int(value)
    if parsed > maximum:
        raise CiError(f"{label} is out of range")
    return parsed


def parse_selector(value: str) -> tuple[int, int]:
    match = SELECTOR_PATTERN.fullmatch(value)
    if match is None:
        raise CiError("baseline selector must use canonical RUN:ATTEMPT syntax")
    run_id, attempt = int(match.group(1)), int(match.group(2))
    if run_id > 9_223_372_036_854_775_807 or attempt > 2_147_483_647:
        raise CiError("baseline selector identity is out of range")
    return run_id, attempt


def parse_api_digest(value: Any) -> tuple[str, str]:
    if not isinstance(value, str):
        raise CiError("artifact digest is missing")
    match = API_DIGEST.fullmatch(value)
    if match is None:
        raise CiError("artifact digest algorithm or encoding is invalid")
    return value, match.group(1)


def parse_protected_manifest(value: str) -> tuple[list[dict[str, Any]], str]:
    if not isinstance(value, str) or not value:
        raise CiError("protected baseline manifest is absent")
    if len(value.encode("utf-8")) > 512 or value.strip() != value:
        raise CiError("protected baseline manifest is not canonical")
    pieces = value.split(";")
    if len(pieces) != 4 or pieces[0] != "candidates":
        raise CiError("protected baseline manifest must name exactly three candidates")
    coordinates: list[dict[str, Any]] = []
    canonical_rows: list[tuple[int, int, int, str]] = []
    for piece in pieces[1:]:
        match = MANIFEST_COORDINATE.fullmatch(piece)
        if match is None:
            raise CiError("protected baseline manifest coordinate is invalid")
        row = (
            int(match.group(1)),
            int(match.group(2)),
            int(match.group(3)),
            match.group(4),
        )
        if (
            row[0] > 9_223_372_036_854_775_807
            or row[1] > 2_147_483_647
            or row[2] > 9_223_372_036_854_775_807
        ):
            raise CiError("protected baseline manifest identity is out of range")
        canonical_rows.append(row)
        coordinates.append(
            {
                "workflow_run_id": row[0],
                "workflow_run_attempt": row[1],
                "artifact_id": row[2],
                "artifact_digest": row[3],
            }
        )
    if canonical_rows != sorted(canonical_rows):
        raise CiError("protected baseline manifest coordinates are not canonical")
    if len({row[0] for row in canonical_rows}) != 3:
        raise CiError("protected baseline manifest run identities are not distinct")
    if len({row[2] for row in canonical_rows}) != 3:
        raise CiError(
            "protected baseline manifest artifact identities are not distinct"
        )
    if len({row[3] for row in canonical_rows}) != 3:
        raise CiError("protected baseline manifest artifact digests are not distinct")
    return coordinates, hashlib.sha256(value.encode("utf-8")).hexdigest()


def context_identity(args: argparse.Namespace) -> tuple[int, int, str]:
    run_id = parse_positive_argument(
        args.current_run_id, "current run identity", 9_223_372_036_854_775_807
    )
    attempt = parse_positive_argument(
        args.current_run_attempt, "current run attempt", 2_147_483_647
    )
    commit = commit_identity(args.current_commit, "current source commit")
    return run_id, attempt, commit


def validate_repository(value: str) -> str:
    if VALID_REPOSITORY.fullmatch(value) is None or ".." in value:
        raise CiError("repository identity is invalid")
    return value


def normalized_workflow_path(value: Any, label: str) -> str:
    if value == WORKFLOW_PATH:
        return WORKFLOW_PATH
    if not isinstance(value, str) or not value.startswith(f"{WORKFLOW_PATH}@"):
        raise CiError(f"{label} mismatch")
    reference = value[len(WORKFLOW_PATH) + 1 :]
    if (
        VALID_WORKFLOW_REF.fullmatch(reference) is None
        or ".." in reference
        or "//" in reference
        or reference.endswith(("/", "."))
    ):
        raise CiError(f"{label} suffix is invalid")
    return WORKFLOW_PATH


def runner_is_hosted() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


def pinned_toolchain_channel() -> str:
    path = RUST_ROOT / "rust-toolchain.toml"
    text = path.read_text(encoding="utf-8")
    matches = re.findall(
        r'^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$', text, re.MULTILINE
    )
    if len(matches) != 1:
        raise CiError("pinned Rust toolchain identity is invalid")
    return matches[0]


def validate_workflow_runner(runner: dict[str, Any], policy: dict[str, Any]) -> None:
    if not runner_is_hosted():
        return
    expected = evidence.runner_identity(policy)
    if expected["runner_class"] != "hosted" or runner != expected:
        raise CiError("hosted runner identity differs from the active workflow runner")


def fresh_output_path(path: Path, label: str) -> None:
    if path.exists() or path.is_symlink():
        raise CiError(f"{label} must be freshly created")
    parent = path.parent
    if not parent.is_dir() or parent.is_symlink():
        raise CiError(f"{label} parent is not a trusted directory")


def write_new_json(path: Path, value: dict[str, Any]) -> None:
    fresh_output_path(path, path.name)
    evidence.atomic_write_json(path, value)
    path.chmod(0o600)


def tree_snapshot(root: Path) -> dict[str, tuple[str, int, int]]:
    if not root.is_dir() or root.is_symlink():
        raise CiError("evidence directory is not a regular directory")
    snapshot: dict[str, tuple[str, int, int]] = {}
    total = 0
    for entry in sorted(root.iterdir(), key=lambda item: item.name):
        if entry.is_symlink() or not entry.is_file():
            raise CiError("evidence tree contains a non-regular entry")
        details = entry.stat()
        if details.st_size < 1 or details.st_size > MAX_FILE_BYTES:
            raise CiError("evidence tree file size is invalid")
        total += details.st_size
        if total > MAX_STAGE_BYTES:
            raise CiError("evidence tree is oversized")
        data = entry.read_bytes()
        if len(data) != details.st_size:
            raise CiError("evidence tree file changed while read")
        snapshot[entry.name] = (
            hashlib.sha256(data).hexdigest(),
            len(data),
            details.st_mtime_ns,
        )
    return snapshot


def validate_flat_tree(root: Path, expected: set[str]) -> None:
    snapshot = tree_snapshot(root)
    if set(snapshot) != expected:
        raise CiError("evidence staging allowlist changed")
    total = 0
    for name, (_, size, _) in snapshot.items():
        if name.startswith(".") or Path(name).name != name:
            raise CiError("evidence staging name is invalid")
        if size < 1 or size > MAX_FILE_BYTES:
            raise CiError("evidence staging file size is invalid")
        total += size
    if total > MAX_STAGE_BYTES:
        raise CiError("evidence staging tree is oversized")


def evidence_tree_digest(root: Path) -> str:
    snapshot = tree_snapshot(root)
    names = set(snapshot)
    if names not in {frozenset(PENDING_FILES), frozenset(QUALIFICATION_FILES)}:
        raise CiError("evidence tree allowlist is incomplete")
    validate_flat_tree(root, names)
    projection = [
        {"name": name, "size": snapshot[name][1], "sha256": snapshot[name][0]}
        for name in sorted(names)
    ]
    return evidence.digest_value(projection)


def validate_sealed_evidence_tree(root: Path) -> None:
    try:
        root_details = root.lstat()
    except OSError as error:
        raise CiError("downloaded evidence seal is unreadable") from error
    if (
        not stat.S_ISDIR(root_details.st_mode)
        or stat.S_IMODE(root_details.st_mode) != 0o500
    ):
        raise CiError("downloaded evidence directory is not sealed read-only")
    names: set[str] = set()
    try:
        entries = list(root.iterdir())
    except OSError as error:
        raise CiError("downloaded evidence seal is unreadable") from error
    for entry in entries:
        try:
            details = entry.lstat()
        except OSError as error:
            raise CiError("downloaded evidence seal is unreadable") from error
        if not stat.S_ISREG(details.st_mode) or stat.S_IMODE(details.st_mode) != 0o400:
            raise CiError("downloaded evidence file is not sealed read-only")
        names.add(entry.name)
    if names not in {frozenset(PENDING_FILES), frozenset(QUALIFICATION_FILES)}:
        raise CiError("downloaded evidence sealed allowlist is incomplete")


def seal_evidence_tree(root: Path) -> str:
    before = evidence_tree_digest(root)
    for path in root.iterdir():
        path.chmod(0o400)
    if evidence_tree_digest(root) != before:
        raise CiError("downloaded evidence changed while it was sealed")
    root.chmod(0o500)
    validate_sealed_evidence_tree(root)
    return before


def remove_evidence_tree(root: Path) -> None:
    try:
        if root.is_symlink():
            root.unlink(missing_ok=True)
            return
        if not root.exists():
            return
        for current, directories, files in os.walk(
            root, topdown=True, followlinks=False
        ):
            current_path = Path(current)
            current_path.chmod(0o700)
            for name in directories:
                child = current_path / name
                if child.is_symlink():
                    child.unlink(missing_ok=True)
                else:
                    child.chmod(0o700)
            for name in files:
                child = current_path / name
                if child.is_symlink():
                    child.unlink(missing_ok=True)
                else:
                    child.chmod(0o600)
        shutil.rmtree(root, ignore_errors=True)
    except OSError:
        shutil.rmtree(root, ignore_errors=True)


def gh_json(endpoint: str) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            [
                "gh",
                "api",
                "--method",
                "GET",
                "-H",
                "Accept: application/vnd.github+json",
                endpoint,
            ],
            cwd=RUST_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=GH_TIMEOUT_SECONDS,
        )
    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
        raise CiError("hosted artifact resolver transport failed") from error
    if completed.returncode != 0 or completed.stderr:
        raise CiError("hosted artifact resolver transport was uncertain")
    return strict_json_object(completed.stdout, "hosted artifact resolver metadata")


def gh_archive(endpoint: str) -> bytes:
    try:
        completed = subprocess.run(
            [
                "gh",
                "api",
                "--method",
                "GET",
                "-H",
                "Accept: application/vnd.github+json",
                endpoint,
            ],
            cwd=RUST_ROOT,
            check=False,
            capture_output=True,
            timeout=GH_TIMEOUT_SECONDS,
        )
    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
        raise CiError("hosted artifact download failed") from error
    if completed.returncode != 0 or completed.stderr:
        raise CiError("hosted artifact download was uncertain")
    if not completed.stdout or len(completed.stdout) > MAX_ARCHIVE_BYTES:
        raise CiError("hosted artifact archive size is invalid")
    return completed.stdout


def validate_run_metadata(
    value: dict[str, Any],
    *,
    run_id: int,
    attempt: int,
    current_run_id: int,
    conclusion: str,
) -> dict[str, Any]:
    if run_id == current_run_id:
        raise CiError("baseline selector must name a distinct prior run")
    if positive_integer(value.get("id"), "source run identity") != run_id:
        raise CiError("source run identity mismatch")
    if positive_integer(value.get("run_attempt"), "source run attempt") != attempt:
        raise CiError("source run attempt mismatch")
    workflow_id = positive_integer(value.get("workflow_id"), "source workflow identity")
    normalized_workflow_path(value.get("path"), "source workflow path")
    if value.get("status") != "completed" or value.get("conclusion") != conclusion:
        raise CiError("source workflow conclusion is not eligible")
    commit = commit_identity(value.get("head_sha"), "source commit")
    return {
        "workflow_path": WORKFLOW_PATH,
        "runner_label": configured_runner_label(),
        "workflow_id": workflow_id,
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "source_commit": commit,
    }


def validate_current_run_metadata(
    value: dict[str, Any], *, run_id: int, attempt: int, source_commit: str
) -> dict[str, Any]:
    if positive_integer(value.get("id"), "current run identity") != run_id:
        raise CiError("current run identity mismatch")
    if positive_integer(value.get("run_attempt"), "current run attempt") != attempt:
        raise CiError("current run attempt mismatch")
    normalized_workflow_path(value.get("path"), "current workflow path")
    if commit_identity(value.get("head_sha"), "current source commit") != source_commit:
        raise CiError("current source commit mismatch")
    if value.get("status") != "in_progress" or value.get("conclusion") is not None:
        raise CiError("current workflow execution state is invalid")
    return {
        "workflow_path": WORKFLOW_PATH,
        "workflow_id": positive_integer(
            value.get("workflow_id"), "current workflow identity"
        ),
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "source_commit": source_commit,
    }


def validate_workflow_metadata(value: dict[str, Any], workflow_id: int) -> None:
    if positive_integer(value.get("id"), "workflow identity") != workflow_id:
        raise CiError("workflow identity anchor mismatch")
    if value.get("path") != WORKFLOW_PATH:
        raise CiError("workflow identity anchor path mismatch")
    if value.get("state") != "active":
        raise CiError("workflow identity anchor is not active")


def validate_candidate_step_proof(
    value: dict[str, Any], source_run: dict[str, Any]
) -> str:
    jobs = value.get("jobs")
    total = value.get("total_count")
    if (
        isinstance(total, bool)
        or not isinstance(total, int)
        or total != 2
        or not isinstance(jobs, list)
        or total != len(jobs)
        or any(not isinstance(job, dict) for job in jobs)
    ):
        raise CiError("candidate job proof is malformed")
    run_id = positive_integer(
        source_run.get("workflow_run_id"), "candidate source run identity"
    )
    attempt = positive_integer(
        source_run.get("workflow_run_attempt"), "candidate source run attempt"
    )
    source_commit = commit_identity(
        source_run.get("source_commit"), "candidate source commit"
    )
    matching = [job for job in jobs if job.get("name") == ORDINARY_JOB_NAME]
    if len(matching) != 1:
        raise CiError("candidate job proof is ambiguous")
    job = matching[0]
    job_id = positive_integer(job.get("id"), "candidate job identity")
    if positive_integer(job.get("run_id"), "candidate job run identity") != run_id:
        raise CiError("candidate job run identity mismatch")
    if (
        commit_identity(job.get("head_sha"), "candidate job source commit")
        != source_commit
    ):
        raise CiError("candidate job source commit mismatch")
    if job.get("status") != "completed" or job.get("conclusion") != "failure":
        raise CiError("candidate job did not end at qualification enforcement")
    runner_label = configured_runner_label()
    if job.get("labels") != [runner_label]:
        raise CiError("candidate job did not use the fixed workflow runner label")
    steps = job.get("steps")
    if not isinstance(steps, list) or not steps:
        raise CiError("candidate step proof is missing")
    if any(not isinstance(step, dict) for step in steps):
        raise CiError("candidate step proof is malformed")
    required_success = [
        "Checkout",
        "Install pinned Rust toolchain",
        "Validate performance workflow contract",
        "Run deterministic performance contract tests",
        "Run absolute performance suite",
        "Snapshot validated absolute suite",
        "Resolve selected baseline",
        "Prepare validated upload",
        "Upload validated performance evidence",
    ]
    expected_step_names = [
        "Set up job",
        *required_success,
        "Enforce comparative qualification",
        "Post Checkout",
        "Complete job",
    ]
    if [step.get("name") for step in steps] != expected_step_names:
        raise CiError("candidate qualification step sequence is not exact")
    enforce_index = 1 + len(required_success)
    for index, name in enumerate(expected_step_names[:enforce_index]):
        step = steps[index]
        if step.get("status") != "completed" or step.get("conclusion") != "success":
            raise CiError(f"candidate step did not succeed: {name}")
    enforce = steps[enforce_index]
    if enforce.get("status") != "completed" or enforce.get("conclusion") != "failure":
        raise CiError("candidate enforcement step did not fail deliberately")
    cleanup_steps = steps[enforce_index + 1 :]
    if any(
        step.get("status") != "completed" or step.get("conclusion") != "success"
        for step in cleanup_steps
    ):
        raise CiError("candidate run had a later hard failure")
    sibling = next(row for row in jobs if row is not job)
    sibling_id = positive_integer(sibling.get("id"), "skipped protected job identity")
    sibling_labels = sibling.get("labels", [])
    if (
        sibling_id == job_id
        or sibling.get("name") != PROTECTED_JOB_NAME
        or sibling.get("status") != "completed"
        or sibling.get("conclusion") != "skipped"
        or positive_integer(sibling.get("run_id"), "skipped protected job run identity")
        != run_id
        or commit_identity(
            sibling.get("head_sha"), "skipped protected job source commit"
        )
        != source_commit
        or sibling.get("steps") != []
        or sibling_labels not in ([], [runner_label])
    ):
        raise CiError("candidate run has an unexpected sibling job")
    sibling_projection = [
        {
            "job_id": sibling_id,
            "job_name": PROTECTED_JOB_NAME,
            "runner_labels": sibling_labels,
            "job_status": "completed",
            "job_conclusion": "skipped",
            "steps": [],
        }
    ]
    projection = {
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
        "source_commit": source_commit,
        "jobs": [
            {
                "job_id": job_id,
                "job_name": ORDINARY_JOB_NAME,
                "runner_labels": job["labels"],
                "job_status": job["status"],
                "job_conclusion": job["conclusion"],
                "steps": [
                    {
                        "name": step["name"],
                        "status": step.get("status"),
                        "conclusion": step.get("conclusion"),
                    }
                    for step in steps
                ],
            },
            *sibling_projection,
        ],
    }
    return evidence.digest_value(projection)


def artifact_metadata(
    value: dict[str, Any],
    *,
    expected_run: dict[str, Any],
    expected_id: int | None = None,
    expected_digest: str | None = None,
) -> dict[str, Any]:
    run_id = expected_run["workflow_run_id"]
    attempt = expected_run["workflow_run_attempt"]
    expected_name = artifact_name(run_id, attempt)
    identifier = positive_integer(value.get("id"), "artifact identity")
    if expected_id is not None and identifier != expected_id:
        raise CiError("artifact identity mismatch")
    if value.get("name") != expected_name:
        raise CiError("artifact name mismatch")
    size = positive_integer(value.get("size_in_bytes"), "artifact size")
    if size > MAX_ARCHIVE_BYTES:
        raise CiError("artifact is oversized")
    if not isinstance(value.get("expired"), bool):
        raise CiError("artifact expiry state is malformed")
    digest, _ = parse_api_digest(value.get("digest"))
    if expected_digest is not None and digest != expected_digest:
        raise CiError("artifact API digest mismatch")
    workflow_run = value.get("workflow_run")
    if not isinstance(workflow_run, dict):
        raise CiError("artifact workflow metadata is missing")
    if positive_integer(workflow_run.get("id"), "artifact source run") != run_id:
        raise CiError("artifact source run mismatch")
    if (
        commit_identity(workflow_run.get("head_sha"), "artifact source commit")
        != expected_run["source_commit"]
    ):
        raise CiError("artifact source commit mismatch")
    return {
        **expected_run,
        "artifact_id": identifier,
        "artifact_name": expected_name,
        "artifact_size": size,
        "artifact_digest": digest,
        "expired": value["expired"],
    }


def _extract_archive_unchecked(payload: bytes, destination: Path) -> None:
    fresh_output_path(destination, "artifact extraction directory")
    try:
        archive = zipfile.ZipFile(io.BytesIO(payload))
    except (OSError, zipfile.BadZipFile) as error:
        raise CiError("artifact archive is malformed") from error
    with archive:
        infos = archive.infolist()
        names: set[str] = set()
        total = 0
        for info in infos:
            name = info.filename
            mode = (info.external_attr >> 16) & 0xFFFF
            if (
                not name
                or name.startswith(".")
                or "/" in name
                or "\\" in name
                or Path(name).name != name
                or name not in QUALIFICATION_FILES
                or name in names
                or info.is_dir()
                or info.flag_bits & 0x1
                or stat.S_IFMT(mode) not in {0, stat.S_IFREG}
            ):
                raise CiError("artifact archive contains a forbidden entry")
            if info.file_size < 1 or info.file_size > MAX_FILE_BYTES:
                raise CiError("artifact archive entry size is invalid")
            total += info.file_size
            names.add(name)
        if names not in {frozenset(PENDING_FILES), frozenset(QUALIFICATION_FILES)}:
            raise CiError("artifact archive allowlist is incomplete")
        if total > MAX_STAGE_BYTES:
            raise CiError("artifact archive is oversized")
        destination.mkdir(mode=0o700)
        try:
            for info in infos:
                data = archive.read(info)
                if len(data) != info.file_size:
                    raise CiError("artifact archive entry length changed")
                target = destination / info.filename
                flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
                if hasattr(os, "O_NOFOLLOW"):
                    flags |= os.O_NOFOLLOW
                descriptor = os.open(target, flags, 0o600)
                with os.fdopen(descriptor, "wb") as handle:
                    handle.write(data)
        except Exception:
            remove_evidence_tree(destination)
            raise


def extract_archive(payload: bytes, destination: Path) -> None:
    try:
        _extract_archive_unchecked(payload, destination)
    except CiError:
        raise
    except Exception as error:
        remove_evidence_tree(destination)
        raise CiError("artifact archive extraction failed") from error


def download_and_extract(
    repository: str,
    source: dict[str, Any],
    destination: Path,
    archive_fetcher: Callable[[str], bytes],
) -> str:
    payload = archive_fetcher(
        f"repos/{repository}/actions/artifacts/{source['artifact_id']}/zip"
    )
    if len(payload) != source["artifact_size"]:
        raise CiError("downloaded artifact size differs from API metadata")
    _, expected_hex = parse_api_digest(source["artifact_digest"])
    if not hashlib.sha256(payload).hexdigest() == expected_hex:
        raise CiError("downloaded artifact digest differs from API metadata")
    try:
        extract_archive(payload, destination)
        return seal_evidence_tree(destination)
    except Exception:
        remove_evidence_tree(destination)
        raise


def resolution_template(state: str, source_kind: str) -> dict[str, Any]:
    return {
        "schema": "staxtrace.performance-ci-resolution",
        "state": state,
        "source_kind": source_kind,
        "workflow_path": WORKFLOW_PATH,
        "runner_label": configured_runner_label(),
        "selector": None,
        "workflow_id": None,
        "workflow_run_id": None,
        "workflow_run_attempt": None,
        "source_commit": None,
        "artifact_id": None,
        "artifact_name": None,
        "artifact_size": None,
        "artifact_digest": None,
        "artifact_tree_digest": None,
        "protected_manifest_digest": None,
        "resolution_proof_digest": None,
        "protected_sources": [],
    }


def ordinary_resolution_projection(value: dict[str, Any]) -> dict[str, Any]:
    projection = {
        "state": value["state"],
        "selector": value["selector"],
        "workflow_path": value["workflow_path"],
        "runner_label": value["runner_label"],
        "workflow_id": value["workflow_id"],
        "workflow_run_id": value["workflow_run_id"],
        "workflow_run_attempt": value["workflow_run_attempt"],
        "source_commit": value["source_commit"],
        "artifact_name": artifact_name(
            value["workflow_run_id"], value["workflow_run_attempt"]
        ),
    }
    if value["state"] == "artifact_unavailable":
        projection["artifact_count"] = 0
    else:
        projection.update(
            {
                "artifact_id": value["artifact_id"],
                "artifact_size": value["artifact_size"],
                "artifact_digest": value["artifact_digest"],
                "expired": value["state"] == "artifact_expired",
            }
        )
        if value["state"] == "selected":
            projection["artifact_tree_digest"] = value["artifact_tree_digest"]
    return projection


def protected_manifest_from_sources(sources: list[dict[str, Any]]) -> str:
    coordinates = [
        f"{source['workflow_run_id']}:{source['workflow_run_attempt']}:"
        f"{source['artifact_id']}:{source['artifact_digest']}"
        for source in sources
    ]
    return ";".join(["candidates", *coordinates])


def validate_resolution(value: dict[str, Any]) -> None:
    validate_ci_value(value, "resolution", "CI resolution")
    runner_label = configured_runner_label()
    if value["runner_label"] != runner_label:
        raise CiError("CI resolution runner label differs from runtime configuration")
    state = value["state"]
    kind = value["source_kind"]
    ordinary_fields = [
        "workflow_id",
        "workflow_run_id",
        "workflow_run_attempt",
        "source_commit",
        "artifact_id",
        "artifact_name",
        "artifact_size",
        "artifact_digest",
        "artifact_tree_digest",
    ]
    if state == "no_selector":
        if (
            kind != "none"
            or value["selector"] is not None
            or any(value[field] is not None for field in ordinary_fields)
            or value["protected_sources"]
            or value["protected_manifest_digest"] is not None
            or value["resolution_proof_digest"] is not None
        ):
            raise CiError("no-selector resolution state is inconsistent")
    elif state in {"artifact_unavailable", "artifact_expired", "selected"}:
        if (
            kind != "prior_run"
            or value["selector"] is None
            or value["protected_sources"]
            or value["protected_manifest_digest"] is not None
            or value["resolution_proof_digest"] is None
        ):
            raise CiError("ordinary resolution state is inconsistent")
        run_fields = ordinary_fields[:4]
        hosted_fields = ordinary_fields[4:8]
        if state == "artifact_unavailable":
            required = run_fields
        elif state == "artifact_expired":
            required = [*run_fields, *hosted_fields]
        else:
            required = ordinary_fields
        optional = set(ordinary_fields) - set(required)
        if any(value[field] is None for field in required) or any(
            value[field] is not None for field in optional
        ):
            raise CiError("ordinary resolution metadata shape is inconsistent")
        if value["workflow_run_id"] is None or value["workflow_run_attempt"] is None:
            raise CiError("ordinary resolution run identity is incomplete")
        if (
            value["selector"]
            != f"{value['workflow_run_id']}:{value['workflow_run_attempt']}"
        ):
            raise CiError("ordinary resolution selector does not match its metadata")
        expected_name = artifact_name(
            value["workflow_run_id"], value["workflow_run_attempt"]
        )
        if state != "artifact_unavailable" and value["artifact_name"] != expected_name:
            raise CiError("ordinary resolution artifact name is not derived")
        if value["resolution_proof_digest"] != evidence.digest_value(
            ordinary_resolution_projection(value)
        ):
            raise CiError("ordinary resolution proof digest mismatch")
    elif state == "protected_candidates":
        if (
            kind != "protected_manifest"
            or value["selector"] is not None
            or any(value[field] is not None for field in ordinary_fields)
            or value["protected_manifest_digest"] is None
            or value["resolution_proof_digest"] is None
            or len(value["protected_sources"]) != 3
        ):
            raise CiError("protected resolution state is inconsistent")
        sources = value["protected_sources"]
        if any(source["runner_label"] != runner_label for source in sources):
            raise CiError(
                "protected source runner label differs from runtime configuration"
            )
        if len({source["workflow_id"] for source in sources}) != 1:
            raise CiError("protected source workflow identities differ")
        if len({source["source_commit"] for source in sources}) != 1:
            raise CiError("protected source commits differ")
        for source in sources:
            expected_name = artifact_name(
                source["workflow_run_id"], source["workflow_run_attempt"]
            )
            if source["artifact_name"] != expected_name:
                raise CiError("protected source artifact name is not derived")
        for field in (
            "workflow_run_id",
            "artifact_id",
            "artifact_digest",
            "artifact_tree_digest",
        ):
            if len({source[field] for source in sources}) != 3:
                raise CiError(f"protected source {field} values are not distinct")
        manifest = protected_manifest_from_sources(sources)
        if parse_protected_manifest(manifest)[0] != [
            {
                "workflow_run_id": source["workflow_run_id"],
                "workflow_run_attempt": source["workflow_run_attempt"],
                "artifact_id": source["artifact_id"],
                "artifact_digest": source["artifact_digest"],
            }
            for source in sources
        ]:
            raise CiError("protected sources are not in canonical manifest order")
        if (
            hashlib.sha256(manifest.encode("utf-8")).hexdigest()
            != value["protected_manifest_digest"]
        ):
            raise CiError("protected resolution manifest digest mismatch")
        if value["resolution_proof_digest"] != evidence.digest_value(
            {
                "protected_manifest_digest": value["protected_manifest_digest"],
                "protected_sources": sources,
            }
        ):
            raise CiError("protected resolution proof digest mismatch")
    else:
        raise CiError("unknown CI resolution state")


def write_resolution(path: Path, value: dict[str, Any]) -> None:
    validate_resolution(value)
    write_new_json(path, value)
    print(json.dumps({"state": value["state"]}, sort_keys=True, separators=(",", ":")))


def resolve_ordinary(
    args: argparse.Namespace,
    *,
    metadata_fetcher: Callable[[str], dict[str, Any]] = gh_json,
    archive_fetcher: Callable[[str], bytes] = gh_archive,
) -> dict[str, Any]:
    current_run, current_attempt, current_commit = context_identity(args)
    selector = args.selector
    if not selector:
        return resolution_template("no_selector", "none")
    run_id, attempt = parse_selector(selector)
    if run_id == current_run:
        raise CiError("baseline selector must name a distinct prior run")
    repository = validate_repository(args.repository)
    current = validate_current_run_metadata(
        metadata_fetcher(
            f"repos/{repository}/actions/runs/{current_run}/attempts/{current_attempt}"
        ),
        run_id=current_run,
        attempt=current_attempt,
        source_commit=current_commit,
    )
    validate_workflow_metadata(
        metadata_fetcher(
            f"repos/{repository}/actions/workflows/{current['workflow_id']}"
        ),
        current["workflow_id"],
    )
    run = validate_run_metadata(
        metadata_fetcher(
            f"repos/{repository}/actions/runs/{run_id}/attempts/{attempt}"
        ),
        run_id=run_id,
        attempt=attempt,
        current_run_id=current_run,
        conclusion="success",
    )
    if run["workflow_id"] != current["workflow_id"]:
        raise CiError("selected run belongs to a different workflow identity")
    response = metadata_fetcher(
        f"repos/{repository}/actions/runs/{run_id}/artifacts?"
        f"name={artifact_name(run_id, attempt)}&per_page=100"
    )
    artifacts = response.get("artifacts")
    total = response.get("total_count")
    if (
        isinstance(total, bool)
        or not isinstance(total, int)
        or total < 0
        or not isinstance(artifacts, list)
        or total != len(artifacts)
    ):
        raise CiError("artifact listing response is malformed")
    base = resolution_template("selected", "prior_run")
    base.update({"selector": selector, **run})
    if total == 0:
        base["state"] = "artifact_unavailable"
        base["resolution_proof_digest"] = evidence.digest_value(
            ordinary_resolution_projection(base)
        )
        return base
    if total != 1 or not isinstance(artifacts[0], dict):
        raise CiError("selected artifact is ambiguous")
    artifact = artifact_metadata(artifacts[0], expected_run=run)
    base.update(
        {
            key: artifact[key]
            for key in (
                "artifact_id",
                "artifact_name",
                "artifact_size",
                "artifact_digest",
            )
        }
    )
    if artifact["expired"]:
        base["state"] = "artifact_expired"
        base["resolution_proof_digest"] = evidence.digest_value(
            ordinary_resolution_projection(base)
        )
        return base
    base["artifact_tree_digest"] = download_and_extract(
        repository, artifact, Path(args.selected_dir), archive_fetcher
    )
    base["resolution_proof_digest"] = evidence.digest_value(
        ordinary_resolution_projection(base)
    )
    return base


def resolve_protected(
    args: argparse.Namespace,
    *,
    metadata_fetcher: Callable[[str], dict[str, Any]] = gh_json,
    archive_fetcher: Callable[[str], bytes] = gh_archive,
) -> dict[str, Any]:
    current_run, current_attempt, current_commit = context_identity(args)
    if args.selector:
        raise CiError(
            "ordinary selector and protected bootstrap are mutually exclusive"
        )
    if args.protected_environment != PROTECTED_ENVIRONMENT:
        raise CiError("protected bootstrap environment selection is invalid")
    if (
        not args.protected_manifest_env
        or args.protected_manifest_env != "STAX_PERF_PROTECTED_MANIFEST"
    ):
        raise CiError("protected bootstrap secret binding is invalid")
    manifest = os.environ.pop(args.protected_manifest_env, "")
    coordinates, manifest_digest = parse_protected_manifest(manifest)
    repository = validate_repository(args.repository)
    current = validate_current_run_metadata(
        metadata_fetcher(
            f"repos/{repository}/actions/runs/{current_run}/attempts/{current_attempt}"
        ),
        run_id=current_run,
        attempt=current_attempt,
        source_commit=current_commit,
    )
    validate_workflow_metadata(
        metadata_fetcher(
            f"repos/{repository}/actions/workflows/{current['workflow_id']}"
        ),
        current["workflow_id"],
    )
    selected_root = Path(args.selected_dir)
    fresh_output_path(selected_root, "protected artifact directory")
    selected_root.mkdir(mode=0o700)
    sources: list[dict[str, Any]] = []
    try:
        for index, coordinate in enumerate(coordinates, start=1):
            run_id = coordinate["workflow_run_id"]
            attempt = coordinate["workflow_run_attempt"]
            run = validate_run_metadata(
                metadata_fetcher(
                    f"repos/{repository}/actions/runs/{run_id}/attempts/{attempt}"
                ),
                run_id=run_id,
                attempt=attempt,
                current_run_id=current_run,
                conclusion="failure",
            )
            if run["workflow_id"] != current["workflow_id"]:
                raise CiError(
                    "protected candidate belongs to a different workflow identity"
                )
            jobs = metadata_fetcher(
                f"repos/{repository}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"
            )
            step_proof_digest = validate_candidate_step_proof(jobs, run)
            raw_artifact = metadata_fetcher(
                f"repos/{repository}/actions/artifacts/{coordinate['artifact_id']}"
            )
            source = artifact_metadata(
                raw_artifact,
                expected_run=run,
                expected_id=coordinate["artifact_id"],
                expected_digest=coordinate["artifact_digest"],
            )
            if source["expired"]:
                raise CiError("protected candidate artifact is expired")
            destination = selected_root / f"candidate-{index}"
            artifact_tree_digest = download_and_extract(
                repository, source, destination, archive_fetcher
            )
            sources.append(
                {
                    key: source[key]
                    for key in (
                        "workflow_path",
                        "runner_label",
                        "workflow_id",
                        "workflow_run_id",
                        "workflow_run_attempt",
                        "source_commit",
                        "artifact_id",
                        "artifact_name",
                        "artifact_size",
                        "artifact_digest",
                    )
                }
                | {
                    "artifact_tree_digest": artifact_tree_digest,
                    "step_proof_digest": step_proof_digest,
                }
            )
        if len({row["workflow_id"] for row in sources}) != 1:
            raise CiError("protected candidates differ in workflow identity")
        if (
            len({row["source_commit"] for row in sources}) != 1
            or sources[0]["source_commit"] != current_commit
        ):
            raise CiError("protected candidates differ from the cohort source commit")
        selected_root.chmod(0o500)
    except Exception:
        remove_evidence_tree(selected_root)
        raise
    result = resolution_template("protected_candidates", "protected_manifest")
    result["protected_manifest_digest"] = manifest_digest
    result["protected_sources"] = sources
    result["resolution_proof_digest"] = evidence.digest_value(
        {"protected_manifest_digest": manifest_digest, "protected_sources": sources}
    )
    return result


def resolve_command(args: argparse.Namespace) -> int:
    verify_ci_schema_pins()
    if args.bootstrap:
        result = resolve_protected(args)
    else:
        if args.protected_environment or args.protected_manifest_env:
            raise CiError("ordinary resolution cannot bind protected bootstrap inputs")
        result = resolve_ordinary(args)
    write_resolution(Path(args.resolution), result)
    return 0


def load_absolute_suite(path: Path) -> tuple[dict[str, Any], dict[str, Any], str]:
    policy, policy_digest = evidence.load_policy()
    suite = evidence.load_json(path)
    evidence.validate_suite_receipt(suite, policy, policy_digest)
    if suite["baseline_comparison"] != {
        "mode": "absolute_guards",
        "baseline_file": None,
        "comparisons": [],
    }:
        raise CiError("current performance suite is not absolute-guard evidence")
    validate_workflow_runner(suite["runner"], policy)
    return suite, policy, policy_digest


def suite_snapshot(
    suite: dict[str, Any], policy: dict[str, Any], policy_digest: str
) -> dict[str, Any]:
    result = {
        "schema": "staxtrace.performance-ci-suite-snapshot",
        "suite_digest": evidence.digest_value(suite),
        "run_id": suite["run_id"],
        "policy_digest": policy_digest,
        "scenario_identity_digest": scenario_identity_digest(policy),
        "evidence_schema_digest": evidence_schema_digest(),
        "runner_label": configured_runner_label(),
        "runner_key": suite["runner"]["key"],
    }
    validate_ci_value(result, "snapshot", "CI suite snapshot")
    return result


def snapshot_command(args: argparse.Namespace) -> int:
    verify_ci_schema_pins()
    suite, policy, policy_digest = load_absolute_suite(Path(args.receipt))
    result = suite_snapshot(suite, policy, policy_digest)
    write_new_json(Path(args.output), result)
    print(
        json.dumps(
            {"suite_digest": result["suite_digest"]},
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


def validate_snapshot(
    path: Path, suite: dict[str, Any], policy: dict[str, Any], policy_digest: str
) -> None:
    stored = load_artifact_json(path, "CI suite snapshot")
    validate_ci_value(stored, "snapshot", "CI suite snapshot")
    if evidence.canonical_bytes(stored) != evidence.canonical_bytes(
        suite_snapshot(suite, policy, policy_digest)
    ):
        raise CiError("absolute suite changed after its validated snapshot")


def provenance_projection(provenance: dict[str, Any]) -> dict[str, Any]:
    return {
        key: value for key, value in provenance.items() if key != "aggregate_digest"
    }


def validate_provenance(
    provenance: dict[str, Any],
    baseline: dict[str, Any],
    baseline_resolution: dict[str, Any],
    policy: dict[str, Any],
    policy_digest: str,
) -> None:
    validate_ci_value(provenance, "provenance", "CI baseline provenance")
    validate_resolution(baseline_resolution)
    if baseline_resolution["state"] != "protected_candidates":
        raise CiError("baseline creation resolution is not a protected cohort")
    expected_suite_digests = [
        evidence.digest_value(suite) for suite in baseline["source_suites"]
    ]
    expected_harness_run_ids = [suite["run_id"] for suite in baseline["source_suites"]]
    expected = {
        "policy_digest": policy_digest,
        "scenario_identity_digest": scenario_identity_digest(policy),
        "evidence_schema_digest": evidence_schema_digest(),
        "runner_label": configured_runner_label(),
        "runner_key": baseline["runner"]["key"],
        "baseline_digest": evidence.digest_value(baseline),
        "source_suite_digests": expected_suite_digests,
        "source_harness_run_ids": expected_harness_run_ids,
    }
    for field, value in expected.items():
        if evidence.canonical_bytes(provenance[field]) != evidence.canonical_bytes(
            value
        ):
            raise CiError(f"baseline provenance {field} mismatch")
    if (
        provenance["creation_manifest_digest"]
        != baseline_resolution["protected_manifest_digest"]
    ):
        raise CiError("baseline provenance manifest proof mismatch")
    if (
        provenance["creation_resolution_proof_digest"]
        != baseline_resolution["resolution_proof_digest"]
    ):
        raise CiError("baseline provenance resolution proof mismatch")
    protected_sources = baseline_resolution["protected_sources"]
    if provenance["source_workflow_run_ids"] != [
        source["workflow_run_id"] for source in protected_sources
    ]:
        raise CiError("baseline provenance workflow-run order mismatch")
    if provenance["workflow_run_id"] in provenance["source_workflow_run_ids"]:
        raise CiError("baseline creation run overlaps a protected candidate run")
    if provenance["source_workflow_run_attempts"] != [
        source["workflow_run_attempt"] for source in protected_sources
    ]:
        raise CiError("baseline provenance workflow-attempt order mismatch")
    if any(
        source["source_commit"] != provenance["source_commit"]
        for source in protected_sources
    ):
        raise CiError("baseline provenance cohort commit mismatch")
    if (
        len(set(provenance["source_suite_digests"])) != 3
        or len(set(provenance["source_harness_run_ids"])) != 3
        or len(set(provenance["source_workflow_run_ids"])) != 3
    ):
        raise CiError("baseline provenance sources are not distinct")
    if provenance["aggregate_digest"] != evidence.digest_value(
        provenance_projection(provenance)
    ):
        raise CiError("baseline provenance aggregate digest mismatch")


def rederive_baseline(
    baseline: dict[str, Any], policy: dict[str, Any], policy_digest: str
) -> None:
    evidence.validate_baseline(baseline, policy, policy_digest)
    with tempfile.TemporaryDirectory(prefix="stax-perf-baseline-") as directory:
        root = Path(directory)
        receipt_paths: list[str] = []
        for index, suite in enumerate(baseline["source_suites"], start=1):
            path = root / f"source-{index}.json"
            evidence.atomic_write_json(path, suite)
            receipt_paths.append(str(path))
        output = root / BASELINE_NAME
        arguments = argparse.Namespace(output=str(output), receipts=receipt_paths)
        with contextlib.redirect_stdout(io.StringIO()):
            evidence.create_baseline(arguments)
        derived = evidence.load_json(output)
        if evidence.canonical_bytes(derived) != evidence.canonical_bytes(baseline):
            raise CiError("baseline does not equal its fresh median derivation")


def absolute_suite_from_comparison(
    suite: dict[str, Any], policy: dict[str, Any], policy_digest: str
) -> dict[str, Any]:
    absolute = copy.deepcopy(suite)
    absolute["baseline_comparison"] = {
        "mode": "absolute_guards",
        "baseline_file": None,
        "comparisons": [],
    }
    evidence.validate_suite_receipt(absolute, policy, policy_digest)
    return absolute


def exact_comparison_suite(
    absolute: dict[str, Any],
    baseline: dict[str, Any],
    baseline_path: Path,
    policy: dict[str, Any],
    policy_digest: str,
) -> dict[str, Any]:
    receipts = evidence.validate_suite_receipt(absolute, policy, policy_digest)
    comparison = evidence.compare_baseline(
        receipts, baseline_path, policy, policy_digest
    )
    result = copy.deepcopy(absolute)
    result["baseline_comparison"] = comparison
    evidence.validate_suite_receipt(result, policy, policy_digest)
    return result


def validate_qualification(
    qualification: dict[str, Any],
    resolution: dict[str, Any],
    suite: dict[str, Any],
    policy: dict[str, Any],
    policy_digest: str,
    baseline: dict[str, Any] | None,
    provenance: dict[str, Any] | None,
    baseline_resolution: dict[str, Any] | None,
) -> None:
    validate_ci_value(qualification, "qualification", "CI qualification")
    common = {
        "policy_digest": policy_digest,
        "scenario_identity_digest": scenario_identity_digest(policy),
        "evidence_schema_digest": evidence_schema_digest(),
        "runner_label": configured_runner_label(),
        "runner_key": suite["runner"]["key"],
        "suite_digest": evidence.digest_value(suite),
        "resolution_digest": evidence.digest_value(resolution),
    }
    for field, value in common.items():
        if qualification[field] != value:
            raise CiError(f"qualification {field} mismatch")
    absolute = absolute_suite_from_comparison(suite, policy, policy_digest)
    if qualification["absolute_suite_digest"] != evidence.digest_value(absolute):
        raise CiError("qualification absolute suite digest mismatch")
    state = qualification["qualification"]
    reason = qualification["reason"]
    source = qualification["baseline_source"]
    selector = qualification["baseline_selector"]
    manifest_digest = qualification["protected_manifest_digest"]
    if (
        selector != resolution["selector"]
        or manifest_digest != resolution["protected_manifest_digest"]
    ):
        raise CiError("qualification selection does not match its resolver proof")
    if state != "qualified" and evidence.canonical_bytes(
        absolute
    ) != evidence.canonical_bytes(suite):
        raise CiError("non-qualified evidence is not the absolute suite")
    if (
        state != "qualified"
        and qualification["absolute_suite_digest"] != qualification["suite_digest"]
    ):
        raise CiError("non-qualified suite digests are not identical")
    if state == "baseline_candidate":
        if (reason, source, selector, manifest_digest) != (
            "no_selector",
            "none",
            None,
            None,
        ):
            raise CiError("candidate qualification state is inconsistent")
        if resolution["state"] != "no_selector" or resolution["source_kind"] != "none":
            raise CiError("candidate resolver state is inconsistent")
        if (
            qualification["comparison_eligible"]
            or baseline is not None
            or provenance is not None
            or baseline_resolution is not None
        ):
            raise CiError("candidate qualification cannot carry a baseline")
    elif state == "pending_comparable_baseline":
        if reason not in {
            "artifact_unavailable",
            "artifact_expired",
            "incompatible_runner",
        }:
            raise CiError("pending qualification reason is invalid")
        if source == "prior_run":
            if selector is None or manifest_digest is not None:
                raise CiError("pending ordinary selection state is inconsistent")
            expected_resolution = (
                reason
                if reason in {"artifact_unavailable", "artifact_expired"}
                else "selected"
            )
            if (
                resolution["state"] != expected_resolution
                or resolution["source_kind"] != "prior_run"
            ):
                raise CiError("pending ordinary resolver state is inconsistent")
        elif source == "protected_manifest":
            if (
                selector is not None
                or manifest_digest is None
                or reason != "incompatible_runner"
            ):
                raise CiError("pending protected selection state is inconsistent")
            if (
                resolution["state"] != "protected_candidates"
                or resolution["source_kind"] != "protected_manifest"
            ):
                raise CiError("pending protected resolver state is inconsistent")
        else:
            raise CiError("pending qualification source is invalid")
        if (
            qualification["comparison_eligible"]
            or baseline is not None
            or provenance is not None
            or baseline_resolution is not None
        ):
            raise CiError("pending qualification cannot carry a baseline")
    elif state == "qualified":
        if reason != "comparable" or source not in {"prior_run", "protected_manifest"}:
            raise CiError("qualified selection state is inconsistent")
        if source == "prior_run" and (selector is None or manifest_digest is not None):
            raise CiError("qualified ordinary selection state is inconsistent")
        if source == "protected_manifest" and (
            selector is not None or manifest_digest is None
        ):
            raise CiError("qualified protected selection state is inconsistent")
        expected_resolution = (
            "selected" if source == "prior_run" else "protected_candidates"
        )
        if resolution["state"] != expected_resolution:
            raise CiError("qualified resolver state is inconsistent")
        if (
            not qualification["comparison_eligible"]
            or baseline is None
            or provenance is None
            or baseline_resolution is None
        ):
            raise CiError("qualified evidence is missing its baseline")
        if qualification["baseline_digest"] != evidence.digest_value(baseline):
            raise CiError("qualification baseline digest mismatch")
        if qualification["baseline_provenance_digest"] != evidence.digest_value(
            provenance
        ):
            raise CiError("qualification provenance digest mismatch")
        if qualification["baseline_resolution_digest"] != evidence.digest_value(
            baseline_resolution
        ):
            raise CiError("qualification baseline-resolution digest mismatch")
        if source == "prior_run" and qualification["workflow_run_id"] in {
            resolution["workflow_run_id"],
            provenance["workflow_run_id"],
        }:
            raise CiError("ordinary qualification run overlaps its selected lineage")
        if source == "protected_manifest":
            if provenance["creation_manifest_digest"] != manifest_digest:
                raise CiError("bootstrap qualification manifest lineage mismatch")
            if (
                provenance["creation_resolution_proof_digest"]
                != resolution["resolution_proof_digest"]
            ):
                raise CiError("bootstrap qualification resolver lineage mismatch")
            creation = (
                provenance["workflow_run_id"],
                provenance["workflow_run_attempt"],
                provenance["source_commit"],
            )
            current = (
                qualification["workflow_run_id"],
                qualification["workflow_run_attempt"],
                qualification["source_commit"],
            )
            if creation != current:
                raise CiError("bootstrap provenance is not bound to its creation run")
    else:
        raise CiError("qualification state is invalid")
    if state != "qualified" and (
        qualification["baseline_digest"] is not None
        or qualification["baseline_provenance_digest"] is not None
        or qualification["baseline_resolution_digest"] is not None
    ):
        raise CiError("non-qualified evidence carries baseline digests")


def validate_stage(
    root: Path,
    *,
    require_qualified: bool = False,
    expected_run: dict[str, Any] | None = None,
    expected_tree_digest: str | None = None,
    defer_selected_qualified_runner: bool = False,
    defer_protected_candidate_runner: bool = False,
) -> tuple[
    dict[str, Any],
    dict[str, Any],
    dict[str, Any] | None,
    dict[str, Any] | None,
    dict[str, Any] | None,
]:
    verify_ci_schema_pins()
    if defer_selected_qualified_runner and defer_protected_candidate_runner:
        raise CiError("runner compatibility deferral modes are mutually exclusive")
    if defer_selected_qualified_runner and (
        not require_qualified or expected_run is None or expected_tree_digest is None
    ):
        raise CiError(
            "runner compatibility deferral is restricted to a sealed selected baseline"
        )
    if defer_protected_candidate_runner and (
        require_qualified or expected_run is None or expected_tree_digest is None
    ):
        raise CiError(
            "protected runner deferral is restricted to sealed candidate evidence"
        )
    if expected_tree_digest is not None:
        if expected_run is None:
            raise CiError("tree digest validation requires selected-run metadata")
        validate_sealed_evidence_tree(root)
        if evidence_tree_digest(root) != expected_tree_digest:
            raise CiError("selected evidence tree digest mismatch")
    before = tree_snapshot(root)
    qualification = load_artifact_json(
        root / QUALIFICATION_NAME, "CI qualification evidence"
    )
    validate_ci_value(qualification, "qualification", "CI qualification")
    expected_files = (
        QUALIFICATION_FILES
        if qualification["qualification"] == "qualified"
        else PENDING_FILES
    )
    validate_flat_tree(root, expected_files)
    policy, policy_digest = evidence.load_policy()
    resolution = load_artifact_json(root / RESOLUTION_NAME, "CI resolution evidence")
    validate_resolution(resolution)
    suite = load_artifact_json(root / SUITE_NAME, "CI suite evidence")
    evidence.validate_suite_receipt(suite, policy, policy_digest)
    if not (defer_selected_qualified_runner or defer_protected_candidate_runner):
        validate_workflow_runner(suite["runner"], policy)
    baseline: dict[str, Any] | None = None
    provenance: dict[str, Any] | None = None
    baseline_resolution: dict[str, Any] | None = None
    if qualification["qualification"] == "qualified":
        baseline = load_artifact_json(root / BASELINE_NAME, "CI baseline evidence")
        provenance = load_artifact_json(
            root / PROVENANCE_NAME, "CI baseline provenance"
        )
        baseline_resolution = load_artifact_json(
            root / BASELINE_RESOLUTION_NAME, "CI baseline creation resolution"
        )
        rederive_baseline(baseline, policy, policy_digest)
        validate_provenance(
            provenance, baseline, baseline_resolution, policy, policy_digest
        )
        absolute = absolute_suite_from_comparison(suite, policy, policy_digest)
        expected_suite = exact_comparison_suite(
            absolute, baseline, root / BASELINE_NAME, policy, policy_digest
        )
        if evidence.canonical_bytes(expected_suite) != evidence.canonical_bytes(suite):
            raise CiError("qualified suite comparison was not canonically derived")
    validate_qualification(
        qualification,
        resolution,
        suite,
        policy,
        policy_digest,
        baseline,
        provenance,
        baseline_resolution,
    )
    if require_qualified and qualification["qualification"] != "qualified":
        raise CiError("selected artifact is not qualified baseline evidence")
    if defer_protected_candidate_runner and (
        qualification["qualification"] != "baseline_candidate"
        or qualification["reason"] != "no_selector"
        or qualification["baseline_source"] != "none"
        or resolution["state"] != "no_selector"
        or resolution["source_kind"] != "none"
        or baseline is not None
        or provenance is not None
        or baseline_resolution is not None
    ):
        raise CiError("protected runner deferral received non-candidate evidence")
    if expected_run is not None:
        checks = {
            "workflow_path": WORKFLOW_PATH,
            "workflow_run_id": expected_run["workflow_run_id"],
            "workflow_run_attempt": expected_run["workflow_run_attempt"],
            "source_commit": expected_run["source_commit"],
        }
        for field, value in checks.items():
            if qualification[field] != value:
                raise CiError(f"selected qualification {field} mismatch")
    if (
        expected_tree_digest is not None
        and evidence_tree_digest(root) != expected_tree_digest
    ):
        raise CiError("selected evidence tree changed during semantic validation")
    if expected_tree_digest is not None:
        validate_sealed_evidence_tree(root)
    if tree_snapshot(root) != before:
        raise CiError("evidence validation mutated its source tree")
    return qualification, suite, baseline, provenance, baseline_resolution


def standalone_baseline_outcome(path: Path) -> str:
    try:
        completed = subprocess.run(
            [
                sys.executable,
                str(PERF / "run.py"),
                "validate-baseline",
                "--receipt",
                str(path),
                "--require-current-runner",
            ],
            cwd=RUST_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise CiError("standalone baseline validator failed") from error
    expected_by_code = {
        0: {
            "schema": "staxtrace.performance-validation",
            "artifact": "baseline",
            "outcome": "comparable",
        },
        3: {
            "schema": "staxtrace.performance-validation",
            "artifact": "baseline",
            "outcome": "incompatible",
        },
    }
    expected = expected_by_code.get(completed.returncode)
    if expected is None or completed.stderr:
        raise CiError("standalone baseline validator returned a hard failure")
    canonical = json.dumps(expected, sort_keys=True, separators=(",", ":")) + "\n"
    if completed.stdout != canonical:
        raise CiError("standalone baseline validator outcome is malformed")
    return expected["outcome"]


def create_provenance(
    baseline: dict[str, Any],
    policy: dict[str, Any],
    policy_digest: str,
    *,
    workflow_run_id: int,
    workflow_run_attempt: int,
    source_commit: str,
    creation_manifest_digest: str,
    creation_resolution_proof_digest: str,
    protected_sources: list[dict[str, Any]],
) -> dict[str, Any]:
    if len(protected_sources) != 3:
        raise CiError("baseline provenance requires exactly three protected sources")
    if any(source["source_commit"] != source_commit for source in protected_sources):
        raise CiError("baseline provenance source commit differs from its cohort")
    result = {
        "schema": "staxtrace.performance-ci-baseline-provenance",
        "creation_source": "protected_manifest",
        "workflow_path": WORKFLOW_PATH,
        "workflow_run_id": workflow_run_id,
        "workflow_run_attempt": workflow_run_attempt,
        "source_commit": source_commit,
        "policy_digest": policy_digest,
        "scenario_identity_digest": scenario_identity_digest(policy),
        "evidence_schema_digest": evidence_schema_digest(),
        "runner_label": configured_runner_label(),
        "runner_key": baseline["runner"]["key"],
        "baseline_digest": evidence.digest_value(baseline),
        "creation_manifest_digest": creation_manifest_digest,
        "creation_resolution_proof_digest": creation_resolution_proof_digest,
        "aggregate_digest": "0" * 64,
        "source_suite_digests": [
            evidence.digest_value(suite) for suite in baseline["source_suites"]
        ],
        "source_harness_run_ids": [
            suite["run_id"] for suite in baseline["source_suites"]
        ],
        "source_workflow_run_ids": [
            source["workflow_run_id"] for source in protected_sources
        ],
        "source_workflow_run_attempts": [
            source["workflow_run_attempt"] for source in protected_sources
        ],
    }
    result["aggregate_digest"] = evidence.digest_value(provenance_projection(result))
    validate_provenance(
        result,
        baseline,
        {
            "schema": "staxtrace.performance-ci-resolution",
            "state": "protected_candidates",
            "source_kind": "protected_manifest",
            "workflow_path": WORKFLOW_PATH,
            "runner_label": configured_runner_label(),
            "selector": None,
            "workflow_id": None,
            "workflow_run_id": None,
            "workflow_run_attempt": None,
            "source_commit": None,
            "artifact_id": None,
            "artifact_name": None,
            "artifact_size": None,
            "artifact_digest": None,
            "artifact_tree_digest": None,
            "protected_manifest_digest": creation_manifest_digest,
            "resolution_proof_digest": creation_resolution_proof_digest,
            "protected_sources": protected_sources,
        },
        policy,
        policy_digest,
    )
    return result


def derive_protected_baseline(
    selected_root: Path,
    resolution: dict[str, Any],
    policy: dict[str, Any],
    policy_digest: str,
    current_run: int,
    current_attempt: int,
    current_commit: str,
) -> tuple[dict[str, Any], dict[str, Any]]:
    suites: list[dict[str, Any]] = []
    runner_keys: set[str] = set()
    policy_ids: set[str] = set()
    scenario_ids: set[str] = set()
    suite_digests: set[str] = set()
    harness_runs: set[str] = set()
    for index, source in enumerate(resolution["protected_sources"], start=1):
        root = selected_root / f"candidate-{index}"
        qualification, suite, baseline, provenance, baseline_resolution = (
            validate_stage(
                root,
                expected_run=source,
                expected_tree_digest=source["artifact_tree_digest"],
                defer_protected_candidate_runner=True,
            )
        )
        if (
            qualification["qualification"] != "baseline_candidate"
            or qualification["reason"] != "no_selector"
        ):
            raise CiError("protected manifest names a non-candidate artifact")
        if (
            baseline is not None
            or provenance is not None
            or baseline_resolution is not None
        ):
            raise CiError("protected candidate carries baseline evidence")
        suites.append(suite)
        runner_keys.add(suite["runner"]["key"])
        policy_ids.add(qualification["policy_digest"])
        scenario_ids.add(qualification["scenario_identity_digest"])
        suite_digests.add(evidence.digest_value(suite))
        harness_runs.add(suite["run_id"])
    if len(runner_keys) != 1 or runner_keys != {suites[0]["runner"]["key"]}:
        raise CiError("protected candidates differ in runner identity")
    if policy_ids != {policy_digest} or scenario_ids != {
        scenario_identity_digest(policy)
    }:
        raise CiError("protected candidates differ from current policy or scenarios")
    if len(suite_digests) != 3 or len(harness_runs) != 3:
        raise CiError("protected candidate suites or harness runs are not distinct")
    if any(suite["runner"] != suites[0]["runner"] for suite in suites):
        raise CiError("protected candidate runner records differ")
    with tempfile.TemporaryDirectory(prefix="stax-perf-bootstrap-") as directory:
        root = Path(directory)
        paths: list[str] = []
        for index, suite in enumerate(suites, start=1):
            path = root / f"candidate-{index}.json"
            evidence.atomic_write_json(path, suite)
            paths.append(str(path))
        output = root / BASELINE_NAME
        with contextlib.redirect_stdout(io.StringIO()):
            evidence.create_baseline(
                argparse.Namespace(output=str(output), receipts=paths)
            )
        baseline = evidence.load_json(output)
    rederive_baseline(baseline, policy, policy_digest)
    provenance = create_provenance(
        baseline,
        policy,
        policy_digest,
        workflow_run_id=current_run,
        workflow_run_attempt=current_attempt,
        source_commit=current_commit,
        creation_manifest_digest=resolution["protected_manifest_digest"],
        creation_resolution_proof_digest=resolution["resolution_proof_digest"],
        protected_sources=resolution["protected_sources"],
    )
    return baseline, provenance


def qualification_value(
    *,
    state: str,
    reason: str,
    source: str,
    selector: str | None,
    manifest_digest: str | None,
    resolution: dict[str, Any],
    current_run: int,
    current_attempt: int,
    current_commit: str,
    absolute_suite: dict[str, Any],
    suite: dict[str, Any],
    policy: dict[str, Any],
    policy_digest: str,
    baseline: dict[str, Any] | None,
    provenance: dict[str, Any] | None,
    baseline_resolution: dict[str, Any] | None,
) -> dict[str, Any]:
    result = {
        "schema": "staxtrace.performance-ci-qualification",
        "qualification": state,
        "reason": reason,
        "baseline_source": source,
        "baseline_selector": selector,
        "protected_manifest_digest": manifest_digest,
        "workflow_path": WORKFLOW_PATH,
        "workflow_run_id": current_run,
        "workflow_run_attempt": current_attempt,
        "source_commit": current_commit,
        "policy_digest": policy_digest,
        "scenario_identity_digest": scenario_identity_digest(policy),
        "evidence_schema_digest": evidence_schema_digest(),
        "runner_label": configured_runner_label(),
        "runner_key": suite["runner"]["key"],
        "absolute_suite_digest": evidence.digest_value(absolute_suite),
        "suite_digest": evidence.digest_value(suite),
        "resolution_digest": evidence.digest_value(resolution),
        "baseline_digest": evidence.digest_value(baseline)
        if baseline is not None
        else None,
        "baseline_provenance_digest": evidence.digest_value(provenance)
        if provenance is not None
        else None,
        "baseline_resolution_digest": evidence.digest_value(baseline_resolution)
        if baseline_resolution is not None
        else None,
        "comparison_eligible": state == "qualified",
        "publication_eligible": False,
    }
    validate_qualification(
        result,
        resolution,
        suite,
        policy,
        policy_digest,
        baseline,
        provenance,
        baseline_resolution,
    )
    return result


def publish_stage(
    destination: Path,
    qualification: dict[str, Any],
    resolution: dict[str, Any],
    suite: dict[str, Any],
    baseline: dict[str, Any] | None,
    provenance: dict[str, Any] | None,
    baseline_resolution: dict[str, Any] | None,
) -> None:
    fresh_output_path(destination, "upload staging directory")
    temporary = Path(
        tempfile.mkdtemp(prefix=".stax-perf-stage-", dir=destination.parent)
    )
    try:
        temporary.chmod(0o700)
        values: dict[str, dict[str, Any]] = {
            QUALIFICATION_NAME: qualification,
            RESOLUTION_NAME: resolution,
            SUITE_NAME: suite,
        }
        if (
            baseline is not None
            and provenance is not None
            and baseline_resolution is not None
        ):
            values[BASELINE_NAME] = baseline
            values[PROVENANCE_NAME] = provenance
            values[BASELINE_RESOLUTION_NAME] = baseline_resolution
        for name, value in values.items():
            evidence.atomic_write_json(temporary / name, value)
            (temporary / name).chmod(0o600)
        validate_stage(temporary)
        temporary.replace(destination)
        validate_stage(destination)
    except Exception:
        shutil.rmtree(temporary, ignore_errors=True)
        if (
            destination.exists()
            and destination.is_dir()
            and not destination.is_symlink()
        ):
            shutil.rmtree(destination, ignore_errors=True)
        raise


def append_github_output(path_value: str | None, qualification: str) -> None:
    if not path_value:
        return
    path = Path(path_value)
    if path.is_symlink() or not path.is_file():
        raise CiError("workflow output channel is invalid")
    with path.open("a", encoding="utf-8") as handle:
        handle.write("upload_ready=true\n")
        handle.write(f"qualification={qualification}\n")


def qualify_command(args: argparse.Namespace) -> int:
    verify_ci_schema_pins()
    current_run, current_attempt, current_commit = context_identity(args)
    absolute, policy, policy_digest = load_absolute_suite(Path(args.receipt))
    validate_snapshot(Path(args.snapshot), absolute, policy, policy_digest)
    resolution = load_artifact_json(Path(args.resolution), "CI resolution evidence")
    validate_resolution(resolution)
    stage = Path(args.staging_dir)
    fresh_output_path(stage, "upload staging directory")
    state = resolution["state"]
    baseline: dict[str, Any] | None = None
    provenance: dict[str, Any] | None = None
    baseline_resolution: dict[str, Any] | None = None
    suite = absolute
    selector = resolution["selector"]
    manifest_digest = resolution["protected_manifest_digest"]
    if state == "no_selector":
        q_state, reason, source = "baseline_candidate", "no_selector", "none"
    elif state in {"artifact_unavailable", "artifact_expired"}:
        q_state = "pending_comparable_baseline"
        reason = state
        source = "prior_run"
    elif state == "selected":
        selected_root = Path(args.selected_dir)
        _, _, baseline, provenance, baseline_resolution = validate_stage(
            selected_root,
            require_qualified=True,
            expected_run=resolution,
            expected_tree_digest=resolution["artifact_tree_digest"],
            defer_selected_qualified_runner=True,
        )
        if not (
            baseline is not None
            and provenance is not None
            and baseline_resolution is not None
        ):
            raise CiError("selected qualified evidence is missing its baseline lineage")
        outcome = standalone_baseline_outcome(selected_root / BASELINE_NAME)
        if outcome == "incompatible":
            q_state, reason, source = (
                "pending_comparable_baseline",
                "incompatible_runner",
                "prior_run",
            )
            baseline = provenance = baseline_resolution = None
        else:
            suite = exact_comparison_suite(
                absolute, baseline, selected_root / BASELINE_NAME, policy, policy_digest
            )
            q_state, reason, source = "qualified", "comparable", "prior_run"
    elif state == "protected_candidates":
        selected_root = Path(args.selected_dir)
        baseline, provenance = derive_protected_baseline(
            selected_root,
            resolution,
            policy,
            policy_digest,
            current_run,
            current_attempt,
            current_commit,
        )
        baseline_resolution = resolution
        with tempfile.TemporaryDirectory(prefix="stax-perf-validate-") as directory:
            baseline_path = Path(directory) / BASELINE_NAME
            evidence.atomic_write_json(baseline_path, baseline)
            outcome = standalone_baseline_outcome(baseline_path)
            if outcome == "incompatible":
                q_state, reason, source = (
                    "pending_comparable_baseline",
                    "incompatible_runner",
                    "protected_manifest",
                )
                baseline = provenance = baseline_resolution = None
            else:
                suite = exact_comparison_suite(
                    absolute, baseline, baseline_path, policy, policy_digest
                )
                q_state, reason, source = (
                    "qualified",
                    "comparable",
                    "protected_manifest",
                )
    else:
        raise CiError("unsupported resolver state")
    qualification = qualification_value(
        state=q_state,
        reason=reason,
        source=source,
        selector=selector,
        manifest_digest=manifest_digest,
        resolution=resolution,
        current_run=current_run,
        current_attempt=current_attempt,
        current_commit=current_commit,
        absolute_suite=absolute,
        suite=suite,
        policy=policy,
        policy_digest=policy_digest,
        baseline=baseline,
        provenance=provenance,
        baseline_resolution=baseline_resolution,
    )
    publish_stage(
        stage,
        qualification,
        resolution,
        suite,
        baseline,
        provenance,
        baseline_resolution,
    )
    try:
        append_github_output(args.github_output, q_state)
    except Exception:
        if stage.is_dir() and not stage.is_symlink():
            shutil.rmtree(stage, ignore_errors=True)
        raise
    print(json.dumps({"qualification": q_state}, sort_keys=True, separators=(",", ":")))
    return 0


def enforce_command(args: argparse.Namespace) -> int:
    qualification, _, _, _, _ = validate_stage(Path(args.staging_dir))
    state = qualification["qualification"]
    if state != "qualified":
        print(
            json.dumps(
                {"qualification": state, "outcome": "pending"},
                sort_keys=True,
                separators=(",", ":"),
            )
        )
        return 4
    print(
        json.dumps(
            {"qualification": state, "outcome": "pass"},
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


def validate_stage_command(args: argparse.Namespace) -> int:
    validate_stage(Path(args.staging_dir), require_qualified=args.require_qualified)
    print("performance CI evidence validation PASS")
    return 0


def self_check(_: argparse.Namespace) -> int:
    verify_ci_schema_pins()
    evidence.load_policy()
    configured_runner_label()
    channel = pinned_toolchain_channel()
    try:
        active = subprocess.run(
            ["rustc", "--version"],
            cwd=RUST_ROOT,
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        raise CiError("unable to validate the pinned Rust toolchain") from error
    if not active.startswith(f"rustc {channel} "):
        raise CiError("active Rust toolchain differs from the checked-in pin")
    print("performance CI schemas and policy self-check PASS")
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="verb", required=True)

    commands.add_parser("self-check").set_defaults(handler=self_check)

    snapshot = commands.add_parser("snapshot")
    snapshot.add_argument("--receipt", required=True)
    snapshot.add_argument("--output", required=True)
    snapshot.set_defaults(handler=snapshot_command)

    resolve = commands.add_parser("resolve")
    resolve.add_argument("--selector", default="")
    resolve.add_argument("--repository", required=True)
    resolve.add_argument("--current-run-id", required=True)
    resolve.add_argument("--current-run-attempt", required=True)
    resolve.add_argument("--current-commit", required=True)
    resolve.add_argument("--resolution", required=True)
    resolve.add_argument("--selected-dir", required=True)
    resolve.add_argument("--bootstrap", action="store_true")
    resolve.add_argument("--protected-environment", default="")
    resolve.add_argument("--protected-manifest-env", default="")
    resolve.set_defaults(handler=resolve_command)

    qualify = commands.add_parser("qualify")
    qualify.add_argument("--receipt", required=True)
    qualify.add_argument("--snapshot", required=True)
    qualify.add_argument("--resolution", required=True)
    qualify.add_argument("--selected-dir", required=True)
    qualify.add_argument("--staging-dir", required=True)
    qualify.add_argument("--current-run-id", required=True)
    qualify.add_argument("--current-run-attempt", required=True)
    qualify.add_argument("--current-commit", required=True)
    qualify.add_argument("--github-output")
    qualify.set_defaults(handler=qualify_command)

    enforce = commands.add_parser("enforce")
    enforce.add_argument("--staging-dir", required=True)
    enforce.set_defaults(handler=enforce_command)

    validate = commands.add_parser("validate-stage")
    validate.add_argument("--staging-dir", required=True)
    validate.add_argument("--require-qualified", action="store_true")
    validate.set_defaults(handler=validate_stage_command)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        return int(args.handler(args))
    except RecursionError:
        print("performance CI error: evidence nesting exceeds limit", file=sys.stderr)
        return 2
    except (
        CiError,
        evidence.ValidationError,
        OSError,
        ValueError,
        TimeoutError,
    ) as error:
        message = sanitize_error_message(error)
        print(f"performance CI error: {message}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
