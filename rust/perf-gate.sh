#!/usr/bin/env bash
# Reproducible release-only performance evidence.
set -euo pipefail

cd "$(dirname "$0")"

if [[ "${1:-}" == "baseline" ]]; then
    shift
    if [[ "$#" -ne 4 ]]; then
        echo "usage: ./perf-gate.sh baseline OUTPUT SUITE_RECEIPT_1 SUITE_RECEIPT_2 SUITE_RECEIPT_3" >&2
        exit 2
    fi
    python3 perf/run.py baseline --output "$1" "$2" "$3" "$4"
    exit $?
fi

baseline_path=""
receipt_dir=""
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --baseline)
            [[ "$#" -ge 2 ]] || { echo "--baseline requires a path" >&2; exit 2; }
            baseline_path="$2"
            shift 2
            ;;
        --receipt-dir)
            [[ "$#" -ge 2 ]] || { echo "--receipt-dir requires a path" >&2; exit 2; }
            receipt_dir="$2"
            shift 2
            ;;
        *)
            echo "unknown performance-gate argument" >&2
            exit 2
            ;;
    esac
done

python3 perf/run.py self-check
if [[ -z "$receipt_dir" ]]; then
    receipt_root="${CARGO_TARGET_DIR:-$PWD/target}/performance-receipts"
    receipt_dir="$receipt_root/run-$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi
mkdir -p "$receipt_dir"
if find "$receipt_dir" -maxdepth 1 \
    \( -name '*.json' -o -name '.suite-receipt.*.pending' \) \
    -print -quit | grep -q .; then
    echo "receipt directory must not contain prior performance evidence" >&2
    exit 2
fi

export STAX_PERF_HARNESS=1
export STAX_PERF_RECEIPT_DIR="$receipt_dir"
export STAX_PERF_RUN_ID="$(python3 perf/run.py run-identity)"
export STAX_PERF_POLICY_DIGEST="$(python3 perf/run.py policy-value policy_digest)"
runner_snapshot="$(python3 perf/run.py cache-runner)"
IFS=$'\t' read -r \
    STAX_PERF_RUNNER_OS \
    STAX_PERF_RUNNER_ARCH \
    STAX_PERF_RUNNER_CLASS \
    STAX_PERF_RUNNER_TOOLCHAIN \
    STAX_PERF_RUNNER_KEY \
    STAX_PERF_RUNNER_DIGEST <<< "$runner_snapshot"
unset runner_snapshot
if [[ -z "$STAX_PERF_RUNNER_OS" || -z "$STAX_PERF_RUNNER_ARCH" || \
      -z "$STAX_PERF_RUNNER_CLASS" || -z "$STAX_PERF_RUNNER_TOOLCHAIN" || \
      -z "$STAX_PERF_RUNNER_KEY" || -z "$STAX_PERF_RUNNER_DIGEST" ]]; then
    echo "unable to cache the complete performance runner identity" >&2
    exit 2
fi
export STAX_PERF_RUNNER_OS STAX_PERF_RUNNER_ARCH STAX_PERF_RUNNER_CLASS
export STAX_PERF_RUNNER_TOOLCHAIN STAX_PERF_RUNNER_KEY STAX_PERF_RUNNER_DIGEST
readonly STAX_PERF_RUNNER_OS STAX_PERF_RUNNER_ARCH STAX_PERF_RUNNER_CLASS
readonly STAX_PERF_RUNNER_TOOLCHAIN STAX_PERF_RUNNER_KEY STAX_PERF_RUNNER_DIGEST
suite_timeout="$(python3 perf/run.py policy-value suite_timeout_seconds)"
setup_timeout="$(python3 perf/run.py policy-value setup_timeout_seconds)"
scenario_timeout="$(python3 perf/run.py policy-value scenario_timeout_seconds)"
execution_plan="$(python3 perf/run.py plan)"
suite_start="$(python3 -c 'import time; print(time.monotonic())')"
export STAX_PERF_SUITE_START="$suite_start"
export STAX_PERF_SUITE_TIMEOUT="$suite_timeout"
readonly suite_start suite_timeout STAX_PERF_SUITE_START STAX_PERF_SUITE_TIMEOUT

run_bounded() {
    local timeout="$1"
    shift
    python3 perf/run.py exec \
        --timeout "$timeout" \
        --suite-timeout "$suite_timeout" \
        --suite-start "$suite_start" \
        -- "$@"
}

run_scenario() {
    local scenario_csv="$1"
    local package="$2"
    local integration_test="$3"
    local test_name="$4"
    local scenario_args=()
    local scenario_ids=()
    IFS=',' read -r -a scenario_ids <<< "$scenario_csv"
    for scenario in "${scenario_ids[@]}"; do
        scenario_args+=(--scenario "$scenario")
    done
    python3 perf/run.py exec \
        --timeout "$scenario_timeout" \
        --suite-timeout "$suite_timeout" \
        --suite-start "$suite_start" \
        "${scenario_args[@]}" \
        -- cargo test --release --locked -p "$package" --test "$integration_test" \
        "$test_name" -- --ignored --exact --nocapture --test-threads=1
}

# Compile outside scenario timers. The verified plan includes core and strict
# additive registrations, so later scenarios do not rewrite this runner.
run_bounded "$setup_timeout" cargo build --release --locked -p stax-hooks
while IFS=$'\t' read -r compile_scenarios package integration_test compile_test_name; do
    run_bounded "$setup_timeout" cargo test --release --locked \
        -p "$package" --test "$integration_test" --no-run
done <<< "$execution_plan"

while IFS=$'\t' read -r scenarios package integration_test test_name; do
    run_scenario "$scenarios" "$package" "$integration_test" "$test_name"
done <<< "$execution_plan"

validate_args=(
    validate
    --receipt-dir "$receipt_dir"
    --suite-timeout "$suite_timeout"
    --suite-start "$suite_start"
)
if [[ -n "$baseline_path" ]]; then
    validate_args+=(--baseline "$baseline_path")
fi
python3 perf/run.py "${validate_args[@]}"
echo "performance suite PASS: suite-receipt.json"
