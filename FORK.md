# Fork notes

Fork of [dani-garcia/vaultwarden](https://github.com/dani-garcia/vaultwarden) with per-user
Russian/English email templates (Russian ones in `src/static/templates/email/ru/`).

## Branch model

- `main` = upstream `main` + the fork patch.
- `.github/workflows/sync-upstream.yml` runs daily (and on demand), merges `upstream/main`
  into `main`, and pushes `main` and upstream tags.
- The merge logic lives in `tools/sync_upstream.sh` (tests: `tools/test_sync_upstream.sh`).

## One-time setup

1. Actions are disabled on new forks: enable them in the repository's Actions tab.
2. Add the repository secret `SYNC_TOKEN`: a fine-grained personal access token limited to this
   repository with **Contents: write** and **Workflows: write**. The default `GITHUB_TOKEN`
   cannot push upstream commits that change `.github/workflows/*`. Without the secret the sync
   run fails with a message saying so.
3. Optional: disable upstream workflows the fork does not need, for example
   `gh workflow disable "SDK live tests"` and `gh workflow disable Cleanup`.

## When the sync run fails

GitHub emails you about failed scheduled runs. Open the run log, step "Merge upstream main".

**Merge conflict**: the merge was aborted, nothing was pushed. Locally:

    git fetch upstream --tags && git checkout main && git merge upstream/main
    # resolve the listed files, commit, push

**Outdated translations**: the merge was pushed; the run fails to remind you. The log lists
English templates that changed upstream. Update the matching file in
`src/static/templates/email/ru/`, then regenerate the manifest from the repository root:

    M=src/static/templates/email/ru/SOURCE_HASHES; awk '{print $2}' $M | xargs shasum -a 256 > $M.new && mv $M.new $M

Commit and push both changes. (Use `sha256sum` instead of `shasum -a 256` on Linux if you prefer.)
