#!/bin/sh
# Contract tests for the Docker LAN gate's CI workflow.
#
# No network and no Docker daemon are needed: these read
# `.github/workflows/lan-docker.yml`, `pins.env` and the gate's documented
# timeouts, and fail if the job stops running the real repository command, gains
# a write scope, a secret, a fork-unsafe trigger, an unbounded timeout, a
# `--keep`/`--image` escape hatch or a way to pass after a failure. They also
# pin the six existing checks in `ci.yml` so adding this workflow cannot quietly
# drop or rename one.
#
# Run: sh scripts/lan-docker/tests/workflow-tests.sh
set -eu

root=$(cd "$(dirname "$0")/../../.." && pwd)
workflow="$root/.github/workflows/lan-docker.yml"
ci="$root/.github/workflows/ci.yml"
lib="$root/scripts/lan-docker/lib.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

checks=0
failures=0
pass() { checks=$((checks + 1)); printf 'ok %s\n' "$1"; }
failed() {
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    if [ -n "${2:-}" ]; then
        printf '%s\n' "$2" | sed 's/^/    /'
    fi
}

if [ ! -f "$workflow" ]; then
    printf 'FAIL %s is missing\n' "$workflow"
    exit 1
fi

# Comment prose is allowed to mention what the code must not do; only live
# YAML counts. Trailing whitespace is ignored so exact-line matches are stable.
clean="$work/workflow"
sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$workflow" >"$clean"
ci_clean="$work/ci"
sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$ci" >"$ci_clean"

hline() { grep -Fxq -- "$1" "$2" >/dev/null 2>&1; }
has() { grep -Fq -- "$1" "$2" >/dev/null 2>&1; }
count_exact() { grep -Fxc -- "$1" "$2" 2>/dev/null || true; }

show() { sed 's/^/    /' "$1"; }

# 1. Triggers: exactly the approved events. Comparing the whole `on:` block,
#    rather than looking for keys, catches an added event (a `schedule`), a
#    narrowing input (`types: [closed]`) and an ignored branch or path list.
cat >"$work/on.expected" <<'ON'
on:
  pull_request:
  push:
    branches: [main]
  workflow_dispatch:
ON
awk '
    /^on:$/ { inside = 1; print; next }
    inside && /^[[:space:]]*$/ { next }
    inside && /^[^[:space:]]/ { exit }
    inside { print }
' "$clean" >"$work/on.actual"
if diff -u "$work/on.expected" "$work/on.actual" >"$work/on.diff" 2>&1; then
    pass "the workflow triggers on exactly pull_request, push to main and workflow_dispatch"
else
    failed "the workflow triggers on exactly pull_request, push to main and workflow_dispatch" "$(show "$work/on.diff")"
fi

# 2. Never pull_request_target: that would give untrusted fork code a writable
#    token and repository secrets.
if ! has 'pull_request_target' "$clean"; then
    pass "the workflow never uses pull_request_target"
else
    failed "the workflow never uses pull_request_target" "$(show "$clean")"
fi

# 3. No paths filter: the check stays visible on every event it triggers on.
if ! has 'paths:' "$clean" && ! has 'paths-ignore:' "$clean"; then
    pass "no paths filter suppresses the check"
else
    failed "no paths filter suppresses the check" "$(show "$clean")"
fi

# 4. Least privilege: read-only contents, no other write scope, no secret.
if hline 'permissions:' "$clean" &&
    hline '  contents: read' "$clean" &&
    ! has ': write' "$clean" &&
    ! has 'write-all' "$clean" &&
    ! has 'secrets.' "$clean"; then
    pass "the token is read-only contents and no secret is used"
else
    failed "the token is read-only contents and no secret is used" "$(show "$clean")"
fi

# 5. GitHub-hosted, never self-hosted or a persistent daemon.
if hline '    runs-on: ubuntu-latest' "$clean" && ! has 'self-hosted' "$clean"; then
    pass "the job runs on a GitHub-hosted ubuntu-latest runner"
else
    failed "the job runs on a GitHub-hosted ubuntu-latest runner" "$(show "$clean")"
fi

# 6. Stable check name: the command itself, as ci.yml names its checks.
if hline '    name: sh scripts/lan-docker/run.sh' "$clean"; then
    pass "the check is named after the command it runs"
else
    failed "the check is named after the command it runs" "$(show "$clean")"
fi

# 7. The job timeout covers the gate's documented phase limits, and is bounded.
sum=0
missing_timeouts=""
for variable in SNX_LAN_BUILD_TIMEOUT SNX_LAN_START_TIMEOUT SNX_LAN_CLIENT_TIMEOUT SNX_LAN_SHUTDOWN_TIMEOUT; do
    seconds=$(sed -n "s/.*${variable}:-\([0-9][0-9]*\)}.*/\1/p" "$lib" | head -1)
    if [ -n "$seconds" ]; then
        sum=$((sum + seconds))
    else
        missing_timeouts="$missing_timeouts $variable"
    fi
done
job_timeout=$(sed -n 's/^    timeout-minutes: \([0-9][0-9]*\)$/\1/p' "$clean")
needed=$(( (sum + 59) / 60 ))
if [ -z "$missing_timeouts" ] && [ "$sum" -gt 0 ] &&
    [ "$(printf '%s\n' "$job_timeout" | grep -c .)" = "1" ] &&
    [ "$job_timeout" -ge "$needed" ] && [ "$job_timeout" -le 60 ]; then
    pass "the ${job_timeout}-minute job covers the documented ${sum}s of phase timeouts"
else
    failed "the job timeout covers the documented ${sum}s of phase timeouts" \
        "documented limit sum: ${sum}s (${needed} min); job timeout: '${job_timeout}'; missing:${missing_timeouts:- none}"
fi

# 8. Exactly the real gate command, on one whole line, with no options and no
#    shell operator that could mask its exit status.
#    `grep -Fx` (not a substring match) is what rejects `--help`, `|| :` and a
#    rewritten `bash -c` wrapper.
gate_runs=$(count_exact '        run: sh scripts/lan-docker/run.sh' "$clean")
if [ "$gate_runs" = "1" ] &&
    ! has '--keep' "$clean" &&
    ! has '--image' "$clean" &&
    ! has '--pull' "$clean" &&
    ! grep -Eq '^[[:space:]]*run: .*(\|\||;|&&)' "$clean"; then
    pass "the job runs exactly 'sh scripts/lan-docker/run.sh' with no options or masking"
else
    failed "the job runs exactly 'sh scripts/lan-docker/run.sh' with no options or masking" \
        "matching run lines: ${gate_runs:-0}
$(show "$clean")"
fi

# 9. Fail closed: no continue-on-error, no `|| true`, no skipped gate step.
if ! has 'continue-on-error' "$clean" && ! has '|| true' "$clean" && ! has 'if:' "$clean"; then
    pass "a failing build, assertion, cleanup or timeout fails the job"
else
    failed "a failing build, assertion, cleanup or timeout fails the job" "$(show "$clean")"
fi

# 10. Checkout is pinned to a release commit, uses the event revision (no
#     `ref`/`repository` override) and leaves no credentials behind.
if [ "$(sed -n 's/^      - uses: actions\/checkout@[0-9a-f]\{40\}$/checkout/p' "$clean" | grep -c checkout)" = "1" ] &&
    ! grep -Eq '^[[:space:]]+(ref|repository|token|fetch-depth):' "$clean" &&
    hline '          persist-credentials: false' "$clean"; then
    pass "checkout is pinned by commit, uses the event revision and keeps no token"
else
    failed "checkout is pinned by commit, uses the event revision and keeps no token" "$(show "$clean")"
fi

# 11. The six existing checks keep their jobs, names and triggers in ci.yml:
#     fmt, clippy, test on two toolchains and release on two toolchains.
if hline '  pull_request:' "$ci_clean" &&
    hline '  push:' "$ci_clean" &&
    hline '    branches: [main]' "$ci_clean" &&
    hline '  fmt:' "$ci_clean" &&
    hline '  clippy:' "$ci_clean" &&
    hline '  test:' "$ci_clean" &&
    hline '  release:' "$ci_clean" &&
    hline '    name: cargo fmt --all --check' "$ci_clean" &&
    hline '    name: cargo clippy --workspace --all-targets -- -D warnings' "$ci_clean" &&
    hline "    name: cargo test --workspace (\${{ matrix.toolchain }})" "$ci_clean" &&
    hline "    name: cargo build --workspace --release (\${{ matrix.toolchain }})" "$ci_clean" &&
    [ "$(count_exact '        toolchain: [stable, 1.85.0]' "$ci_clean")" = "2" ]; then
    pass "ci.yml still declares the six existing checks and their triggers"
else
    failed "ci.yml still declares the six existing checks and their triggers" "$(show "$ci_clean")"
fi

printf '\n%d checks, %d failed\n' "$checks" "$failures"
[ "$failures" -eq 0 ]
