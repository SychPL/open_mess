#!/bin/sh
# Sends one commit to the Codex and GLM reviewers in the background. Never blocks or fails the commit.
# Called by .githooks/post-commit; run by hand to review an older commit: scripts/review/review-commit.sh <sha>
# Output in .reviews/ (gitignored): <sha>-codex.txt and <sha>-glm.txt end with a VERDICT line,
# <sha>-<reviewer>.log ends with exit=0 (APPROVE), exit=1 (REJECT) or exit=2 (reviewer dead).
# Reconcile every verdict before pushing: fix or answer each P1/P2, then write <sha>.processed.

root=$(git rev-parse --show-toplevel)
sha=$(git -C "$root" rev-parse --short "${1:-HEAD}")
dir="$root/.reviews"

for tool in pwsh cygpath codex opencode; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "open_mess review: $tool not found, commit $sha was not sent for review (CONTRIBUTING.md, Reviews)" >&2
    exit 0
  fi
done

mkdir -p "$dir"
# Git exports GIT_INDEX_FILE (and sometimes GIT_DIR) to hooks. Reviewers must not inherit them:
# opencode's snapshots would write into this checkout's index and corrupt it.
unset GIT_INDEX_FILE GIT_DIR GIT_WORK_TREE GIT_PREFIX GIT_COMMON_DIR
# Already queued: amend and rebase replays don't review twice. Delete <sha>.patch to re-run.
[ -e "$dir/$sha.patch" ] && exit 0
git -C "$root" show --stat --patch "$sha" > "$dir/$sha.patch"
sed "s#@SHA@#$sha#g" "$root/scripts/review/prompts/commit.md" > "$dir/$sha-prompt.md"

for reviewer in codex glm; do
  (
    pwsh -NoProfile -File "$root/scripts/review/run-reviewer.ps1" -Reviewer "$reviewer" \
      -PromptFile "$(cygpath -w "$dir/$sha-prompt.md")" -OutFile "$(cygpath -w "$dir/$sha-$reviewer.txt")" \
      -RepoDir "$(cygpath -w "$root")"
    echo "exit=$?"
  ) < /dev/null > "$dir/$sha-$reviewer.log" 2>&1 &
done
exit 0
