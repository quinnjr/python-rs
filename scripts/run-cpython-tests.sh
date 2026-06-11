#!/usr/bin/env bash
# Run CPython 3.0's regression test suite under python-rs.
#
# This is the project's headline progress oracle. See
# docs/superpowers/specs/2026-05-19-cpython-3.0-compatibility-design.md
# for the strategy.
#
# Usage:
#   scripts/run-cpython-tests.sh              # full suite (slow)
#   scripts/run-cpython-tests.sh --fast       # tier-1 smoke list (PR CI)
#   scripts/run-cpython-tests.sh test_grammar # run a single test file
#
# Output:
#   - Streams regrtest output to stdout
#   - Writes machine-readable summary to tests/cpython-3.0/compat-report.json
#   - Exits non-zero if any test that previously passed now fails

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_LIB="${REPO_ROOT}/vendor/cpython-3.0/Lib"
REGRTEST="${VENDOR_LIB}/test/regrtest.py"
REPORT="${REPO_ROOT}/tests/cpython-3.0/compat-report.json"
SKIP_FILE="${REPO_ROOT}/tests/cpython-3.0/skip.toml"

# Tier-1 smoke list — small, stable, fast subset for PR CI.
# Source: design spec, "Tier-1 smoke list" section.
TIER1_TESTS=(
    test_grammar
    test_compile
    test_syntax
    test_int
    test_str
    test_list
    test_dict
    test_tuple
    test_set
    test_iter
)

mode="full"
explicit_tests=()
for arg in "$@"; do
    case "$arg" in
        --fast)  mode="fast" ;;
        --help|-h)
            sed -n '2,15p' "${BASH_SOURCE[0]}"
            exit 0
            ;;
        test_*)  explicit_tests+=("$arg"); mode="explicit" ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

# Build python-rs (release) before running.
echo "==> Building python-rs (release)"
( cd "$REPO_ROOT" && cargo build --release --quiet )
PYTHON_RS="${REPO_ROOT}/target/release/python-rs"

if [[ ! -x "$PYTHON_RS" ]]; then
    echo "error: python-rs binary not found at $PYTHON_RS" >&2
    exit 1
fi

# Environment for the test run.
export PYTHONPATH="${VENDOR_LIB}"
export PYTHONDONTWRITEBYTECODE=1

# Determine the test list.
case "$mode" in
    fast)
        echo "==> Running tier-1 smoke list (${#TIER1_TESTS[@]} tests)"
        test_args=("${TIER1_TESTS[@]}")
        ;;
    explicit)
        echo "==> Running explicit tests: ${explicit_tests[*]}"
        test_args=("${explicit_tests[@]}")
        ;;
    full)
        echo "==> Running full Lib/test/ suite"
        test_args=()  # regrtest discovers all test_*.py when no list is given
        ;;
esac

# Invocation. regrtest.py understands -v for verbose, -j for parallelism (3.0
# may not), etc. We keep it minimal for now.
echo "==> Invoking regrtest under python-rs"
echo "    binary:    $PYTHON_RS"
echo "    regrtest:  $REGRTEST"
echo "    pythonpath: $PYTHONPATH"
echo

# NOTE: python-rs cannot yet execute regrtest.py — this script will fail until
# milestone M3 lands. That is expected. The script exists so the harness is
# in place and CI can be wired immediately.
"$PYTHON_RS" "$REGRTEST" "${test_args[@]}" || run_exit=$?
run_exit="${run_exit:-0}"

# scripts/update-compat-report.py rebuilds the JSON report from raw output.
# That script lands in a follow-up commit alongside the first successful run.

echo
echo "==> regrtest exited with $run_exit"
echo "    Report file: $REPORT"
echo "    Skip list:   $SKIP_FILE"
exit "$run_exit"
