#!/usr/bin/env bash
# Merge upstream/main into the current branch of the fork checked out in $PWD.
#
# Exit codes:
#   0   merged (or already up to date), translations current or no manifest
#   1   unexpected error
#   10  merge conflict: merge aborted, conflicting files listed on stdout
#   11  merged, but Russian email translations are outdated: English templates listed on stdout
set -euo pipefail

readonly EXIT_OK=0
readonly EXIT_ERROR=1
readonly EXIT_CONFLICT=10
readonly EXIT_DRIFT=11

readonly UPSTREAM_REMOTE="${UPSTREAM_REMOTE:-upstream}"
readonly UPSTREAM_BRANCH="${UPSTREAM_BRANCH:-main}"
readonly HASH_MANIFEST="src/static/templates/email/ru/SOURCE_HASHES"

log() { echo "sync_upstream: $*"; }
die() { echo "sync_upstream: ERROR: $*" >&2; exit "$EXIT_ERROR"; }

sha256_check() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c --quiet "$1"
    else
        shasum -a 256 -c --quiet "$1"
    fi
}

# Prints the English templates whose hash no longer matches the manifest.
list_drift() {
    # The checker exits non-zero on drift; its FAILED lines are the result.
    sha256_check "$HASH_MANIFEST" 2>/dev/null | sed -n 's/: FAILED.*$//p' || true
}

merge_upstream() {
    if git merge --no-edit "${UPSTREAM_REMOTE}/${UPSTREAM_BRANCH}"; then
        return "$EXIT_OK"
    fi
    local conflicts
    conflicts="$(git diff --name-only --diff-filter=U)"
    [ -n "$conflicts" ] || die "merge failed without conflicts (dirty tree or missing identity?)"
    git merge --abort
    log "merge conflict in:"
    echo "$conflicts" | sed 's/^/  /'
    return "$EXIT_CONFLICT"
}

cd "$(git rev-parse --show-toplevel)"
git remote get-url "$UPSTREAM_REMOTE" >/dev/null 2>&1 || die "remote '$UPSTREAM_REMOTE' not configured"

log "fetching ${UPSTREAM_REMOTE}/${UPSTREAM_BRANCH} and tags"
git fetch --tags "$UPSTREAM_REMOTE" "$UPSTREAM_BRANCH"

status=$EXIT_OK
merge_upstream || status=$?
[ "$status" -eq "$EXIT_OK" ] || exit "$status"

if [ ! -f "$HASH_MANIFEST" ]; then
    log "no $HASH_MANIFEST, skipping translation drift check"
    exit "$EXIT_OK"
fi

drift="$(list_drift)"
if [ -n "$drift" ]; then
    log "translations outdated for:"
    echo "$drift" | sed 's/^/  /'
    exit "$EXIT_DRIFT"
fi
log "translations up to date"
exit "$EXIT_OK"
