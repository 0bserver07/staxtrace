#!/usr/bin/env bash
# The gates every wave of the Rust port must pass, in order.
#
#   1. cargo fmt --check   — formatting is the law inside rust/. The Python
#                            tree's format exemption does not apply here; a
#                            fleet of agents writing Rust in parallel needs one
#                            canonical layout or every diff is noise.
#   2. cargo clippy        — --all-targets so tests and benches are linted too,
#                            -D warnings so a lint is a failure, not a message
#                            nobody reads.
#   3. cargo test          — the whole workspace.
#   4. CLI byte-parity     — every verb × {fresh store, populated-FTS store} ×
#                            {text, --json}: stdout, stderr and exit code
#                            diffed byte for byte against the Python CLI. This
#                            is the P0 gate (rust/DIRECTIVE-PARITY-P0.md):
#                            "drop-in or it doesn't ship", and the
#                            populated-FTS state is the maintainer's machine,
#                            not an edge case. Skipped only where the Python
#                            venv or the harness states are absent — loudly.
#   5. Ingest parity       — one full `run_ingest` pass over a scratch home by
#                            each implementation, then a full-row diff of
#                            projects / sessions / messages / usage_events /
#                            ingest_log. The wave-4 gate: the store IS the
#                            contract, so the comparison is of rows, not counts.
#                            Fixture-corpus only by default (~2 s); export
#                            STAX_INGEST_REAL_PROJECTS=N to widen it to N of the
#                            maintainer's real ~/.claude projects, which is what
#                            the wave gate itself was run at.
#   6. Endpoint parity     — gate 4's bargain for HTTP: boot BOTH servers
#                            against one shared STACKUNDERFLOW_HOME, walk
#                            parity/endpoint-cases.txt in order, diff status +
#                            content-type + BODY BYTES. Case-file driven, so an
#                            endpoint batch adds rows rather than editing the
#                            gate. Rows whose id starts `!` are KNOWN-OPEN: the
#                            differ prints them in full and does not fail on
#                            them, which is how an unported endpoint stays
#                            visible instead of absent. Skipped only where the
#                            Python venv or the harness state is missing —
#                            loudly, like gate 4.
#
# NOT a gate, on purpose — the per-wave differs that are too slow to run per
# commit. Each is a standalone script with the same case-file shape and the same
# exit codes; run them at a wave boundary, not in this loop:
#
#   rust/ingest-tail-proof.sh   the live-tail latency measurement (wave 4)
#   rust/hooks-parity.sh        the hook surface, all nine ids (wave 6) —
#                               80 recorded invocations diffed on stdout bytes,
#                               stderr, exit code, the `captured_events` rows
#                               each side wrote and the governance JSON each
#                               side left behind. 36.7 s measured, because 80 of
#                               its 160 process starts are CPython at ~190 ms
#                               and nine of them spawn a SECOND CPython (the
#                               recall hook shells `memory file --json`). The
#                               threshold for wiring a differ in here was 10 s;
#                               this is 3.7x over it, so it stays out and
#                               `cargo test -p stax-hooks` (75 tests, 0.02 s)
#                               carries the per-commit half.
#   rust/import-differ.sh       `import`'s SUCCESS leg (RS-8-101) — and the
#                               proof that its REJECTIONS did nothing. Out for
#                               the same reason as `backup-differ.sh`: the verb
#                               writes `store.db`, so it needs two homes and a
#                               row-level store comparison rather than a stdout
#                               diff. The 47 `T7-imp-*` rows in gate 4 are all
#                               rejections, by design (DIV-078: a case row is
#                               side-effect-free); this is where the writes,
#                               the cursor sidecar, and the argv + cwd +
#                               environment the export command actually saw are
#                               compared. ~20 s, ten scenarios.
#   rust/project-set-differ.sh  `POST /api/project`'s SUCCESS leg (RS-5-095).
#                               Out of gate 6 for a reason of KIND, not of
#                               speed: it needs TWO homes. What the endpoint
#                               mutates is process-global server state, so a
#                               row in the shared-home matrix would re-point
#                               the reference's current project mid-run — and
#                               only the reference's — silently changing what
#                               every row after it means. Six requests per
#                               side, raw sockets, transcripts diffed;
#                               ~15 s. The three ERROR legs are ordinary rows
#                               in gate 6, because the guard order in
#                               `projects.py:44` proves they return before the
#                               assignment. `rust/PROJECT-SET-DIFFER.md` is
#                               the argument in full.
#
# Usage:  rust/ci.sh                       (runs all seven)
#         rust/ci.sh --skip-parity         (gates 0-3 only; boxes without the venv)
#         rust/ci.sh --clean-checkout-only (gate 0 only)
# Exit:   first failing gate's status.
set -euo pipefail

cd "$(dirname "$0")"

SKIP_PARITY=0
CLEAN_CHECKOUT_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --skip-parity) SKIP_PARITY=1 ;;
        --clean-checkout-only) CLEAN_CHECKOUT_ONLY=1 ;;
        *) echo "ci.sh: unknown argument '$arg'" >&2; exit 2 ;;
    esac
done

# The toolchain is user-local (docs/specs/rust-port.md §5: rustup, no sudo).
# Rustup must WIN over a distro cargo (found the hard way: /usr/bin/cargo 1.75
# shadows the pinned 1.97 and dies on edition2024). Prepend unconditionally.
if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

gate() {
    printf '\n=== %s ===\n' "$1"
    shift
    "$@"
}

# Gate 0 exists because it caught us: a manifest edit lived only in the working
# tree while its Cargo.lock entry was committed, so five commits in a row failed
# to compile from a clean checkout while every in-tree run stayed green. The
# working tree's opinion of itself is not evidence.
#
# The extraction set is the Rust workspace plus the contracts exercised by its
# tests. All compile-time data, migrations, skills and UI files live under
# `rust/assets/`, so archiving the entire workspace is both complete and hard to
# make stale when a new embedded asset class is added. `git archive` takes only
# tracked files: a build that accidentally depends on an untracked source or
# asset therefore fails here even when an in-tree build succeeds.
#
# The archive path is unique on every run, so Cargo sees new path-package IDs
# and re-evaluates build-script dependencies while still reusing third-party
# artifacts from the caller's target directory. Incremental compilation is
# disabled for this gate so an old workspace object cannot stand in for the
# extracted tracked source.
# STANDALONE, NOT A GATE (same ruling as `hooks-parity.sh`): `sync-parity.sh`
# copies four SQLite fixtures per case per implementation across 173 corpus rows
# and spawns two processes for each, plus nine `age` round trips and ten CLI
# invocations. Measured wall time is well past the 10 s per-gate threshold this
# file holds itself to, and none of it is a regression surface the other gates
# do not already cover (`stax-sync`'s 101 unit tests run in gate 3, and
# `endpoint-parity.sh`'s `SY-*` rows cover both sync endpoints in gate 6).
# Run it directly:
#   rust/sync-parity.sh                # 192 identical / 0 divergent
#   rust/sync-parity.sh --build-state  # rebuild the synthetic multi-device stores

_cc_tmp=""

cleanup_clean_checkout() {
    if [[ -n "$_cc_tmp" && "$_cc_tmp" != "/" && -d "$_cc_tmp" ]]; then
        rm -rf -- "$_cc_tmp"
    fi
    _cc_tmp=""
}

trap cleanup_clean_checkout EXIT
trap 'exit 130' HUP INT TERM

require_clean_checkout_file() {
    local archive_root="$1"
    local relative_path="$2"
    local asset_class="$3"
    if [[ ! -f "$archive_root/$relative_path" ]]; then
        echo "GATE 0 FAILED: missing required embedded asset class: $asset_class ($relative_path)" >&2
        return 1
    fi
}

run_clean_checkout_gate() {
    local repo_root tmp_parent
    repo_root="$(git rev-parse --show-toplevel)"
    tmp_parent="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
    _cc_tmp="$(mktemp -d "$tmp_parent/stax-clean-checkout.XXXXXXXX")"
    if [[ ! -d "$_cc_tmp" || "$_cc_tmp" == "/" ]]; then
        echo "GATE 0 FAILED: could not create a bounded temporary archive" >&2
        return 1
    fi

    if ! (cd "$repo_root" && git archive --format=tar HEAD -- rust contracts) \
        | tar -x -C "$_cc_tmp"; then
        echo "GATE 0 FAILED: could not extract tracked Rust build inputs" >&2
        return 1
    fi

    # A strict symbolic test hook exercises the missing-asset path without
    # accepting a filesystem path or allowing deletion outside the archive.
    case "${STAX_CLEAN_CHECKOUT_TEST_MISSING_ASSET:-}" in
        "") ;;
        ui-bundle-receipt)
            rm -f -- "$_cc_tmp/rust/assets/static/ui-bundle-provenance.json"
            ;;
        *)
            echo "GATE 0 FAILED: unknown missing-asset test selector" >&2
            return 2
            ;;
    esac

    require_clean_checkout_file "$_cc_tmp" \
        rust/assets/data/models.toml "pricing manifest"
    require_clean_checkout_file "$_cc_tmp" \
        rust/assets/adapters/capabilities.json "adapter capability table"
    require_clean_checkout_file "$_cc_tmp" \
        rust/assets/infra/model_candidates.json "model candidate table"
    require_clean_checkout_file "$_cc_tmp" \
        rust/assets/static/react/index.html "UI entry point"
    require_clean_checkout_file "$_cc_tmp" \
        rust/assets/static/ui-bundle-provenance.json "UI bundle provenance receipt"

    if ! (cd "$_cc_tmp/rust" \
        && CARGO_INCREMENTAL=0 \
            CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/rust/target}" \
            cargo check --workspace --all-targets --locked --quiet); then
        echo "GATE 0 FAILED: HEAD does not build and compile its tests from a clean checkout" >&2
        return 1
    fi

    cleanup_clean_checkout
    echo "    clean checkout builds and its tests compile"
}

echo "=== gate 0/7  clean-checkout build/test compilation (git archive HEAD) ==="
run_clean_checkout_gate

if [[ "$CLEAN_CHECKOUT_ONLY" = 1 ]]; then
    echo "clean-checkout gate green"
    exit 0
fi

gate "gate 1/7  cargo fmt --check" cargo fmt --check
gate "gate 2/7  cargo clippy --workspace --all-targets -- -D warnings" \
    cargo clippy --workspace --all-targets -- -D warnings
gate "gate 3/7  cargo test --workspace" cargo test --workspace

# Gate 4 — the P0 gate. It is last because it is the slowest and because a
# workspace that does not compile cannot be diffed; it is not optional because
# every gate above it can be green while the shipped binary answers a question
# differently from the tool it replaces. That is exactly what happened: 39 of
# 188 cases diverged on the populated-FTS store while gates 0-3 were green.
printf '\n=== gate 4/7  CLI byte-parity vs the Python CLI ===\n'
if [ "$SKIP_PARITY" = 1 ]; then
    echo '  !!  GATE 4 SKIPPED by --skip-parity.'
    echo '  !!  Drop-in parity is UNVERIFIED for this run. The only sanctioned'
    echo '  !!  reason is a box without the Python venv; a green ci.sh here'
    echo '  !!  does NOT mean the binary is a drop-in replacement.'
else
    _parity_out="$(mktemp)"
    set +e
    ./parity-cli.sh 2>&1 | tee "$_parity_out"
    _parity_rc="${PIPESTATUS[0]}"
    set -e
    case "$_parity_rc" in
        0) : ;;
        2)
            echo
            echo '  !!  GATE 4 COULD NOT RUN (setup): see the message above.'
            echo '  !!  Build the states once with: rust/parity-cli.sh --build-state'
            echo '  !!  Re-run with --skip-parity only if this box genuinely has'
            echo '  !!  no Python venv — parity is UNVERIFIED either way.'
            rm -f "$_parity_out"
            exit 2
            ;;
        *)
            echo
            echo "GATE 4 FAILED: the CLI is not a drop-in replacement (see the tally above)."
            rm -f "$_parity_out"
            exit 1
            ;;
    esac
    rm -f "$_parity_out"
fi

# Gate 5 — the wave-4 ingest gate. Cheap in its default shape because the
# fixture corpus is 1 MB; the value is that the writer, the watermarks and the
# per-record normalize hook cannot regress silently between here and wave 10.
printf '\n=== gate 5/7  ingest parity (full-row, scratch home) ===\n'
if [ "$SKIP_PARITY" = 1 ]; then
    echo '  !!  GATE 5 SKIPPED by --skip-parity (it needs the Python venv too).'
else
    set +e
    STAX_INGEST_REAL_PROJECTS="${STAX_INGEST_REAL_PROJECTS:-0}" ./ingest-parity.sh
    _ingest_rc=$?
    set -e
    case "$_ingest_rc" in
        0) : ;;
        2)
            echo
            echo '  !!  GATE 5 COULD NOT RUN (setup): see the message above.'
            exit 2
            ;;
        *)
            echo
            echo "GATE 5 FAILED: the ingest layer does not reproduce Python's rows."
            exit 1
            ;;
    esac
fi

# Gate 6 — the wave-5 HTTP gate. Last because it is the only one that binds
# ports and boots two servers, and because a workspace that fails gate 3 has
# nothing worth serving. It exists for the same reason gate 4 does: every gate
# above it can be green while the shipped server answers a request differently
# from the one it replaces, and a dashboard is a *byte* contract — key order,
# float presentation and `ensure_ascii` are all invisible to a parsed compare.
printf '\n=== gate 6/7  endpoint byte-parity vs the Python server ===\n'
if [ "$SKIP_PARITY" = 1 ]; then
    echo '  !!  GATE 6 SKIPPED by --skip-parity.'
    echo '  !!  HTTP parity is UNVERIFIED for this run. The React bundle is the'
    echo '  !!  oracle and it has not been pointed at this build; a green ci.sh'
    echo '  !!  here does NOT mean the dashboard works against the port.'
else
    set +e
    ./endpoint-parity.sh
    _endpoint_rc=$?
    set -e
    case "$_endpoint_rc" in
        0) : ;;
        2)
            echo
            echo '  !!  GATE 6 COULD NOT RUN (setup): see the message above.'
            echo '  !!  Build the state once with: rust/parity-cli.sh --build-state'
            echo '  !!  Re-run with --skip-parity only if this box genuinely has'
            echo '  !!  no Python venv — HTTP parity is UNVERIFIED either way.'
            exit 2
            ;;
        *)
            echo
            echo "GATE 6 FAILED: the server does not reproduce the reference's bytes."
            exit 1
            ;;
    esac
fi

printf '\nall gates green\n'
