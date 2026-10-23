# Native Question Picker Captures

Captured from real terminal UIs on 2026-10-23, using `herdr agent read <pane> --source detection --format text`.
These are live-smoke evidence, not synthetic screens classified against bundled rules in unit tests.

`codex-collapsed.txt`: Codex CLI 0.163.0, GPT-6.1 Sol xhigh, native `request_user_input_async` in question-parity-sol
(w1:p65). The pending Land it / Hold question was initially `working`; the deployed 2026.10.11.1 manifest had no
matching blocker. The question count and `shift+← to answer` action can occupy separate lines. With the new
manifest hot-reloaded, `agent explain` reported `collapsed_question`, `visible_blocker: true`, `state: blocked`,
and `agent wait --any-of blocked` plus `agent get` confirmed the runtime status. No server restart occurred.

`muse-picker.txt`: Muse Code 1.4.12 (R6909.1), muse-wonder-full-1.0 xhigh, in the disposable named session
`question-parity-probe`. `muse-proof.ps1` scoped the prepared Luna native Stop adapter to that test session.
The agent first ended with the plain Blue/Green question, then opened `request_user_input` after the hook
continued its turn. The existing 2026.08.26.1 manifest matched `pick_request_blocked`, `visible_blocker: true`,
`state: blocked`; `agent get` independently confirmed blocked. No Muse picker rule change was needed.

The source and distribution copies retain the trust-directory, update-picker and idle-title fixes already deployed
in 2026.10.11.1. The new version adds only the collapsed-question rule to that deployed baseline. A pending async
question must outrank a concurrent working title, so its rule has higher priority than the working-title rule.

To repeat: ask a real native question, capture the detection buffer and `agent explain`, install a temporary
manifest override only if the override path is unoccupied, reload with `herdr server reload-agent-manifests`,
and confirm both the explain result and the authoritative `agent get` status. Answer the test question and
check the tab leaves blocked. Restore the prior override state after the test.
