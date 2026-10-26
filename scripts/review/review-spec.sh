#!/bin/sh
# Reviews one spec with Codex and GLM in parallel and waits for both verdicts.
# Usage: scripts/review/review-spec.sh .specs/<slug>.md [round]
# Output: .specs/review/<slug>-<reviewer>-r<round>.txt (gitignored).
# Exit: 0 both APPROVE, 1 any REJECT, 2 a reviewer is dead (whole fallback chain failed).
# Needs Windows, pwsh 7, the codex CLI and opencode with zai-coding-plan (CONTRIBUTING.md, "Reviews").

spec=${1:?usage: review-spec.sh .specs/<slug>.md [round]}
round=${2:-1}
root=$(git rev-parse --show-toplevel)
slug=$(basename "$spec" .md)
dir="$root/.specs/review"
mkdir -p "$dir"
prompt="$dir/$slug-prompt.md"
sed "s#@SPEC@#$spec#g" "$root/scripts/review/prompts/spec.md" > "$prompt"

win() { cygpath -w "$1"; }

# Plain background jobs of this shell: GLM through opencode hangs when started from a detached pwsh.
review() {
  pwsh -NoProfile -File "$root/scripts/review/run-reviewer.ps1" -Reviewer "$1" -PromptFile "$(win "$prompt")" \
    -OutFile "$(win "$dir/$slug-$1-r$round.txt")" -RepoDir "$(win "$root")"
}
review codex & codex_pid=$!
review glm & glm_pid=$!
wait $codex_pid; codex_exit=$?
wait $glm_pid; glm_exit=$?

verdict() { case $1 in 0) echo APPROVE ;; 1) echo REJECT ;; *) echo DEAD ;; esac; }
echo "codex: $(verdict $codex_exit)  -> $dir/$slug-codex-r$round.txt"
echo "glm:   $(verdict $glm_exit)  -> $dir/$slug-glm-r$round.txt"

if [ $codex_exit -ge 2 ] || [ $glm_exit -ge 2 ]; then exit 2; fi
if [ $codex_exit -ne 0 ] || [ $glm_exit -ne 0 ]; then exit 1; fi
exit 0
