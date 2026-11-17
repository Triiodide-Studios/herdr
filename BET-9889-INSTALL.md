herdr with the session-record restore (BET-9889), built 2026-11-11, NOT INSTALLED. Nothing is restarted or replaced by this build existing.

What:      a restored Claude tab resumes at the model and effort its session last ran at, read from Luna's per-session record (<state>\herdr\plugins\triiodide.luna\effort\<session id>.json,
           {"model": "...", "effort": "..."}, written by the Claude mod on every request that moved: /model, /effort, a restore), in place of the --model/--effort in session.json launch_args,
           which are read off the command line once and never follow a /model or /effort. The record's model and effort replace the launch ones together, never mixed; --permission-mode and
           the switch flags stay as launched; a missing, half-written or unsafe record, a non-UUID id, and every other agent leave the launch flags as they are. The Opus clamp (BET-9805:
           Opus at xhigh/max resumes at medium) still applies after it, so a record that says Opus xhigh also comes back at medium. A claude found by the detector with no hook report
           (terminal/state.rs record_detected_launch) is planned the same way.
Source:    C:\Users\BlakeThomas\herdr-wt\resume-record-bet9889, branch triiodide/resume-record-bet9889 = triiodide/opus-effort-clamp (6eca1a0d, which is 447a6f0a + the clamp, the build in
           Programs\Herdr\restore-agents now, plus one commit) + the BET-9889 commits. Files: src/agent_resume.rs, src/persist/restore.rs, src/terminal/state.rs,
           triiodide-claude-record-proof.ps1.
Exe:       T:\Investigations\BET-9889\herdr-record\herdr.exe   (28,678,144 bytes)
SHA-256:   2503A67333C8B31ADD4C2338718BED9D40135F2BB3B2B8EAB72D9CBDFE733893
Build:     ZIG=C:\Users\BlakeThomas\Tools\zig-x86_64-windows-0.16.0\zig.exe cargo build --release --bin herdr, under ue-broker run (6 min 9 s cold); the target dir was deleted afterwards.
Tests:     cargo test --bin herdr -- agent_resume:: restore_plan persist::snapshot terminal::state. New: agent_resume::tests::a_claude_session_record_replaces_the_launch_model_and_effort_and_keeps_the_rest,
           persist::restore::tests::restore_plan_follows_the_session_record_through_a_change_made_after_launch (enters through restore_plan_for_snapshot and round-trips Opus/xhigh launch ->
           Fable/low -> Sonnet/xhigh -> Opus/xhigh clamped to medium -> half-written -> budget record).
Proof:     triiodide-claude-record-proof.ps1 -Exe <herdr.exe> on a throwaway server (scratch APPDATA and XDG_STATE_HOME, cmd.exe panes, a stand-in claude.exe that sleeps; it fails if any claude.exe
           under the server is not the stand-in). Tab 1 launched Opus xhigh and moved to Fable low, tab 2 launched Opus xhigh, tab 3 moved from xhigh to high with --permission-mode auto:
           this build: all 6 checks PASS (tab 1 comes back "--model claude-fable-5-2 --effort low", then Sonnet xhigh after a second change; tab 2 medium; tab 3 high with its permission mode)
           (logs\proof-green.log). The clamp-only build (T:\Investigations\BET-9805\herdr-clamp\herdr.exe): tab 1 comes back Opus medium, tab 3 xhigh: 4 of 6 FAIL (logs\proof-red-clamp-only-build.log).
Live now:  Programs\Herdr\restore-agents\herdr.exe is 447a6f0a, SHA-256 D9F019A153A044D1C862C612D5287CC823E044CBE34575460A3BBEC65FCD7DFA (checked 11/11 19:55). The running server (pid 38724, started
           16:41) is an older image and stays running until the next herdr restart. This build contains the BET-9805 clamp: install this one, not T:\Investigations\BET-9805\herdr-clamp.

INSTALL (Blake times it, together with the broker policy bump 33598; do not do it before):
  1. Rollback copy first:  Copy-Item $env:LOCALAPPDATA\Programs\Herdr\restore-agents\herdr.exe  $env:LOCALAPPDATA\Programs\Herdr\restore-agents\herdr.exe.before-record-bet9889
     (check its SHA-256 is D9F019A1... first; if it is not, someone installed another build after 11/11 19:55 and this one must be rebased on it: `git rebase --onto <new> 6eca1a0d` on
     triiodide/resume-record-bet9889, then build and run the proof again.)
  2. Copy-Item T:\Investigations\BET-9889\herdr-record\herdr.exe  $env:LOCALAPPDATA\Programs\Herdr\restore-agents\herdr.exe -Force
     (check the copy's SHA-256 is 2503A673...). If Windows refuses (a hook or plugin runs herdr as HERDR_BIN_PATH, which is this file), retry; do not stop the server.
  3. It takes effect at the next herdr restart: Start menu "Restart BET Herdr" (Programs\Herdr\Restart-BET-Herdr.ps1 -Detached -NoSsh), outside herdr. Until then hooks and plugins run the new exe
     as a client of the old server; the change touches only the resume command a restore builds.
  4. After the restart, every Claude tab's claude.exe command line (bet ps --name '^claude' --json) should carry the --model and --effort in its Luna record, and no Opus at xhigh or max.
ROLLBACK:  Copy-Item $env:LOCALAPPDATA\Programs\Herdr\restore-agents\herdr.exe.before-record-bet9889  $env:LOCALAPPDATA\Programs\Herdr\restore-agents\herdr.exe -Force  (then restart herdr to go back).
           After the first restart on this build, delete herdr.exe.before-record-bet9889 once the restart has been looked at.
Not touched: session.json (its launch_args stay as recorded; the restore reads the record over them), the running herdr, herdr's config, ~/.claude settings, Luna.
Known edges: a record older than a launch with new flags and no request since wins, until the mod rewrites it on the first request; a Claude session with no record resumes as launched; a hand-started
             tab whose hooks never report is planned once, when detected.
