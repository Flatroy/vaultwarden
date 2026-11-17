#!/usr/bin/env bash
# Tests for sync_upstream.sh using throwaway git repos. Works with bash 3.2 (macOS) and Linux.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/sync_upstream.sh"
readonly EXIT_OK=0
readonly EXIT_CONFLICT=10
readonly EXIT_DRIFT=11
readonly MANIFEST_DIR="src/static/templates/email/ru"
readonly EN_DIR="src/static/templates/email"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.test GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.test

FAILURES=0
UPSTREAM="" FORK=""

expect() { # name expected actual
    if [ "$2" = "$3" ]; then
        echo "  ok   $1: '$3'"
    else
        echo "  FAIL $1: expected '$2', got '$3'"
        FAILURES=$((FAILURES + 1))
    fi
}

commit_in() { # repo message  (stages everything)
    git -C "$1" add -A && git -C "$1" commit -q -m "$2"
}

listed_paths() { sed -n 's/^  \([^ ].*\)$/\1/p' "$WORK/out.txt" | sort; } # lines the script indents under a header

new_case() { # name: fresh upstream work repo + fork clone with remote "upstream"
    echo "== case: $1"
    UPSTREAM="$WORK/$1-upstream"
    FORK="$WORK/$1-fork"
    git init -q -b main "$UPSTREAM"
    mkdir -p "$UPSTREAM/$EN_DIR"
    echo "hello" >"$UPSTREAM/$EN_DIR/a.hbs"
    echo "base" >"$UPSTREAM/file.txt"
    commit_in "$UPSTREAM" base
    git clone -q "$UPSTREAM" "$FORK"
    git -C "$FORK" remote rename origin upstream
}

run_sync() { # sets RC, output in $WORK/out.txt
    RC=0
    (cd "$FORK" && "$SCRIPT") >"$WORK/out.txt" 2>&1 || RC=$?
    sed 's/^/    | /' "$WORK/out.txt"
}

new_case clean
echo "upstream change" >"$UPSTREAM/new.txt"; commit_in "$UPSTREAM" upstream-change
echo "fork change" >"$FORK/fork.txt"; commit_in "$FORK" fork-change
run_sync
expect "exit code" "$EXIT_OK" "$RC"
expect "upstream file merged" "yes" "$([ -f "$FORK/new.txt" ] && echo yes || echo no)"
expect "fork file kept" "yes" "$([ -f "$FORK/fork.txt" ] && echo yes || echo no)"
expect "no conflicts reported" "0" "$(grep -c "merge conflict" "$WORK/out.txt" || true)"
expect "no drift reported" "0" "$(grep -c "outdated" "$WORK/out.txt" || true)"

new_case conflict
echo "upstream version" >"$UPSTREAM/file.txt"; commit_in "$UPSTREAM" upstream-edit
echo "up" >"$UPSTREAM/other.txt"; commit_in "$UPSTREAM" upstream-other
echo "fork version" >"$FORK/file.txt"; commit_in "$FORK" fork-edit
echo "fork" >"$FORK/other.txt"; commit_in "$FORK" fork-other
head_before="$(git -C "$FORK" rev-parse HEAD)"
run_sync
expect "exit code" "$EXIT_CONFLICT" "$RC"
expect "conflict list" "file.txt
other.txt" "$(listed_paths)"
expect "HEAD unchanged" "$head_before" "$(git -C "$FORK" rev-parse HEAD)"
expect "working tree clean" "" "$(git -C "$FORK" status --porcelain)"
expect "no merge in progress" "no" "$([ -f "$FORK/.git/MERGE_HEAD" ] && echo yes || echo no)"
expect "no drift reported" "0" "$(grep -c "outdated" "$WORK/out.txt" || true)"

new_case drift
mkdir -p "$FORK/$MANIFEST_DIR"
(cd "$FORK" && shasum -a 256 "$EN_DIR/a.hbs" >"$MANIFEST_DIR/SOURCE_HASHES")
echo "ru" >"$FORK/$MANIFEST_DIR/a.hbs"
commit_in "$FORK" add-manifest
echo "changed english" >"$UPSTREAM/$EN_DIR/a.hbs"; commit_in "$UPSTREAM" change-english
run_sync
expect "exit code" "$EXIT_DRIFT" "$RC"
expect "outdated list" "$EN_DIR/a.hbs" "$(listed_paths)"
expect "merge happened" "changed english" "$(cat "$FORK/$EN_DIR/a.hbs")"

new_case in-sync
mkdir -p "$FORK/$MANIFEST_DIR"
(cd "$FORK" && shasum -a 256 "$EN_DIR/a.hbs" >"$MANIFEST_DIR/SOURCE_HASHES")
commit_in "$FORK" add-manifest
echo "unrelated" >"$UPSTREAM/unrelated.txt"; commit_in "$UPSTREAM" unrelated
run_sync
expect "exit code" "$EXIT_OK" "$RC"
expect "no drift reported" "0" "$(grep -c "outdated" "$WORK/out.txt" || true)"

new_case no-manifest
echo "x" >"$UPSTREAM/x.txt"; commit_in "$UPSTREAM" x
run_sync
expect "exit code" "$EXIT_OK" "$RC"
expect "skip message" "1" "$(grep -c "skipping translation drift check" "$WORK/out.txt" || true)"

new_case no-remote
git -C "$FORK" remote remove upstream
run_sync
expect "exit code" "1" "$RC"

echo
if [ "$FAILURES" -eq 0 ]; then echo "ALL PASSED"; else echo "$FAILURES FAILED"; exit 1; fi
