#Requires -Version 7
param(
  [Parameter(Mandatory)][ValidateSet('codex','glm','agy')][string]$Reviewer,
  [Parameter(Mandatory)][string]$PromptFile,   # absolute path
  [Parameter(Mandatory)][string]$OutFile,      # absolute path
  [string]$RepoDir = (Get-Location).Path,
  [int]$TimeoutSec = 480,
  [int]$AttemptsPerModel = 2
)
# Copied from the dual-review-loop skill and hardened for open_mess (changes marked "open_mess:");
# called by review-spec.sh and review-commit.sh.
# Watchdog runner for external reviewers (codex CLI / ZCode or opencode GLM / agy CLI).
# Exit codes: 0 = VERDICT APPROVE, 1 = VERDICT REJECT (findings in $OutFile),
#             2 = whole fallback chain dead (timeouts / no valid verdict).
#
# Resume-on-retry: attempt 1 of each chain link starts a fresh, PERSISTED
# session (codex: no --ephemeral, session id read from the exec banner in
# $OutFile; opencode: session found in opencode.db by a unique --title tag).
# Attempt 2+ resumes that session instead of re-reading the repo from zero.
# Switching to a fallback model always starts fresh (sessions are not
# portable across providers). agy has no resume here - it prints no session id
# in print mode, so every agy attempt starts fresh. Neither has ZCode (its
# session id is not printed in -p mode).

# open_mess: reviewers read untrusted diffs, so opencode gets read-only tools. Everything else is
# denied (bash, edit, write, webfetch, MCP tools); inherited by the cmd.exe and opencode children.
# opencode's built-in MCP resource readers ignore this and stay available, read-only.
$env:OPENCODE_PERMISSION = '{"*":"deny","read":"allow","grep":"allow","glob":"allow","list":"allow"}'

$rel = [IO.Path]::GetRelativePath($RepoDir, $PromptFile).Replace([char]92, [char]47)  # backslash -> fwd slash
$ocPrompt = "Wykonaj doslownie instrukcje z pliku @$rel. NIE uruchamiaj polecen terminala ani git - tylko czytaj pliki"
# no-terminal clause: agy auto-denies RunCommand in print mode (cannot prompt),
# so a review that reaches for the terminal dies with an empty out-file
$agyPrompt = "Przeczytaj plik $rel w tym katalogu i wykonaj doslownie jego instrukcje. NIE uruchamiaj zadnych polecen terminala - korzystaj tylko z narzedzi czytania i szukania w plikach"
# ZCode CLI: the app's bundled zcode.cjs run by its own Electron as Node (needs node:sqlite,
# which system Node 20 lacks). The bundle can't find its provider config outside the app's
# folder layout, so it is passed in. Plan mode plus no Bash/Edit/Write keeps it read-only
# (an opencode GLM once ran git checkout in the reviewed work tree).
$zcodeHome = Join-Path $env:LOCALAPPDATA 'Programs\ZCode'
$zcodePrompt = "Przeczytaj plik $rel w tym katalogu i wykonaj doslownie jego instrukcje. NIE uruchamiaj zadnych polecen terminala ani git - korzystaj tylko z narzedzi czytania i szukania w plikach"
$zcodeDenied = 'Bash,Edit,Write,TodoWrite,Agent,Memory,CronCreate,CronUpdate,CronDelete,OffPeakCreate,CreateWorkflow,AmendWorkflow,FillWorkflowHole'
$resumePrompt = 'Kontynuuj przerwana recenzje wg wczesniejszych instrukcji. Dokoncz analize. Ostatnia linia odpowiedzi to dokladnie VERDICT: APPROVE albo VERDICT: REJECT.'
$runTag = 'dualrev-' + [IO.Path]::GetFileNameWithoutExtension($OutFile) + '-' + $PID

function BuildLine([hashtable]$link, [string]$sessionId) {
  if ($link.kind -eq 'codex') {
    if ($sessionId) { return "call codex exec resume $sessionId --skip-git-repo-check `"$resumePrompt`" > `"$OutFile`" 2>&1" }
    return "call codex exec -s read-only --skip-git-repo-check - < `"$PromptFile`" > `"$OutFile`" 2>&1"
  }
  if ($link.kind -eq 'zcode') {
    return "set ELECTRON_RUN_AS_NODE=1`r`nset ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=$zcodeHome\resources\config\provider\zcode-builtin.json`r`n" +
      "`"$zcodeHome\ZCode.exe`" `"$zcodeHome\resources\glm\zcode.cjs`" --cwd `"$RepoDir`" --mode plan --disallowed-tools $zcodeDenied -p `"$zcodePrompt`" > `"$OutFile`" 2>&1"
  }
  if ($link.kind -eq 'agy') {
    # -p must come LAST with the prompt attached: a flag after -p is eaten as the prompt.
    # No --dangerously-skip-permissions: read-only allow-rules live in agy settings.json.
    return "call agy --model $($link.model) --print-timeout $($TimeoutSec)s -p `"$agyPrompt`" > `"$OutFile`" 2>&1"
  }
  # short inline prompt only - long inline prompt hangs opencode init
  if ($sessionId) { return "call opencode run -m $($link.model) -s $sessionId `"$resumePrompt`" > `"$OutFile`" 2>&1" }
  return "call opencode run -m $($link.model) --title `"$runTag`" `"$ocPrompt`" > `"$OutFile`" 2>&1"
}

function Get-ReviewSessionId([hashtable]$link) {
  # called right after a FAILED attempt, before $OutFile is cleared
  if ($link.kind -eq 'codex') {
    if (Test-Path $OutFile) {
      $m = [regex]::Match([string](Get-Content $OutFile -Raw), 'session id:\s*([0-9a-f][0-9a-f-]{34}[0-9a-f])')
      if ($m.Success) { return $m.Groups[1].Value }
    }
    return ''
  }
  if ($link.kind -in 'agy', 'zcode') { return '' }   # print mode prints no conversation id
  $py = @'
import sqlite3, os, sys
db = os.path.expanduser("~/.local/share/opencode/opencode.db")
try:
    con = sqlite3.connect("file:" + db.replace(os.sep, "/") + "?mode=ro", uri=True)
    r = con.execute(
        "SELECT id FROM session WHERE title LIKE ? ORDER BY time_created DESC LIMIT 1",
        (sys.argv[1] + "%",),
    ).fetchone()
    print(r[0] if r else "")
except Exception:
    print("")
'@
  $pyFile = "$OutFile.sid.py"
  Set-Content -Path $pyFile -Value $py -Encoding ASCII
  try {
    $sid = (& python $pyFile $runTag 2>$null | Select-Object -First 1)
  } finally {
    Remove-Item $pyFile -Force -ErrorAction SilentlyContinue
  }
  if ($sid -and $sid -match '^ses_[A-Za-z0-9]+$') { return $sid }
  return ''
}

# open_mess: Codex has no fallback to another provider. A GLM verdict filed as the Codex review would
# let a spec or commit pass without the Codex review AGENTS.md requires; a dead Codex is exit 2.
$chain = if ($Reviewer -eq 'codex') {
  @(
    @{ name = 'codex(default model)';          kind = 'codex' }
  )
} elseif ($Reviewer -eq 'agy') {
  @(
    @{ name = 'agy/gemini-3.8-flash-high';     kind = 'agy'; model = 'gemini-3.8-flash-high' },
    @{ name = 'agy/gemini-3.1-pro-high';       kind = 'agy'; model = 'gemini-3.1-pro-high' },
    @{ name = 'zai-coding-plan/glm-5.3';       kind = 'oc'; model = 'zai-coding-plan/glm-5.3' }
  )
} else {
  @(
    # No ollama-served GLM (user's choice, 2026-10-15). ZCode's CLI only reaches GLM through ollama
    # here (no Z.ai key in ZCode), so its link is out too; kind 'zcode' stays for a Z.ai-logged ZCode.
    @{ name = 'zai-coding-plan/glm-5.3';       kind = 'oc'; model = 'zai-coding-plan/glm-5.3' },
    @{ name = 'zai-coding-plan/glm-5.3-flash'; kind = 'oc'; model = 'zai-coding-plan/glm-5.3-flash' }
  )
}

$cmdFile = "$OutFile.cmd"
foreach ($link in $chain) {
  $sessionId = ''   # resume only within the same link; fallback model starts fresh
  for ($i = 1; $i -le $AttemptsPerModel; $i++) {
    # temp .cmd: only PATHS interpolated (no quotes/metachars from content); 'call' hits .cmd shims on PATH
    Set-Content -Path $cmdFile -Value "@echo off`r`nset NO_COLOR=1`r`ncd /d `"$RepoDir`"`r`n$(BuildLine $link $sessionId)`r`n" -Encoding ASCII
    Remove-Item $OutFile -Force -ErrorAction SilentlyContinue
    $mode = if ($sessionId) { "resume $sessionId" } else { 'fresh' }
    $p = Start-Process cmd.exe -ArgumentList '/c', "`"$cmdFile`"" -WindowStyle Hidden -PassThru
    if (-not $p.WaitForExit($TimeoutSec * 1000)) {
      taskkill /PID $p.Id /T /F 2>$null | Out-Null   # /T = kill tree (cmd -> cli -> node)
      $sid = Get-ReviewSessionId $link
      if ($sid) { $sessionId = $sid }
      Write-Host "${Reviewer}: $($link.name) attempt $i ($mode) TIMEOUT (${TimeoutSec}s)$(if ($sessionId) { ", will resume $sessionId" })"
      continue
    }
    $txt = if (Test-Path $OutFile) { [string](Get-Content $OutFile -Raw) } else { '' }
    $txt = [regex]::Replace($txt, "`e\[[0-9;?]*[A-Za-z]", '')   # strip ANSI - opencode colors break line-anchored match
    $m = [regex]::Matches($txt, '(?m)^\s*VERDICT:\s*(APPROVE|REJECT)\s*$')
    if ($m.Count -gt 0) {
      $v = $m[$m.Count - 1].Groups[1].Value   # last match wins (prompt-echo defense)
      Remove-Item $cmdFile -Force -ErrorAction SilentlyContinue
      Write-Host "${Reviewer}: $($link.name) attempt $i ($mode) VERDICT $v"
      exit ($v -eq 'APPROVE' ? 0 : 1)
    }
    $sid = Get-ReviewSessionId $link
    if ($sid) { $sessionId = $sid }
    Write-Host "${Reviewer}: $($link.name) attempt $i ($mode) invalid output ($($txt.Trim().Length) chars, no VERDICT)"
  }
}
Remove-Item $cmdFile -Force -ErrorAction SilentlyContinue
Write-Host "${Reviewer}: chain exhausted - reviewer dead"
exit 2
