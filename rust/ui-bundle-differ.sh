#!/usr/bin/env bash
# Prove that the checked-in dashboard bundle is the deterministic output of
# the checked-in UI inputs. The build happens in a clean export of HEAD, so
# ignored dependencies and other local files cannot influence the result.

set -euo pipefail

readonly PINNED_NODE="v22.18.0"
readonly PINNED_NPM="10.9.3"
readonly RECEIPT_RELATIVE="rust/assets/static/ui-bundle-provenance.json"

DIFFER_SCRATCH_BASE=""
DIFFER_SCRATCH_ROOT=""

die() {
  printf 'ui-bundle-differ: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  case "$DIFFER_SCRATCH_ROOT" in
    "$DIFFER_SCRATCH_BASE"/staxtrace-ui-bundle.*)
      chmod -R u+w "$DIFFER_SCRATCH_ROOT" 2>/dev/null || true
      rm -rf -- "$DIFFER_SCRATCH_ROOT"
      ;;
    "") ;;
    *)
      printf 'ui-bundle-differ: refusing to clean unexpected path\n' >&2
      return 1
      ;;
  esac
}

verify_toolchain() {
  local actual_node actual_npm
  actual_node="$(node --version)"
  actual_npm="$(npm --version)"
  [[ "$actual_node" == "$PINNED_NODE" ]] || die \
    "Node ${PINNED_NODE#v} is required (found ${actual_node#v})"
  [[ "$actual_npm" == "$PINNED_NPM" ]] || die \
    "npm $PINNED_NPM is required (found $actual_npm)"
}

sha256_file() {
  node --input-type=module - "$1" <<'NODE'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'

const file = process.argv[2]
process.stdout.write(createHash('sha256').update(readFileSync(file)).digest('hex'))
NODE
}

# Hash a directory by sorted relative path and file bytes. Length-prefixing
# both parts makes the stream unambiguous. The UI source hash excludes its
# separately hashed lockfile and ignored dependency tree. The output call has
# no exclusions, and is rooted at static/react, so the receipt cannot hash
# itself.
sha256_tree() {
  local root="$1"
  shift
  node --input-type=module - "$root" "$@" <<'NODE'
import { createHash } from 'node:crypto'
import { lstatSync, readdirSync, readFileSync } from 'node:fs'
import { join, relative, resolve, sep } from 'node:path'

const root = resolve(process.argv[2])
const excluded = new Set(process.argv.slice(3))
const files = []

function walk(directory) {
  for (const name of readdirSync(directory).sort()) {
    const absolute = join(directory, name)
    const relativePath = relative(root, absolute).split(sep).join('/')
    if (excluded.has(relativePath) || relativePath === 'node_modules' || relativePath.startsWith('node_modules/')) {
      continue
    }
    const metadata = lstatSync(absolute)
    if (metadata.isSymbolicLink()) {
      throw new Error(`refusing to hash symbolic link: ${relativePath}`)
    }
    if (metadata.isDirectory()) {
      walk(absolute)
    } else if (metadata.isFile()) {
      files.push(relativePath)
    } else {
      throw new Error(`refusing to hash non-file entry: ${relativePath}`)
    }
  }
}

walk(root)
const hash = createHash('sha256')
for (const relativePath of files.sort()) {
  const pathBytes = Buffer.from(relativePath, 'utf8')
  const contents = readFileSync(join(root, ...relativePath.split('/')))
  const lengths = Buffer.alloc(16)
  lengths.writeBigUInt64BE(BigInt(pathBytes.length), 0)
  lengths.writeBigUInt64BE(BigInt(contents.length), 8)
  hash.update(lengths)
  hash.update(pathBytes)
  hash.update(contents)
}
process.stdout.write(hash.digest('hex'))
NODE
}

render_receipt() {
  local ui_root="$1"
  local output_root="$2"
  local source_hash lock_hash output_hash
  source_hash="$(sha256_tree "$ui_root" package-lock.json)"
  lock_hash="$(sha256_file "$ui_root/package-lock.json")"
  output_hash="$(sha256_tree "$output_root")"
  printf '%s\n' \
    '{' \
    '  "schema": "staxtrace.ui-bundle-provenance/1",' \
    '  "hash_algorithm": "sha256-length-prefixed-path-and-bytes",' \
    "  \"source_tree_sha256\": \"$source_hash\"," \
    "  \"lockfile_sha256\": \"$lock_hash\"," \
    "  \"generated_output_tree_sha256\": \"$output_hash\"," \
    '  "toolchain": {' \
    "    \"node\": \"${PINNED_NODE#v}\"," \
    "    \"npm\": \"$PINNED_NPM\"" \
    '  }' \
    '}'
}

run_node_tests() {
  local ui_root="$1"
  local test_files=()
  local test_file
  while IFS= read -r test_file; do
    test_files+=("$test_file")
  done < <(cd "$ui_root" && LC_ALL=C find tests -type f -name '*.test.ts' | LC_ALL=C sort)
  ((${#test_files[@]} > 0)) || die "no Node tests found under ui/tests"
  (cd "$ui_root" && node --test "${test_files[@]}")
}

main() {
  verify_toolchain

  local repo_root
  repo_root="$(git rev-parse --show-toplevel)"

  if [[ "${1:-}" == "--print-receipt" ]]; then
    [[ $# -eq 1 ]] || die "usage: $0 [--print-receipt]"
    render_receipt "$repo_root/ui" "$repo_root/rust/assets/static/react"
    return
  fi
  [[ $# -eq 0 ]] || die "usage: $0 [--print-receipt]"

  git -C "$repo_root" diff --quiet HEAD -- \
    ui rust/assets/static/react "$RECEIPT_RELATIVE" || die \
    "tracked UI inputs or embedded outputs are not committed"

  local scratch_base scratch_root checkout expected_output actual_receipt
  scratch_base="${TMPDIR:-/tmp}"
  scratch_base="${scratch_base%/}"
  scratch_root="$(mktemp -d "$scratch_base/staxtrace-ui-bundle.XXXXXX")"
  checkout="$scratch_root/checkout"
  expected_output="$scratch_root/expected-react"
  actual_receipt="$scratch_root/actual-receipt.json"
  DIFFER_SCRATCH_BASE="$scratch_base"
  DIFFER_SCRATCH_ROOT="$scratch_root"
  trap cleanup EXIT

  mkdir -p "$checkout" "$expected_output"
  git -C "$repo_root" archive --format=tar HEAD | tar -xf - -C "$checkout"
  cp -R "$checkout/rust/assets/static/react/." "$expected_output/"

  local committed_receipt lock_before lock_after
  committed_receipt="$checkout/$RECEIPT_RELATIVE"
  [[ -f "$committed_receipt" ]] || die "missing $RECEIPT_RELATIVE"
  lock_before="$(sha256_file "$checkout/ui/package-lock.json")"

  export CI=1
  export LANG=C
  export LC_ALL=C
  export NPM_CONFIG_AUDIT=false
  export NPM_CONFIG_FUND=false
  export TZ=UTC

  npm ci --prefix "$checkout/ui"
  lock_after="$(sha256_file "$checkout/ui/package-lock.json")"
  [[ "$lock_before" == "$lock_after" ]] || die "npm ci changed the lockfile"

  npm run typecheck --prefix "$checkout/ui"
  run_node_tests "$checkout/ui"
  npm run build --prefix "$checkout/ui"

  [[ -f "$committed_receipt" ]] || die "the UI build deleted its provenance receipt"
  diff -r "$expected_output" "$checkout/rust/assets/static/react" || die \
    "generated dashboard bytes differ from the committed bundle"

  render_receipt "$checkout/ui" "$checkout/rust/assets/static/react" >"$actual_receipt"
  if ! cmp -s "$committed_receipt" "$actual_receipt"; then
    diff -u "$committed_receipt" "$actual_receipt" || true
    die "the committed provenance receipt is stale"
  fi

  printf 'ui bundle provenance: PASS\n'
  printf 'source_tree_sha256=%s\n' "$(sha256_tree "$checkout/ui" package-lock.json)"
  printf 'lockfile_sha256=%s\n' "$lock_after"
  printf 'generated_output_tree_sha256=%s\n' \
    "$(sha256_tree "$checkout/rust/assets/static/react")"
}

main "$@"
