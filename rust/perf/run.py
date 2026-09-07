#!/usr/bin/env python3
"""Bound release scenarios and validate sanitized performance evidence."""

from __future__ import annotations

import argparse
import copy
import hashlib
import ipaddress
import json
import math
import os
from pathlib import Path
import platform
import re
import secrets
import signal
import statistics
import subprocess
import sys
import time
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
PERF = ROOT / "perf"
POLICY_PATH = PERF / "workloads.json"
SCENARIO_DIR = PERF / "scenarios"
SCHEMA_PATHS = {
    "policy": PERF / "policy.schema.json",
    "receipt": PERF / "receipt.schema.json",
    "failure": PERF / "failure.schema.json",
    "suite": PERF / "suite.schema.json",
    "baseline": PERF / "baseline.schema.json",
    "fragment": PERF / "fragment.schema.json",
    "validation": PERF / "validation.schema.json",
}
SCHEMA_REFERENCES = {path.name: path for path in SCHEMA_PATHS.values()}

# Independent pin for the complete semantic policy. The Rust tests consume the
# policy fields; the shell consumes values only after this digest passes.
EXPECTED_POLICY_DIGEST = (
    "5786a16e7178a31d793de9b5aa54cba81767108f547ad16197b19d4a26a82327"
)
EXPECTED_SCHEMA_DIGESTS = {
    "policy": "e1bb712c06eb775b88f087bbc375f26c9b7940d1679b887e09889e3105bca87d",
    "receipt": "5d56d66869a04a344fca835248cd67774f821c2d3cb5806cc4280cea7aecede9",
    "failure": "a994ca897376ef63b60bb2df72252a677071efdc2c4be0b5540aa010099d86bb",
    "suite": "bc89f68f2e796de6631ec68e00eb0c30bd3a95fcd271683006b5f68be2025b62",
    "baseline": "092c6284bd9727eae76437aeec0fcd27e7175f7d4bae1caaa49624a731f21e1d",
    "fragment": "fbc14f985b6c0a08ee6c454f71fd80c6e1e960a8bd1ef73a6a776a41b32c543a",
    "validation": "6652d83e45e152df19da18297a41d41493d9aa450640c78251c9ad05625743e4",
}
EXPECTED_SCENARIOS = {
    "query_cold",
    "query_warm",
    "one_file_append",
    "batch_ingest",
    "noop_refresh",
    "incremental_reindex",
    "full_reindex",
    "concurrent_read_write",
    "hook_execution",
}
SUITE_SETUP_SCENARIO = "suite_setup"
SUITE_FINALIZATION_SCENARIO = "suite_finalization"
SUITE_SENTINELS = {SUITE_SETUP_SCENARIO, SUITE_FINALIZATION_SCENARIO}
RUNNER_ENV_FIELDS = {
    "STAX_PERF_RUNNER_OS": "os",
    "STAX_PERF_RUNNER_ARCH": "architecture",
    "STAX_PERF_RUNNER_CLASS": "runner_class",
    "STAX_PERF_RUNNER_TOOLCHAIN": "toolchain",
    "STAX_PERF_RUNNER_KEY": "key",
}
RUNNER_DIGEST_ENV = "STAX_PERF_RUNNER_DIGEST"
SUITE_START_ENV = "STAX_PERF_SUITE_START"
SUITE_TIMEOUT_ENV = "STAX_PERF_SUITE_TIMEOUT"
FINALIZER_EVIDENCE_EXIT = 20
FINALIZER_COMPARISON_EXIT = 21
FINALIZER_TERMINAL_EXIT = 22
SUITE_RECEIPT_NAME = "suite-receipt.json"


class ValidationError(ValueError):
    """A sanitized evidence or policy validation failure."""


class IncompatibleEvidence(ValidationError):
    """Schema-valid evidence for a different runner/toolchain key."""


class ComparisonFailure(ValidationError):
    """A verified same-runner baseline comparison regressed."""


class TerminalScenarioEvidence(ValidationError):
    """Valid current-run scenario evidence already records a terminal failure."""


def staged_suite_receipt_name(run_id: str) -> str:
    if re.fullmatch(r"[a-f0-9]{32}", run_id) is None:
        raise ValidationError("suite staging requires a sanitized run identity")
    return f".suite-receipt.{run_id}.pending"


def load_json(path: Path) -> dict[str, Any]:
    try:
        with path.open("r", encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValidationError(f"{path.name}: unreadable JSON") from error
    if not isinstance(value, dict):
        raise ValidationError(f"{path.name}: expected a JSON object")
    return value


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def digest_value(value: Any) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def is_number(value: Any) -> bool:
    return not isinstance(value, bool) and isinstance(value, (int, float))


def finite_number(value: Any, label: str) -> float:
    if not is_number(value):
        raise ValidationError(f"{label}: expected a number")
    number = float(value)
    if not math.isfinite(number):
        raise ValidationError(f"{label}: expected a finite number")
    return number


def finite_environment_number(value: Any, label: str) -> float:
    if not isinstance(value, str) or not value:
        raise ValidationError(f"{label}: expected a numeric environment value")
    try:
        number = float(value)
    except ValueError as error:
        raise ValidationError(
            f"{label}: expected a numeric environment value"
        ) from error
    if not math.isfinite(number):
        raise ValidationError(f"{label}: expected a finite number")
    return number


def type_matches(value: Any, expected: str) -> bool:
    return {
        "object": isinstance(value, dict),
        "array": isinstance(value, list),
        "string": isinstance(value, str),
        "integer": not isinstance(value, bool) and isinstance(value, int),
        "number": is_number(value) and math.isfinite(float(value)),
        "boolean": isinstance(value, bool),
        "null": value is None,
    }[expected]


def validate_schema(value: Any, schema: dict[str, Any], label: str) -> None:
    reference = schema.get("$ref")
    if reference is not None:
        if set(schema) != {"$ref"} or reference not in SCHEMA_REFERENCES:
            raise ValidationError(f"{label}: unsupported schema reference")
        validate_schema(value, load_json(SCHEMA_REFERENCES[reference]), label)
        return

    expected_type = schema.get("type")
    if expected_type is not None:
        choices = expected_type if isinstance(expected_type, list) else [expected_type]
        if not any(type_matches(value, choice) for choice in choices):
            raise ValidationError(f"{label}: wrong JSON type")

    if "const" in schema and canonical_bytes(value) != canonical_bytes(schema["const"]):
        raise ValidationError(f"{label}: constant value changed")
    if "enum" in schema and all(
        canonical_bytes(value) != canonical_bytes(candidate)
        for candidate in schema["enum"]
    ):
        raise ValidationError(f"{label}: value is not allowed")

    if isinstance(value, dict):
        required = set(schema.get("required", []))
        missing = sorted(required - value.keys())
        if missing:
            raise ValidationError(f"{label}: missing fields {missing}")
        if len(value) < int(schema.get("minProperties", 0)):
            raise ValidationError(f"{label}: too few fields")
        properties = schema.get("properties", {})
        additional = schema.get("additionalProperties", True)
        property_names = schema.get("propertyNames")
        if isinstance(property_names, dict):
            for key in value:
                validate_schema(key, property_names, f"{label}.field")
        extra = sorted(set(value) - set(properties))
        if additional is False and extra:
            raise ValidationError(f"{label}: unexpected fields {extra}")
        for key, item in value.items():
            if key in properties:
                validate_schema(item, properties[key], f"{label}.{key}")
            elif isinstance(additional, dict):
                validate_schema(item, additional, f"{label}.{key}")

    if isinstance(value, list):
        minimum_items = int(schema.get("minItems", 0))
        maximum_items = schema.get("maxItems")
        if len(value) < minimum_items:
            raise ValidationError(f"{label}: too few items")
        if maximum_items is not None and len(value) > int(maximum_items):
            raise ValidationError(f"{label}: too many items")
        if schema.get("uniqueItems"):
            encoded = [canonical_bytes(item) for item in value]
            if len(set(encoded)) != len(encoded):
                raise ValidationError(f"{label}: duplicate items")
        item_schema = schema.get("items")
        if isinstance(item_schema, dict):
            for index, item in enumerate(value):
                validate_schema(item, item_schema, f"{label}[{index}]")

    if isinstance(value, str):
        if len(value) < int(schema.get("minLength", 0)):
            raise ValidationError(f"{label}: string is too short")
        maximum_length = schema.get("maxLength")
        if maximum_length is not None and len(value) > int(maximum_length):
            raise ValidationError(f"{label}: string is too long")
        pattern = schema.get("pattern")
        if pattern is not None and re.search(pattern, value) is None:
            raise ValidationError(f"{label}: string format is invalid")

    if is_number(value):
        number = finite_number(value, label)
        if "minimum" in schema and number < float(schema["minimum"]):
            raise ValidationError(f"{label}: below minimum")
        if "exclusiveMinimum" in schema and number <= float(schema["exclusiveMinimum"]):
            raise ValidationError(f"{label}: below exclusive minimum")
        if "maximum" in schema and number > float(schema["maximum"]):
            raise ValidationError(f"{label}: above maximum")


def validate_private_string(value: str, label: str) -> None:
    stripped = value.strip()
    if not stripped:
        return
    if "\x00" in value or "\n" in value or "\r" in value:
        raise ValidationError(f"{label}: command-shaped string is forbidden")
    # A reserved local host stays private when written with its DNS trailing
    # dot, a port, or a path delimiter. Check this before schema/filename
    # exceptions so neither typed field can accidentally whitelist it.
    if re.search(
        r"(?i)(?<![A-Za-z0-9_-])localhost(?:\.)?(?=$|[:/\\])",
        stripped,
    ):
        raise ValidationError(f"{label}: hostname is forbidden")
    if re.search(r"(?:[A-Za-z][A-Za-z0-9+.-]*://|\bwww\.)", value):
        raise ValidationError(f"{label}: URL is forbidden")
    if stripped.startswith(("/", "~/", "~\\", "\\\\")) or re.match(
        r"^[A-Za-z]:[\\/]", stripped
    ):
        raise ValidationError(f"{label}: absolute path is forbidden")
    if ".." in re.split(r"[\\/]", stripped):
        raise ValidationError(f"{label}: path traversal is forbidden")

    if re.search(r"(?<![0-9.])(?:\d{1,3}\.){3}\d{1,3}(?![0-9.])", value):
        raise ValidationError(f"{label}: network identifier is forbidden")
    ipv6_candidates = re.findall(
        r"(?<![0-9A-Fa-f:])(?:[0-9A-Fa-f]{0,4}:){2,}[0-9A-Fa-f:]{0,4}"
        r"(?![0-9A-Fa-f:])",
        value,
    )
    candidates = [
        stripped.strip("[](),"),
        *re.split(r"[\s,;()\[\]]+", stripped),
        *ipv6_candidates,
    ]
    for candidate in candidates:
        address = candidate.strip("[]")
        if not address:
            continue
        try:
            ipaddress.ip_address(address.split("%", 1)[0])
        except ValueError:
            continue
        raise ValidationError(f"{label}: network identifier is forbidden")
    baseline_filename = label.endswith(".baseline_file") and re.fullmatch(
        r"[a-z0-9][a-z0-9_-]{0,126}\.json", stripped
    )
    schema_identifier = label.endswith(".schema") and re.fullmatch(
        r"staxtrace\.[a-z][a-z0-9-]*", stripped
    )
    dns_shaped = re.search(
        r"(?i)(?<![A-Za-z0-9_-])"
        r"(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)+"
        r"[A-Za-z](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?"
        r"(?![A-Za-z0-9_-])",
        value,
    )
    if dns_shaped and not baseline_filename and not schema_identifier:
        raise ValidationError(f"{label}: hostname is forbidden")
    if (
        any(token in value for token in ("$(", "`", "&&", "||", ";"))
        or re.search(r"(?:^|\s)(?:sh|bash|zsh)\s+-c\b", value)
        or re.search(r"(?:^|\s)(?:rm|curl|wget)\s+-", value)
        or re.search(r"(?:^|\s)(?:python\d*|node)\s+-[cem]\b", value)
        or re.search(r"(?:^|\s)cargo\s+(?:build|run|test)\b", value)
        or re.search(r"(?:^|\s)rustc\s+--", value)
        or re.search(r"(?:^|\s)[<>|](?:\s|$)", value)
    ):
        raise ValidationError(f"{label}: command-shaped string is forbidden")


def validate_privacy(value: Any, label: str = "evidence") -> None:
    forbidden_keys = {
        "argv",
        "arguments",
        "command",
        "cwd",
        "workingdirectory",
        "hostname",
        "host",
        "networkidentifier",
    }
    if isinstance(value, dict):
        for key, item in value.items():
            validate_private_string(key, f"{label}.object_key")
            normalized_key = re.sub(r"[^a-z]", "", key.lower())
            if normalized_key in forbidden_keys:
                raise ValidationError(f"{label}: forbidden private field")
            validate_privacy(item, f"{label}.{key}")
    elif isinstance(value, list):
        for index, item in enumerate(value):
            validate_privacy(item, f"{label}[{index}]")
    elif isinstance(value, str):
        validate_private_string(value, label)


def verify_schema_pins() -> None:
    if set(EXPECTED_SCHEMA_DIGESTS) != set(SCHEMA_PATHS):
        raise ValidationError("performance schema pin set is incomplete")
    for name, path in SCHEMA_PATHS.items():
        schema = load_json(path)
        if digest_value(schema) != EXPECTED_SCHEMA_DIGESTS[name]:
            raise ValidationError(
                f"performance {name} schema failed its independent digest pin"
            )


def load_core_policy() -> dict[str, Any]:
    verify_schema_pins()
    core = load_json(POLICY_PATH)
    validate_schema(core, load_json(SCHEMA_PATHS["policy"]), "policy")
    digest = digest_value(core)
    if digest != EXPECTED_POLICY_DIGEST:
        raise ValidationError("performance policy failed its independent digest pin")
    if set(core["scenarios"]) != EXPECTED_SCENARIOS:
        raise ValidationError("performance policy scenario set changed")
    fixed = {
        "suite_timeout_seconds": 900,
        "setup_timeout_seconds": 900,
        "scenario_timeout_seconds": 180,
        "runner_classes": ["local", "dedicated", "hosted"],
        "resource_limits": {"peak_rss_bytes": 1_073_741_824, "open_files": 256},
        "resource_observation": {
            "interval_milliseconds": 250,
            "observer_wall_seconds_limit": 30.0,
        },
        "baseline_policy": {
            "clean_run_count": 3,
            "latency_regression_fraction": 0.20,
            "rss_regression_fraction": 0.20,
            "open_file_regression_fraction": 0.20,
            "throughput_regression_fraction": 0.15,
        },
    }
    for key, expected in fixed.items():
        if canonical_bytes(core[key]) != canonical_bytes(expected):
            raise ValidationError(f"performance policy fixed field changed: {key}")
    return core


def merge_scenario_fragments(
    core: dict[str, Any], scenario_dir: Path
) -> dict[str, Any]:
    combined = copy.deepcopy(core)
    known = set(combined["scenarios"])
    try:
        fragments = sorted(scenario_dir.glob("*.json"), key=lambda path: path.name)
    except OSError as error:
        raise ValidationError("unable to enumerate additive scenarios") from error
    fragment_schema = load_json(SCHEMA_PATHS["fragment"])
    for path in fragments:
        if path.is_symlink() or not path.is_file():
            raise ValidationError("additive scenario must be a regular JSON file")
        fragment = load_json(path)
        validate_schema(fragment, fragment_schema, f"scenario fragment {path.name}")
        validate_privacy(fragment, f"scenario fragment {path.name}")
        scenario = fragment["scenario"]
        if scenario in known:
            raise ValidationError(
                "additive scenario collides with a core or prior identifier"
            )
        known.add(scenario)
        combined["scenarios"][scenario] = {
            key: value
            for key, value in fragment.items()
            if key not in {"schema", "scenario"}
        }
    validate_schema(combined, load_json(SCHEMA_PATHS["policy"]), "combined policy")
    return combined


def load_policy() -> tuple[dict[str, Any], str]:
    combined = merge_scenario_fragments(load_core_policy(), SCENARIO_DIR)
    return combined, digest_value(combined)


def runner_identity(policy: dict[str, Any] | None = None) -> dict[str, str]:
    if policy is None:
        policy, _ = load_policy()
    os_name = platform.system().lower()
    if os_name == "darwin":
        os_name = "macos"
    architecture = platform.machine().lower()
    if architecture == "arm64":
        architecture = "aarch64"
    elif architecture == "amd64":
        architecture = "x86_64"
    elif architecture in {"i386", "i686"}:
        architecture = "x86"
    runner_class = os.environ.get("STAX_PERF_RUNNER_CLASS", "local")
    if runner_class not in policy["runner_classes"]:
        raise ValidationError("runner class is not in the checked-in allowlist")
    try:
        toolchain = subprocess.run(
            ["rustc", "--version"],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        raise ValidationError("unable to identify the Rust toolchain") from error
    if not toolchain:
        raise ValidationError("Rust toolchain identity is empty")
    key = f"{os_name}-{architecture}-{runner_class}-{toolchain}"
    runner = {
        "os": os_name,
        "architecture": architecture,
        "runner_class": runner_class,
        "toolchain": toolchain,
        "key": key,
    }
    validate_privacy(runner, "runner")
    validate_runner(runner, "runner", policy)
    return runner


def validate_runner(runner: Any, label: str, policy: dict[str, Any]) -> None:
    schema = load_json(SCHEMA_PATHS["receipt"])["properties"]["runner"]
    validate_schema(runner, schema, label)
    validate_privacy(runner, label)
    if runner["runner_class"] not in policy["runner_classes"]:
        raise ValidationError(
            f"{label}: runner class is not in the checked-in allowlist"
        )
    if runner["os"] not in {"freebsd", "linux", "macos", "windows"}:
        raise ValidationError(f"{label}: operating system identity is not allowed")
    if runner["architecture"] not in {"aarch64", "armv7", "riscv64", "x86", "x86_64"}:
        raise ValidationError(f"{label}: architecture identity is not allowed")
    if (
        re.fullmatch(
            r"rustc \d+\.\d+\.\d+(?:-[A-Za-z0-9.]+)? "
            r"\([a-f0-9]+ \d{4}-\d{2}-\d{2}\)",
            runner["toolchain"],
        )
        is None
    ):
        raise ValidationError(f"{label}: toolchain identity is malformed")
    expected_key = (
        f"{runner['os']}-{runner['architecture']}-{runner['runner_class']}-"
        f"{runner['toolchain']}"
    )
    if runner["key"] != expected_key:
        raise ValidationError(f"{label}: runner key is not derived from its fields")


def environment_with_cached_runner(runner: dict[str, str]) -> dict[str, str]:
    environment = os.environ.copy()
    for environment_name, runner_field in RUNNER_ENV_FIELDS.items():
        environment[environment_name] = runner[runner_field]
    environment[RUNNER_DIGEST_ENV] = digest_value(runner)
    return environment


def cached_runner_identity(policy: dict[str, Any]) -> dict[str, str]:
    names = [*RUNNER_ENV_FIELDS, RUNNER_DIGEST_ENV]
    present = {name: os.environ.get(name) for name in names}
    if any(value is None or value == "" for value in present.values()):
        raise ValidationError("cached runner identity is incomplete")
    runner = {
        runner_field: str(present[environment_name])
        for environment_name, runner_field in RUNNER_ENV_FIELDS.items()
    }
    validate_runner(runner, "cached runner", policy)
    expected_digest = digest_value(runner)
    if not secrets.compare_digest(str(present[RUNNER_DIGEST_ENV]), expected_digest):
        raise ValidationError("cached runner identity was mutated")
    return runner


def cached_runner_line(runner: dict[str, str]) -> str:
    values = [runner[field] for field in RUNNER_ENV_FIELDS.values()]
    values.append(digest_value(runner))
    if any("\t" in value or "\n" in value or "\r" in value for value in values):
        raise ValidationError("cached runner identity cannot be serialized")
    return "\t".join(values)


def require_harness_environment(policy_digest: str) -> tuple[Path, str]:
    if os.environ.get("STAX_PERF_HARNESS") != "1":
        raise ValidationError("scenario execution requires rust/perf-gate.sh")
    receipt_dir_value = os.environ.get("STAX_PERF_RECEIPT_DIR")
    if not receipt_dir_value:
        raise ValidationError("scenario execution requires a receipt directory")
    run_id = os.environ.get("STAX_PERF_RUN_ID", "")
    if re.fullmatch(r"[a-f0-9]{32}", run_id) is None:
        raise ValidationError("scenario execution requires a sanitized run identity")
    if os.environ.get("STAX_PERF_POLICY_DIGEST") != policy_digest:
        raise ValidationError("scenario execution requires the verified policy digest")
    return Path(receipt_dir_value), run_id


def verify_scenario_command(
    command: list[str], scenarios: list[str], policy: dict[str, Any]
) -> None:
    if not scenarios or len(set(scenarios)) != len(scenarios):
        raise ValidationError("scenario command needs distinct scenario identities")
    if any(scenario not in policy["scenarios"] for scenario in scenarios):
        raise ValidationError("scenario command names an unknown workload")
    if not command or command[0] != "cargo":
        raise ValidationError(
            "performance scenarios must use the exact cargo invocation"
        )
    descriptor = policy["scenarios"][scenarios[0]]["test"]
    exact_command = [
        "cargo",
        "test",
        "--release",
        "--locked",
        "-p",
        descriptor["package"],
        "--test",
        descriptor["integration_test"],
        descriptor["test_name"],
        "--",
        "--ignored",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]
    if command != exact_command:
        raise ValidationError(
            "performance scenario invocation differs from the exact plan"
        )
    mapped = {
        name
        for name, expected in policy["scenarios"].items()
        if expected["test"] == descriptor
    }
    if set(scenarios) != mapped:
        raise ValidationError(
            "scenario identities do not match the fixed test invocation"
        )


def atomic_write_json(destination: Path, value: dict[str, Any]) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.{os.getpid()}.tmp")
    try:
        temporary.write_text(
            json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        temporary.replace(destination)
    except OSError as error:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        raise ValidationError(f"unable to publish {destination.name}") from error


def validate_failure_receipt(
    receipt: dict[str, Any],
    scenario: str,
    policy: dict[str, Any],
    policy_digest: str,
    *,
    expected_run_id: str | None = None,
    expected_runner: dict[str, str] | None = None,
) -> None:
    if scenario not in SUITE_SENTINELS and scenario not in policy["scenarios"]:
        raise ValidationError(f"{scenario}: unknown failure scenario")
    validate_schema(receipt, load_json(SCHEMA_PATHS["failure"]), scenario)
    validate_privacy(receipt, scenario)
    if receipt["scenario"] != scenario:
        raise ValidationError(f"{scenario}: failure scenario identity changed")
    if receipt["policy_digest"] != policy_digest:
        raise ValidationError(f"{scenario}: failure policy identity changed")
    if expected_run_id is not None and receipt["run_id"] != expected_run_id:
        raise ValidationError(f"{scenario}: failure run identity changed")
    expected_fixture = (
        {"registered_scenarios": len(policy["scenarios"])}
        if scenario in SUITE_SENTINELS
        else policy["scenarios"][scenario]["fixture"]
    )
    if receipt["fixture"] != expected_fixture:
        raise ValidationError(f"{scenario}: failure fixture identity changed")
    expected_timeout = (
        policy["setup_timeout_seconds"]
        if scenario == SUITE_SETUP_SCENARIO
        else (
            policy["suite_timeout_seconds"]
            if scenario == SUITE_FINALIZATION_SCENARIO
            else policy["scenario_timeout_seconds"]
        )
    )
    if finite_number(
        receipt["timeout_seconds"], f"{scenario}.timeout_seconds"
    ) != float(expected_timeout):
        raise ValidationError(f"{scenario}: failure timeout class changed")
    validate_runner(receipt["runner"], f"{scenario}.runner", policy)
    if expected_runner is not None and receipt["runner"] != expected_runner:
        raise ValidationError(f"{scenario}: failure runner identity changed")

    kind = receipt["failure_kind"]
    if scenario == SUITE_FINALIZATION_SCENARIO:
        if kind not in {
            "evidence_validation_failure",
            "comparison_failure",
            "timeout",
            "interrupted",
        }:
            raise ValidationError("suite finalization: invalid terminal failure kind")
        if receipt["exit_code"] is not None:
            raise ValidationError("suite finalization: exit code must be null")
    elif kind == "comparison_failure":
        raise ValidationError(f"{scenario}: comparison failure must be suite-scoped")
    elif kind == "evidence_validation_failure":
        if scenario == SUITE_SETUP_SCENARIO:
            raise ValidationError("suite setup: evidence validation is not applicable")
        if receipt["exit_code"] != 0:
            raise ValidationError(
                f"{scenario}: evidence validation requires child exit zero"
            )
    elif kind in {"timeout", "interrupted"} and receipt["exit_code"] is not None:
        raise ValidationError(f"{scenario}: terminal signal failure has an exit code")
    elif kind == "command_failure" and receipt["exit_code"] == 0:
        raise ValidationError(f"{scenario}: command failure cannot have exit zero")


def write_failure_receipts(
    scenarios: list[str],
    kind: str,
    elapsed_seconds: float,
    timeout_seconds: float,
    exit_code: int | None,
    policy: dict[str, Any],
    policy_digest: str,
    receipt_dir: Path,
    run_id: str,
    runner: dict[str, str],
    *,
    replace_existing: bool = False,
) -> None:
    for scenario in scenarios:
        standard_path = receipt_dir / f"{scenario}.json"
        if scenario not in SUITE_SENTINELS and standard_path.exists():
            try:
                existing = load_json(standard_path)
                validate_receipt(
                    existing,
                    scenario,
                    policy["scenarios"][scenario],
                    policy,
                    policy_digest,
                    require_pass=False,
                    expected_run_id=run_id,
                    expected_runner=runner,
                )
                if existing["status"] == "fail":
                    continue
            except Exception:
                # This is the same untrusted child-output boundary as the
                # post-exit validator. A malformed foreign receipt must never
                # suppress trusted current-run failure evidence.
                pass
        failure = {
            "schema": "staxtrace.performance-failure",
            "run_id": run_id,
            "policy_digest": policy_digest,
            "scenario": scenario,
            "profile": "release",
            "runner": runner,
            "fixture": (
                {"registered_scenarios": len(policy["scenarios"])}
                if scenario in SUITE_SENTINELS
                else policy["scenarios"][scenario]["fixture"]
            ),
            "status": "fail",
            "failure_kind": kind,
            "elapsed_seconds": max(0.0, float(elapsed_seconds)),
            "timeout_seconds": max(float(timeout_seconds), 0.001),
            "exit_code": exit_code,
        }
        validate_failure_receipt(
            failure,
            scenario,
            policy,
            policy_digest,
            expected_run_id=run_id,
            expected_runner=runner,
        )
        destination = receipt_dir / f"{scenario}.failure.json"
        if destination.exists() and not replace_existing:
            try:
                existing_failure = load_json(destination)
                validate_failure_receipt(
                    existing_failure,
                    scenario,
                    policy,
                    policy_digest,
                    expected_run_id=run_id,
                    expected_runner=runner,
                )
                continue
            except Exception:
                # A foreign or malformed envelope is not terminal evidence for
                # this run and cannot block the trusted atomic replacement.
                pass
        atomic_write_json(destination, failure)
        print(f"performance failure receipt: {destination.name}", flush=True)


def process_group_exists(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # Some kernels briefly return EPERM while a just-signalled process
        # group is disappearing. Confirm membership read-only before deciding
        # that cleanup is incomplete.
        try:
            groups = subprocess.run(
                ["ps", "-axo", "pgid="],
                check=True,
                capture_output=True,
                text=True,
                timeout=2,
            ).stdout.splitlines()
        except (OSError, subprocess.SubprocessError) as error:
            raise ValidationError("unable to confirm process-group cleanup") from error
        return any(value.strip() == str(process_group) for value in groups)
    return True


def terminate_process_group(process: subprocess.Popen[Any]) -> None:
    process_group = process.pid
    try:
        os.killpg(process_group, signal.SIGTERM)
    except ProcessLookupError:
        process.wait()
        return

    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if not process_group_exists(process_group):
            process.wait()
            return
        if process.poll() is None:
            try:
                process.wait(timeout=0.05)
            except subprocess.TimeoutExpired:
                pass
        else:
            time.sleep(0.05)
    try:
        os.killpg(process_group, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()
    deadline = time.monotonic() + 1.0
    while process_group_exists(process_group) and time.monotonic() < deadline:
        time.sleep(0.01)
    if process_group_exists(process_group):
        raise ValidationError(
            "unable to terminate the complete performance process group"
        )


def validated_execution_timeout(
    args: argparse.Namespace,
    policy: dict[str, Any],
    scenarios: list[str],
    *,
    now: float,
) -> tuple[float, float]:
    _, _, remaining = validated_cached_suite_clock(
        args.suite_start,
        args.suite_timeout,
        policy,
        now=now,
        label="executor",
    )
    class_name = "scenario" if scenarios else "setup"
    expected_timeout = policy[
        "scenario_timeout_seconds" if scenarios else "setup_timeout_seconds"
    ]
    class_timeout = finite_number(args.timeout, f"executor.{class_name}_timeout")
    if class_timeout != float(expected_timeout):
        raise ValidationError(f"{class_name} timeout differs from fixed policy")
    return class_timeout, min(class_timeout, remaining)


def validated_suite_clock(
    suite_start_value: Any,
    suite_timeout_value: Any,
    policy: dict[str, Any],
    *,
    now: float,
    label: str,
) -> tuple[float, float, float]:
    suite_timeout = finite_number(suite_timeout_value, f"{label}.suite_timeout")
    if suite_timeout != float(policy["suite_timeout_seconds"]):
        raise ValidationError("suite timeout differs from fixed policy")
    suite_start = finite_number(suite_start_value, f"{label}.suite_start")
    if suite_start < 0:
        raise ValidationError("suite start must be nonnegative")
    if suite_start > now:
        raise ValidationError("suite start must not be in the future")
    remaining = suite_timeout - (now - suite_start)
    return suite_start, suite_timeout, remaining


def validated_cached_suite_clock(
    suite_start_value: Any,
    suite_timeout_value: Any,
    policy: dict[str, Any],
    *,
    now: float,
    label: str,
) -> tuple[float, float, float]:
    cached_start_value = os.environ.get(SUITE_START_ENV)
    cached_timeout_value = os.environ.get(SUITE_TIMEOUT_ENV)
    if cached_start_value is None or cached_timeout_value is None:
        raise ValidationError("cached suite clock is incomplete")
    supplied_start = finite_number(suite_start_value, f"{label}.suite_start")
    supplied_timeout = finite_number(suite_timeout_value, f"{label}.suite_timeout")
    cached_start = finite_environment_number(cached_start_value, "cached suite start")
    cached_timeout = finite_environment_number(
        cached_timeout_value, "cached suite timeout"
    )
    if supplied_start != cached_start or supplied_timeout != cached_timeout:
        raise ValidationError("cached suite clock was mutated")
    return validated_suite_clock(
        cached_start,
        cached_timeout,
        policy,
        now=now,
        label=label,
    )


def execute(args: argparse.Namespace) -> int:
    policy, policy_digest = load_policy()
    command = (
        args.command[1:] if args.command and args.command[0] == "--" else args.command
    )
    if not command:
        raise ValidationError("exec requires a command after --")
    scenarios = list(args.scenario or [])
    receipt_dir, run_id = require_harness_environment(policy_digest)
    if scenarios:
        verify_scenario_command(command, scenarios, policy)
    evidence_scenarios = scenarios or [SUITE_SETUP_SCENARIO]
    cached_runner = cached_runner_identity(policy)
    class_timeout, timeout = validated_execution_timeout(
        args, policy, scenarios, now=time.monotonic()
    )
    label = ",".join(scenarios) if scenarios else "suite setup"
    print(f"performance command bound: {label} ({max(timeout, 0.0):.0f}s)", flush=True)
    if timeout <= 0:
        write_failure_receipts(
            evidence_scenarios,
            "timeout",
            0.0,
            class_timeout,
            None,
            policy,
            policy_digest,
            receipt_dir,
            run_id,
            cached_runner,
        )
        raise ValidationError("performance suite timeout reached before command start")

    started = time.monotonic()
    try:
        process = subprocess.Popen(
            command,
            env=environment_with_cached_runner(cached_runner),
            start_new_session=True,
        )
    except OSError as error:
        write_failure_receipts(
            evidence_scenarios,
            "command_failure",
            time.monotonic() - started,
            class_timeout,
            None,
            policy,
            policy_digest,
            receipt_dir,
            run_id,
            cached_runner,
        )
        raise ValidationError("unable to start bounded performance command") from error

    kind: str | None = None
    try:
        return_code = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        kind = "timeout"
        terminate_process_group(process)
        return_code = None
    except KeyboardInterrupt:
        kind = "interrupted"
        terminate_process_group(process)
        return_code = None
    elapsed = time.monotonic() - started

    if kind is not None:
        write_failure_receipts(
            evidence_scenarios,
            kind,
            elapsed,
            class_timeout,
            None,
            policy,
            policy_digest,
            receipt_dir,
            run_id,
            cached_runner,
        )
        raise ValidationError(f"performance command ended with {kind}")

    if return_code != 0:
        terminate_process_group(process)
        write_failure_receipts(
            evidence_scenarios,
            "command_failure",
            elapsed,
            class_timeout,
            int(return_code),
            policy,
            policy_digest,
            receipt_dir,
            run_id,
            cached_runner,
        )
        return int(return_code)

    invalid_scenarios: list[str] = []
    terminal_scenarios: list[str] = []
    for scenario in scenarios:
        try:
            receipt = load_json(receipt_dir / f"{scenario}.json")
            validate_receipt(
                receipt,
                scenario,
                policy["scenarios"][scenario],
                policy,
                policy_digest,
                require_pass=False,
                expected_run_id=run_id,
                expected_runner=cached_runner,
            )
        except Exception:
            # Scenario evidence is an untrusted child-output boundary. Never
            # reflect the rejected value, decoder error, path, or argv into a
            # terminal receipt, and keep checking every declared scenario.
            invalid_scenarios.append(scenario)
            continue
        if receipt["status"] == "fail":
            terminal_scenarios.append(scenario)

    if invalid_scenarios:
        write_failure_receipts(
            invalid_scenarios,
            "evidence_validation_failure",
            elapsed,
            class_timeout,
            0,
            policy,
            policy_digest,
            receipt_dir,
            run_id,
            cached_runner,
        )
    if invalid_scenarios or terminal_scenarios:
        return 2
    return 0


def validate_receipt(
    receipt: dict[str, Any],
    scenario: str,
    expected: dict[str, Any],
    policy: dict[str, Any],
    policy_digest: str,
    *,
    require_pass: bool,
    expected_run_id: str | None = None,
    expected_runner: dict[str, str] | None = None,
) -> None:
    validate_schema(receipt, load_json(SCHEMA_PATHS["receipt"]), scenario)
    validate_privacy(receipt, scenario)
    if receipt["scenario"] != scenario:
        raise ValidationError(f"{scenario}: scenario/file mismatch")
    if receipt["policy_digest"] != policy_digest:
        raise ValidationError(f"{scenario}: policy identity mismatch")
    if expected_run_id is not None and receipt["run_id"] != expected_run_id:
        raise ValidationError(f"{scenario}: run identity mismatch")
    if receipt["fixture"] != expected["fixture"]:
        raise ValidationError(f"{scenario}: immutable fixture changed")

    samples = receipt["samples"]
    expected_samples = expected["samples"]
    if expected_samples is not None and samples["count"] != expected_samples:
        raise ValidationError(f"{scenario}: immutable sample count changed")
    if expected_samples is None and samples["count"] <= 0:
        raise ValidationError(f"{scenario}: concurrent scenario has no reader samples")
    p50 = finite_number(samples["p50_ms"], f"{scenario}.p50_ms")
    p95 = finite_number(samples["p95_ms"], f"{scenario}.p95_ms")
    p99 = finite_number(samples["p99_ms"], f"{scenario}.p99_ms")
    if not 0 <= p50 <= p95 <= p99:
        raise ValidationError(f"{scenario}: percentile ordering is invalid")

    if receipt["timeout_seconds"] != policy["scenario_timeout_seconds"]:
        raise ValidationError(f"{scenario}: scenario timeout changed")
    setup = finite_number(receipt["setup_wall_seconds"], f"{scenario}.setup_wall")
    copy = finite_number(receipt["copy_wall_seconds"], f"{scenario}.copy_wall")
    timed = finite_number(receipt["timed_wall_seconds"], f"{scenario}.timed_wall")
    scenario_wall = finite_number(
        receipt["scenario_wall_seconds"], f"{scenario}.scenario_wall"
    )
    if timed <= 0 or scenario_wall <= 0:
        raise ValidationError(f"{scenario}: wall durations must be positive")
    if setup + copy + timed > scenario_wall + 1e-6:
        raise ValidationError(f"{scenario}: separated durations exceed scenario wall")
    timing_passed = scenario_wall <= policy["scenario_timeout_seconds"]

    resources = receipt["resources"]
    expected_resources = expected["resources"]
    limits = policy["resource_limits"]
    observation = policy["resource_observation"]
    if resources["measurement_mode"] != expected_resources["mode"]:
        raise ValidationError(f"{scenario}: resource measurement mode changed")
    if resources["peak_rss_limit_bytes"] != limits["peak_rss_bytes"]:
        raise ValidationError(f"{scenario}: RSS ceiling changed")
    if resources["open_file_limit"] != limits["open_files"]:
        raise ValidationError(f"{scenario}: descriptor ceiling changed")
    if (
        resources["observer_wall_seconds_limit"]
        != observation["observer_wall_seconds_limit"]
    ):
        raise ValidationError(f"{scenario}: observer ceiling changed")
    observer = finite_number(
        resources["observer_wall_seconds"], f"{scenario}.observer_wall"
    )
    observer_fraction = finite_number(
        resources["observer_wall_fraction"], f"{scenario}.observer_fraction"
    )
    expected_fraction = observer / scenario_wall
    if not math.isclose(
        observer_fraction, expected_fraction, rel_tol=1e-9, abs_tol=1e-9
    ):
        raise ValidationError(f"{scenario}: observer fraction is inconsistent")
    resource_passed = (
        0 < resources["peak_rss_bytes"] <= limits["peak_rss_bytes"]
        and 0 < resources["peak_open_files"] <= limits["open_files"]
        and resources["rss_sample_count"] >= expected_resources["minimum_rss_samples"]
        and resources["open_file_sample_count"]
        >= expected_resources["minimum_open_file_samples"]
        and resources["child_process_sample_count"]
        == expected_resources["child_process_samples"]
        and observer <= observation["observer_wall_seconds_limit"]
    )
    expects_child_resources = expected_resources["child_process_samples"] > 0
    child_resources_observed = (
        resources["child_peak_rss_bytes"] > 0 and resources["child_peak_open_files"] > 0
    )
    if child_resources_observed != expects_child_resources:
        raise ValidationError(f"{scenario}: child resource evidence shape changed")
    if expects_child_resources and (
        resources["child_peak_rss_bytes"] > resources["peak_rss_bytes"]
        or resources["child_peak_open_files"] > resources["peak_open_files"]
    ):
        raise ValidationError(
            f"{scenario}: child resource peaks exceed process-tree peaks"
        )
    if not expects_child_resources and (
        resources["child_peak_rss_bytes"] != 0
        or resources["child_peak_open_files"] != 0
    ):
        raise ValidationError(f"{scenario}: unexpected child resource evidence")

    guard = receipt["guard"]
    expected_guard = expected["guard"]
    for key in ("metric", "comparison", "limit"):
        if canonical_bytes(guard[key]) != canonical_bytes(expected_guard[key]):
            raise ValidationError(f"{scenario}: immutable guard {key} changed")
    expected_errors = expected_guard.get("errors")
    if guard["errors"] != expected_errors:
        raise ValidationError(f"{scenario}: operation-error shape changed")
    actual = finite_number(guard["actual"], f"{scenario}.guard.actual")
    limit = finite_number(guard["limit"], f"{scenario}.guard.limit")
    comparison_passed = (
        actual <= limit if guard["comparison"] == "at_most" else actual >= limit
    )
    errors_passed = guard["errors"] in (None, 0)
    guard_passed = comparison_passed and errors_passed
    if guard["passed"] is not guard_passed:
        raise ValidationError(f"{scenario}: guard result is inconsistent")
    if guard["metric"] in {"p99_ms", "reader_p99_ms"} and not math.isclose(
        actual, p99, rel_tol=1e-12, abs_tol=1e-12
    ):
        raise ValidationError(f"{scenario}: latency guard does not use p99")
    throughput = receipt["throughput_per_second"]
    if guard["metric"] == "throughput_per_second":
        measured_throughput = finite_number(
            throughput, f"{scenario}.throughput_per_second"
        )
        if not math.isclose(actual, measured_throughput, rel_tol=1e-12, abs_tol=1e-12):
            raise ValidationError(f"{scenario}: throughput guard value is inconsistent")
    elif throughput is not None:
        raise ValidationError(f"{scenario}: unexpected throughput value")

    if observer > scenario_wall + 1e-6 or observer_fraction > 1.0 + 1e-9:
        raise ValidationError(f"{scenario}: observer duration exceeds scenario wall")

    expected_status = (
        "pass" if guard_passed and resource_passed and timing_passed else "fail"
    )
    if receipt["status"] != expected_status:
        raise ValidationError(f"{scenario}: receipt status is inconsistent")
    if require_pass and receipt["status"] != "pass":
        raise ValidationError(f"{scenario}: receipt does not pass")
    validate_runner(receipt["runner"], f"{scenario}.runner", policy)
    if expected_runner is not None and receipt["runner"] != expected_runner:
        raise ValidationError(f"{scenario}: runner identity mismatch")


def derive_baseline_scenarios(
    suite_receipts: list[list[dict[str, Any]]], policy: dict[str, Any]
) -> dict[str, dict[str, float | int | None]]:
    scenarios: dict[str, dict[str, float | int | None]] = {}
    for name in policy["scenarios"]:
        rows = [
            next(receipt for receipt in receipts if receipt["scenario"] == name)
            for receipts in suite_receipts
        ]
        p99_values = [
            finite_number(row["samples"]["p99_ms"], f"baseline.{name}.p99_ms")
            for row in rows
        ]
        rss_values = [
            finite_number(
                row["resources"]["peak_rss_bytes"], f"baseline.{name}.peak_rss_bytes"
            )
            for row in rows
        ]
        open_file_values = [
            finite_number(
                row["resources"]["peak_open_files"],
                f"baseline.{name}.peak_open_files",
            )
            for row in rows
        ]
        expects_throughput = (
            policy["scenarios"][name]["guard"]["metric"] == "throughput_per_second"
        )
        throughputs = [row["throughput_per_second"] for row in rows]
        if expects_throughput:
            throughput_median: float | None = statistics.median(
                finite_number(value, f"baseline.{name}.throughput_per_second")
                for value in throughputs
            )
        else:
            if any(value is not None for value in throughputs):
                raise ValidationError(
                    f"baseline.{name}: throughput shape differs from current policy"
                )
            throughput_median = None
        scenarios[name] = {
            "p99_ms": statistics.median(p99_values),
            "peak_rss_bytes": statistics.median(rss_values),
            "peak_open_files": statistics.median(open_file_values),
            "throughput_per_second": throughput_median,
        }
    return scenarios


def validate_baseline(
    baseline: dict[str, Any], policy: dict[str, Any], policy_digest: str
) -> None:
    validate_schema(baseline, load_json(SCHEMA_PATHS["baseline"]), "baseline")
    validate_privacy(baseline, "baseline")
    if baseline["policy_digest"] != policy_digest:
        raise ValidationError("baseline: policy identity mismatch")
    required = policy["baseline_policy"]["clean_run_count"]
    if baseline["clean_run_count"] != required:
        raise ValidationError("baseline: clean-run count changed")
    source_suites = baseline["source_suites"]
    if len(source_suites) != required:
        raise ValidationError("baseline: source-suite count changed")
    suite_receipts = [
        validate_suite_receipt(suite, policy, policy_digest) for suite in source_suites
    ]
    run_ids = [suite["run_id"] for suite in source_suites]
    suite_digests = [digest_value(suite) for suite in source_suites]
    if len(set(run_ids)) != required:
        raise ValidationError("baseline: run identities are not distinct")
    if len(set(suite_digests)) != required:
        raise ValidationError("baseline: suite receipts are not distinct")
    if canonical_bytes(baseline["run_ids"]) != canonical_bytes(run_ids):
        raise ValidationError(
            "baseline: stored run identities differ from source suites"
        )
    if canonical_bytes(baseline["suite_digests"]) != canonical_bytes(suite_digests):
        raise ValidationError(
            "baseline: stored suite digests differ from source suites"
        )
    runner = source_suites[0]["runner"]
    if any(suite["runner"] != runner for suite in source_suites):
        raise ValidationError(
            "baseline: source suites do not share one runner identity"
        )
    if baseline["runner"] != runner:
        raise ValidationError("baseline: stored runner differs from source suites")
    derived = derive_baseline_scenarios(suite_receipts, policy)
    if canonical_bytes(baseline["scenarios"]) != canonical_bytes(derived):
        raise ValidationError("baseline: stored medians differ from source suites")
    validate_runner(baseline["runner"], "baseline.runner", policy)


def compare_baseline(
    receipts: list[dict[str, Any]],
    baseline_path: Path,
    policy: dict[str, Any],
    policy_digest: str,
) -> dict[str, Any]:
    baseline = load_json(baseline_path)
    validate_baseline(baseline, policy, policy_digest)
    if baseline["runner"] != receipts[0]["runner"]:
        raise ValidationError("baseline: runner identity does not match this run")
    regression = policy["baseline_policy"]
    comparisons: list[dict[str, Any]] = []
    for receipt in receipts:
        scenario = receipt["scenario"]
        prior = baseline["scenarios"][scenario]
        checks = {
            "p99_ms": receipt["samples"]["p99_ms"]
            <= prior["p99_ms"] * (1.0 + regression["latency_regression_fraction"]),
            "peak_rss_bytes": receipt["resources"]["peak_rss_bytes"]
            <= prior["peak_rss_bytes"] * (1.0 + regression["rss_regression_fraction"]),
            "peak_open_files": receipt["resources"]["peak_open_files"]
            <= prior["peak_open_files"]
            * (1.0 + regression["open_file_regression_fraction"]),
        }
        if receipt["throughput_per_second"] is not None:
            checks["throughput_per_second"] = receipt["throughput_per_second"] >= prior[
                "throughput_per_second"
            ] * (1.0 - regression["throughput_regression_fraction"])
        failed = sorted(name for name, passed in checks.items() if not passed)
        if failed:
            raise ComparisonFailure(f"{scenario}: same-runner regression in {failed}")
        comparisons.append({"scenario": scenario, "checks": checks, "passed": True})
    return {
        "mode": "same_runner_baseline",
        "baseline_file": baseline_path.name,
        "comparisons": comparisons,
    }


def validate_suite_receipt(
    suite: dict[str, Any], policy: dict[str, Any], policy_digest: str
) -> list[dict[str, Any]]:
    validate_schema(suite, load_json(SCHEMA_PATHS["suite"]), "suite")
    validate_privacy(suite, "suite")
    if suite["policy_digest"] != policy_digest:
        raise ValidationError("suite: policy identity mismatch")
    expected_names = set(policy["scenarios"])
    if suite["scenario_count"] != len(expected_names):
        raise ValidationError("suite: scenario count changed")
    receipts = suite["scenarios"]
    by_name: dict[str, dict[str, Any]] = {}
    for receipt in receipts:
        if not isinstance(receipt, dict) or not isinstance(
            receipt.get("scenario"), str
        ):
            raise ValidationError("suite: malformed embedded scenario")
        name = receipt["scenario"]
        if name in by_name or name not in expected_names:
            raise ValidationError("suite: duplicate or unknown scenario")
        validate_receipt(
            receipt,
            name,
            policy["scenarios"][name],
            policy,
            policy_digest,
            require_pass=True,
            expected_run_id=suite["run_id"],
        )
        if receipt["runner"] != suite["runner"]:
            raise ValidationError("suite: embedded runner identity differs")
        by_name[name] = receipt
    if set(by_name) != expected_names:
        raise ValidationError("suite: scenario set is incomplete")
    if set(suite["scenario_digests"]) != expected_names:
        raise ValidationError("suite: scenario digest set is incomplete")
    for name, receipt in by_name.items():
        if suite["scenario_digests"][name] != digest_value(receipt):
            raise ValidationError("suite: embedded scenario digest mismatch")
    validate_runner(suite["runner"], "suite.runner", policy)
    comparison = suite["baseline_comparison"]
    if comparison["mode"] == "absolute_guards":
        if comparison["baseline_file"] is not None or comparison["comparisons"]:
            raise ValidationError("suite: malformed absolute-guard comparison")
    else:
        if comparison["baseline_file"] is None:
            raise ValidationError("suite: baseline filename is missing")
        comparison_names = [row["scenario"] for row in comparison["comparisons"]]
        if set(comparison_names) != expected_names or len(comparison_names) != len(
            expected_names
        ):
            raise ValidationError("suite: baseline comparison set is incomplete")
        for row in comparison["comparisons"]:
            expected_checks = {"p99_ms", "peak_rss_bytes", "peak_open_files"}
            if policy["scenarios"][row["scenario"]]["guard"]["metric"] == (
                "throughput_per_second"
            ):
                expected_checks.add("throughput_per_second")
            if set(row["checks"]) != expected_checks or not all(row["checks"].values()):
                raise ValidationError(
                    "suite: baseline comparison checks are inconsistent"
                )
    return [by_name[name] for name in policy["scenarios"]]


def assemble_suite(
    receipt_dir: Path,
    baseline_path: Path | None,
    policy: dict[str, Any],
    policy_digest: str,
    run_id: str,
    runner: dict[str, str],
    *,
    allowed_auxiliary_names: set[str] | None = None,
) -> dict[str, Any]:
    expected_names = set(policy["scenarios"])
    all_json = list(receipt_dir.glob("*.json"))
    failures = [path for path in all_json if path.name.endswith(".failure.json")]
    if failures:
        for path in failures:
            scenario = path.name.removesuffix(".failure.json")
            if scenario not in expected_names | SUITE_SENTINELS:
                raise ValidationError("failure receipt names an unknown scenario")
            validate_failure_receipt(
                load_json(path),
                scenario,
                policy,
                policy_digest,
                expected_run_id=run_id,
                expected_runner=runner,
            )
        raise TerminalScenarioEvidence("suite contains terminal scenario evidence")
    allowed_names = {f"{name}.json" for name in expected_names} | {
        SUITE_RECEIPT_NAME
    }
    allowed_names.update(allowed_auxiliary_names or set())
    extra = sorted(path.name for path in all_json if path.name not in allowed_names)
    if extra:
        raise ValidationError(f"receipt set has unexpected files {extra}")
    missing = sorted(
        name for name in expected_names if not (receipt_dir / f"{name}.json").is_file()
    )
    if missing:
        raise ValidationError(f"receipt set is missing scenarios {missing}")

    receipts: list[dict[str, Any]] = []
    has_terminal_receipt = False
    for scenario in policy["scenarios"]:
        receipt = load_json(receipt_dir / f"{scenario}.json")
        validate_receipt(
            receipt,
            scenario,
            policy["scenarios"][scenario],
            policy,
            policy_digest,
            require_pass=False,
            expected_run_id=run_id,
            expected_runner=runner,
        )
        has_terminal_receipt |= receipt["status"] == "fail"
        receipts.append(receipt)
    if has_terminal_receipt:
        raise TerminalScenarioEvidence("suite contains terminal scenario evidence")

    comparison = (
        compare_baseline(receipts, baseline_path, policy, policy_digest)
        if baseline_path is not None
        else {"mode": "absolute_guards", "baseline_file": None, "comparisons": []}
    )
    suite = {
        "schema": "staxtrace.performance-suite-receipt",
        "run_id": run_id,
        "policy_digest": policy_digest,
        "runner": runner,
        "scenario_count": len(receipts),
        "scenario_digests": {
            receipt["scenario"]: digest_value(receipt) for receipt in receipts
        },
        "scenarios": receipts,
        "baseline_comparison": comparison,
        "status": "pass",
    }
    validate_suite_receipt(suite, policy, policy_digest)
    return suite


def finalize_suite(
    receipt_dir: Path,
    baseline_path: Path | None,
    policy: dict[str, Any],
    policy_digest: str,
    run_id: str,
    runner: dict[str, str],
    *,
    destination_name: str = SUITE_RECEIPT_NAME,
) -> int:
    staged_name = staged_suite_receipt_name(run_id)
    if destination_name not in {SUITE_RECEIPT_NAME, staged_name}:
        raise ValidationError("suite destination is not allowed")
    suite = assemble_suite(
        receipt_dir,
        baseline_path,
        policy,
        policy_digest,
        run_id,
        runner,
    )
    suite_path = receipt_dir / SUITE_RECEIPT_NAME
    if suite_path.exists():
        existing_suite = load_json(suite_path)
        validate_suite_receipt(existing_suite, policy, policy_digest)
        if existing_suite["run_id"] != run_id or existing_suite["runner"] != runner:
            raise ValidationError("existing suite receipt has a foreign identity")
        if canonical_bytes(existing_suite) != canonical_bytes(suite):
            raise ValidationError("existing suite receipt differs from final evidence")
    else:
        destination = receipt_dir / destination_name
        if destination.exists():
            raise ValidationError("suite staging destination already exists")
        atomic_write_json(destination, suite)
    return 0


def validate_final_suite(
    receipt_dir: Path,
    baseline_path: Path | None,
    policy: dict[str, Any],
    policy_digest: str,
    run_id: str,
    runner: dict[str, str],
    *,
    suite_name: str = SUITE_RECEIPT_NAME,
) -> None:
    staged_name = staged_suite_receipt_name(run_id)
    if suite_name not in {SUITE_RECEIPT_NAME, staged_name}:
        raise ValidationError("suite verification source is not allowed")
    suite = load_json(receipt_dir / suite_name)
    validate_suite_receipt(suite, policy, policy_digest)
    if suite["run_id"] != run_id or suite["runner"] != runner:
        raise ValidationError("final suite receipt has a foreign identity")
    expected_suite = assemble_suite(
        receipt_dir,
        baseline_path,
        policy,
        policy_digest,
        run_id,
        runner,
        allowed_auxiliary_names=(
            {staged_name}
            if suite_name == staged_name
            else set()
        ),
    )
    if canonical_bytes(suite) != canonical_bytes(expected_suite):
        raise ValidationError("final suite receipt differs from source evidence")


def is_valid_current_suite(
    suite_path: Path,
    policy: dict[str, Any],
    policy_digest: str,
    run_id: str,
    runner: dict[str, str],
) -> bool:
    try:
        if suite_path.is_symlink() or not suite_path.is_file():
            return False
        suite = load_json(suite_path)
        validate_suite_receipt(suite, policy, policy_digest)
        return suite["run_id"] == run_id and suite["runner"] == runner
    except Exception:
        return False


def finalize_worker(args: argparse.Namespace) -> int:
    try:
        policy, policy_digest = load_policy()
        trusted_receipt_dir, run_id = require_harness_environment(policy_digest)
        cached_runner = cached_runner_identity(policy)
        _, _, remaining = validated_cached_suite_clock(
            args.suite_start,
            args.suite_timeout,
            policy,
            now=time.monotonic(),
            label="finalizer worker",
        )
        if remaining <= 0:
            return FINALIZER_EVIDENCE_EXIT
        if Path(args.receipt_dir).resolve() != trusted_receipt_dir.resolve():
            raise ValidationError("finalizer receipt directory differs from harness state")
        return finalize_suite(
            trusted_receipt_dir,
            Path(args.baseline) if args.baseline else None,
            policy,
            policy_digest,
            run_id,
            cached_runner,
            destination_name=staged_suite_receipt_name(run_id),
        )
    except TerminalScenarioEvidence:
        return FINALIZER_TERMINAL_EXIT
    except ComparisonFailure:
        return FINALIZER_COMPARISON_EXIT
    except Exception:
        # This private worker consumes untrusted scenario/baseline evidence.
        # Classification is a closed exit code; rejected data and exception
        # text never cross into the trusted parent failure envelope.
        return FINALIZER_EVIDENCE_EXIT


def verify_final_suite_worker(args: argparse.Namespace) -> int:
    try:
        policy, policy_digest = load_policy()
        trusted_receipt_dir, run_id = require_harness_environment(policy_digest)
        cached_runner = cached_runner_identity(policy)
        _, _, remaining = validated_cached_suite_clock(
            args.suite_start,
            args.suite_timeout,
            policy,
            now=time.monotonic(),
            label="final suite verifier",
        )
        if remaining <= 0:
            return FINALIZER_EVIDENCE_EXIT
        if Path(args.receipt_dir).resolve() != trusted_receipt_dir.resolve():
            raise ValidationError("verifier receipt directory differs from harness state")
        validate_final_suite(
            trusted_receipt_dir,
            Path(args.baseline) if args.baseline else None,
            policy,
            policy_digest,
            run_id,
            cached_runner,
            suite_name=args.suite_name,
        )
        return 0
    except Exception:
        return FINALIZER_EVIDENCE_EXIT


def finalizer_child_command(
    verb: str,
    receipt_dir: str,
    suite_start: float,
    suite_timeout: float,
    baseline: str | None = None,
    suite_name: str | None = None,
) -> list[str]:
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        verb,
        "--receipt-dir",
        receipt_dir,
        "--suite-start",
        repr(suite_start),
        "--suite-timeout",
        repr(suite_timeout),
    ]
    if baseline is not None:
        command.extend(["--baseline", baseline])
    if suite_name is not None:
        command.extend(["--suite-name", suite_name])
    return command


def run_finalizer_child(command: list[str], timeout: float) -> int | None:
    process = subprocess.Popen(
        command,
        env=os.environ.copy(),
        start_new_session=True,
    )
    try:
        return_code = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        terminate_process_group(process)
        return None
    except KeyboardInterrupt:
        terminate_process_group(process)
        raise
    if return_code != 0:
        terminate_process_group(process)
    return int(return_code)


def safe_suite_elapsed(suite_start_value: Any, *, now: float | None = None) -> float:
    current = time.monotonic() if now is None else now
    try:
        suite_start = (
            finite_environment_number(suite_start_value, "cached suite start")
            if isinstance(suite_start_value, str)
            else finite_number(suite_start_value, "cached suite start")
        )
    except ValidationError:
        return 0.0
    if suite_start < 0 or suite_start > current:
        return 0.0
    return current - suite_start


def write_finalization_failure(
    kind: str,
    suite_start: float,
    suite_timeout: float,
    policy: dict[str, Any],
    policy_digest: str,
    receipt_dir: Path,
    run_id: str,
    runner: dict[str, str],
) -> None:
    write_failure_receipts(
        [SUITE_FINALIZATION_SCENARIO],
        kind,
        safe_suite_elapsed(suite_start),
        suite_timeout,
        None,
        policy,
        policy_digest,
        receipt_dir,
        run_id,
        runner,
    )


def discard_staged_suite(receipt_dir: Path, run_id: str) -> None:
    try:
        (receipt_dir / staged_suite_receipt_name(run_id)).unlink(missing_ok=True)
    except OSError:
        # A staging cleanup problem must not obscure the typed terminal
        # evidence. The run-bound hidden candidate is never accepted as a
        # suite artifact.
        pass


def discard_all_staged_suites(receipt_dir: Path) -> None:
    for path in receipt_dir.glob(".suite-receipt.*.pending"):
        if (
            re.fullmatch(r"\.suite-receipt\.[a-f0-9]{32}\.pending", path.name)
            is None
        ):
            continue
        try:
            if path.is_symlink() or path.is_file():
                path.unlink(missing_ok=True)
        except OSError:
            pass


def validate(args: argparse.Namespace) -> int:
    policy, policy_digest = load_policy()
    trusted_receipt_dir, run_id = require_harness_environment(policy_digest)
    cached_runner = cached_runner_identity(policy)
    suite_path = trusted_receipt_dir / SUITE_RECEIPT_NAME
    staged_name = staged_suite_receipt_name(run_id)
    staged_suite_path = trusted_receipt_dir / staged_name
    suite_existed_before = suite_path.exists()
    accepted_suite_before = is_valid_current_suite(
        suite_path,
        policy,
        policy_digest,
        run_id,
        cached_runner,
    )
    failure_start: Any = os.environ.get(SUITE_START_ENV, 0.0)
    failure_timeout = float(policy["suite_timeout_seconds"])

    def terminal(kind: str) -> int:
        discard_all_staged_suites(trusted_receipt_dir)
        if not accepted_suite_before:
            write_finalization_failure(
                kind,
                failure_start,
                failure_timeout,
                policy,
                policy_digest,
                trusted_receipt_dir,
                run_id,
                cached_runner,
            )
        return 2

    try:
        suite_start, suite_timeout, remaining = validated_cached_suite_clock(
            args.suite_start,
            args.suite_timeout,
            policy,
            now=time.monotonic(),
            label="finalizer",
        )
    except ValidationError:
        return terminal("evidence_validation_failure")
    failure_start = suite_start
    failure_timeout = suite_timeout
    stale_staging = list(trusted_receipt_dir.glob(".suite-receipt.*.pending"))
    if stale_staging:
        return terminal("evidence_validation_failure")
    if remaining <= 0:
        return terminal("timeout")

    try:
        return_code = run_finalizer_child(
            finalizer_child_command(
                "_finalize",
                args.receipt_dir,
                suite_start,
                suite_timeout,
                args.baseline,
            ),
            remaining,
        )
    except (OSError, ValidationError, KeyboardInterrupt) as error:
        kind = "interrupted" if isinstance(error, KeyboardInterrupt) else (
            "evidence_validation_failure"
        )
        return terminal(kind)

    try:
        _, _, remaining = validated_cached_suite_clock(
            suite_start,
            suite_timeout,
            policy,
            now=time.monotonic(),
            label="finalizer",
        )
    except ValidationError:
        return terminal("evidence_validation_failure")
    if return_code is None or remaining <= 0:
        return terminal("timeout")
    if return_code == FINALIZER_TERMINAL_EXIT:
        discard_all_staged_suites(trusted_receipt_dir)
        return 2
    if return_code == FINALIZER_COMPARISON_EXIT:
        return terminal("comparison_failure")
    if return_code != 0:
        return terminal("evidence_validation_failure")

    suite_name = (
        SUITE_RECEIPT_NAME
        if suite_existed_before
        else staged_name
    )
    try:
        verify_code = run_finalizer_child(
            finalizer_child_command(
                "_verify-final",
                str(trusted_receipt_dir),
                suite_start,
                suite_timeout,
                args.baseline,
                suite_name,
            ),
            remaining,
        )
    except (OSError, ValidationError, KeyboardInterrupt) as error:
        kind = "interrupted" if isinstance(error, KeyboardInterrupt) else (
            "evidence_validation_failure"
        )
        return terminal(kind)
    try:
        _, _, remaining = validated_cached_suite_clock(
            suite_start,
            suite_timeout,
            policy,
            now=time.monotonic(),
            label="finalizer",
        )
    except ValidationError:
        return terminal("evidence_validation_failure")
    if verify_code is None or remaining <= 0:
        return terminal("timeout")
    if verify_code != 0:
        return terminal("evidence_validation_failure")
    if suite_existed_before:
        if not accepted_suite_before or staged_suite_path.exists():
            return terminal("evidence_validation_failure")
        try:
            _, _, remaining = validated_cached_suite_clock(
                suite_start,
                suite_timeout,
                policy,
                now=time.monotonic(),
                label="finalizer commit",
            )
        except ValidationError:
            return terminal("evidence_validation_failure")
        if remaining <= 0:
            return terminal("timeout")
        print(
            f"validated {len(policy['scenarios'])} performance scenarios; "
            "receipt=suite-receipt.json"
        )
        return 0
    try:
        if (
            suite_path.exists()
            or staged_suite_path.is_symlink()
            or not staged_suite_path.is_file()
        ):
            raise ValidationError("verified suite staging state changed")
        _, _, remaining = validated_cached_suite_clock(
            suite_start,
            suite_timeout,
            policy,
            now=time.monotonic(),
            label="finalizer commit",
        )
        if remaining <= 0:
            return terminal("timeout")
        staged_suite_path.replace(suite_path)
    except (OSError, ValidationError):
        return terminal("evidence_validation_failure")
    print(
        f"validated {len(policy['scenarios'])} performance scenarios; "
        "receipt=suite-receipt.json"
    )
    return 0


def create_baseline(args: argparse.Namespace) -> int:
    policy, policy_digest = load_policy()
    required = policy["baseline_policy"]["clean_run_count"]
    if len(args.receipts) != required:
        raise ValidationError(
            f"baseline creation requires exactly {required} suite receipts"
        )
    resolved = [Path(path).resolve() for path in args.receipts]
    if len(set(resolved)) != required:
        raise ValidationError("baseline inputs must be distinct files")
    suites = [load_json(path) for path in resolved]
    suite_receipts = [
        validate_suite_receipt(suite, policy, policy_digest) for suite in suites
    ]
    run_ids = [suite["run_id"] for suite in suites]
    suite_digests = [digest_value(suite) for suite in suites]
    if len(set(run_ids)) != required:
        raise ValidationError("baseline inputs must have unique run identities")
    if len(set(suite_digests)) != required:
        raise ValidationError("baseline inputs must be distinct validated receipts")
    runner = suites[0]["runner"]
    if any(suite["runner"] != runner for suite in suites):
        raise ValidationError("baseline inputs must share one runner identity")

    scenarios = derive_baseline_scenarios(suite_receipts, policy)
    baseline = {
        "schema": "staxtrace.performance-baseline",
        "policy_digest": policy_digest,
        "runner": runner,
        "clean_run_count": required,
        "run_ids": run_ids,
        "suite_digests": suite_digests,
        "source_suites": suites,
        "scenarios": scenarios,
        "status": "pass",
    }
    validate_baseline(baseline, policy, policy_digest)
    output = Path(args.output)
    atomic_write_json(output, baseline)
    print(f"created same-runner median baseline: {output.name}")
    return 0


def emit_validation_outcome(artifact: str, outcome: str) -> None:
    result = {
        "schema": "staxtrace.performance-validation",
        "artifact": artifact,
        "outcome": outcome,
    }
    validate_schema(result, load_json(SCHEMA_PATHS["validation"]), "validation")
    validate_privacy(result, "validation")
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))


def validate_suite_command(args: argparse.Namespace) -> int:
    policy, policy_digest = load_policy()
    path = Path(args.receipt)
    suite = load_json(path)
    validate_suite_receipt(suite, policy, policy_digest)
    if args.require_current_runner and suite["runner"] != runner_identity(policy):
        raise IncompatibleEvidence("suite runner identity differs")
    emit_validation_outcome(
        "suite", "comparable" if args.require_current_runner else "valid"
    )
    return 0


def validate_baseline_command(args: argparse.Namespace) -> int:
    policy, policy_digest = load_policy()
    path = Path(args.receipt)
    baseline = load_json(path)
    validate_baseline(baseline, policy, policy_digest)
    if args.require_current_runner and baseline["runner"] != runner_identity(policy):
        raise IncompatibleEvidence("baseline runner identity differs")
    emit_validation_outcome(
        "baseline", "comparable" if args.require_current_runner else "valid"
    )
    return 0


def self_check(_: argparse.Namespace) -> int:
    load_policy()
    for path in SCHEMA_PATHS.values():
        load_json(path)
    print("performance policy and schemas self-check PASS")
    return 0


def policy_value(args: argparse.Namespace) -> int:
    policy, digest = load_policy()
    values = {
        "suite_timeout_seconds": policy["suite_timeout_seconds"],
        "setup_timeout_seconds": policy["setup_timeout_seconds"],
        "scenario_timeout_seconds": policy["scenario_timeout_seconds"],
        "policy_digest": digest,
    }
    print(values[args.field])
    return 0


def run_identity(_: argparse.Namespace) -> int:
    load_policy()
    print(secrets.token_hex(16))
    return 0


def cache_runner_command(_: argparse.Namespace) -> int:
    policy, _ = load_policy()
    print(cached_runner_line(runner_identity(policy)))
    return 0


def execution_plan(_: argparse.Namespace) -> int:
    policy, _ = load_policy()
    groups: dict[tuple[str, str, str], list[str]] = {}
    for scenario, expected in policy["scenarios"].items():
        test = expected["test"]
        descriptor = (
            test["package"],
            test["integration_test"],
            test["test_name"],
        )
        groups.setdefault(descriptor, []).append(scenario)
    for descriptor in sorted(groups):
        scenarios = ",".join(sorted(groups[descriptor]))
        print("\t".join((scenarios, *descriptor)))
    return 0


def validate_failure_command(args: argparse.Namespace) -> int:
    policy, digest = load_policy()
    path = Path(args.receipt)
    receipt = load_json(path)
    scenario = receipt.get("scenario")
    if not isinstance(scenario, str) or (
        scenario not in policy["scenarios"] and scenario not in SUITE_SENTINELS
    ):
        raise ValidationError("failure receipt has an unknown scenario")
    validate_failure_receipt(receipt, scenario, policy, digest)
    print(f"validated sanitized failure receipt: {path.name}")
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="verb", required=True)

    command = commands.add_parser("exec")
    command.add_argument("--timeout", type=float, required=True)
    command.add_argument("--suite-timeout", type=float, required=True)
    command.add_argument("--suite-start", type=float, required=True)
    command.add_argument("--scenario", action="append")
    command.add_argument("command", nargs=argparse.REMAINDER)
    command.set_defaults(handler=execute)

    check = commands.add_parser("validate")
    check.add_argument("--receipt-dir", required=True)
    check.add_argument("--baseline")
    check.add_argument("--suite-timeout", type=float, required=True)
    check.add_argument("--suite-start", type=float, required=True)
    check.set_defaults(handler=validate)

    for private_verb, handler in (
        ("_finalize", finalize_worker),
        ("_verify-final", verify_final_suite_worker),
    ):
        private = commands.add_parser(private_verb)
        private.add_argument("--receipt-dir", required=True)
        private.add_argument("--suite-timeout", type=float, required=True)
        private.add_argument("--suite-start", type=float, required=True)
        private.add_argument("--baseline")
        if private_verb == "_verify-final":
            private.add_argument("--suite-name", required=True)
        private.set_defaults(handler=handler)

    baseline = commands.add_parser("baseline")
    baseline.add_argument("--output", required=True)
    baseline.add_argument("receipts", nargs="+")
    baseline.set_defaults(handler=create_baseline)

    suite_receipt = commands.add_parser("validate-suite")
    suite_receipt.add_argument("--receipt", required=True)
    suite_receipt.add_argument("--require-current-runner", action="store_true")
    suite_receipt.set_defaults(handler=validate_suite_command)

    baseline_receipt = commands.add_parser("validate-baseline")
    baseline_receipt.add_argument("--receipt", required=True)
    baseline_receipt.add_argument("--require-current-runner", action="store_true")
    baseline_receipt.set_defaults(handler=validate_baseline_command)

    policy = commands.add_parser("policy-value")
    policy.add_argument(
        "field",
        choices=[
            "suite_timeout_seconds",
            "setup_timeout_seconds",
            "scenario_timeout_seconds",
            "policy_digest",
        ],
    )
    policy.set_defaults(handler=policy_value)

    commands.add_parser("self-check").set_defaults(handler=self_check)
    commands.add_parser("run-identity").set_defaults(handler=run_identity)
    commands.add_parser("cache-runner").set_defaults(handler=cache_runner_command)
    commands.add_parser("plan").set_defaults(handler=execution_plan)

    failure = commands.add_parser("validate-failure")
    failure.add_argument("--receipt", required=True)
    failure.set_defaults(handler=validate_failure_command)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        return int(args.handler(args))
    except IncompatibleEvidence:
        if args.verb in {"validate-suite", "validate-baseline"}:
            emit_validation_outcome(args.verb.removeprefix("validate-"), "incompatible")
            return 3
        raise
    except (OSError, ValidationError, TimeoutError) as error:
        if args.verb in {"validate-suite", "validate-baseline"}:
            emit_validation_outcome(args.verb.removeprefix("validate-"), "invalid")
            return 2
        message = str(error)
        message = re.sub(r"/(?:Users|home|private)/[^\s:]+", "<redacted>", message)
        print(f"performance harness error: {message}", file=sys.stderr)
        return 2
    except Exception:
        # Standalone validation is an untrusted-artifact boundary. Even an
        # unexpected decoder/schema edge must have the same typed, path-free
        # outcome as ordinary malformed evidence rather than a traceback.
        if args.verb in {"validate-suite", "validate-baseline"}:
            emit_validation_outcome(args.verb.removeprefix("validate-"), "invalid")
            return 2
        raise


if __name__ == "__main__":
    raise SystemExit(main())
