# Triiodide herdr patches

Stock [herdr](https://github.com/herdrdev/herdr) plus changes to the sidebar (the window) and to the server (the
`waiting` status and agent prompts). The branch
`triiodide/agent-context-menu` sits on the upstream release tag it was built from (now `v0.9.3`).

- Right-click an agent row: Pin / Unpin, Rename, Clear name (after a rename), Copy chat ID (the agent's
  session id, e.g. what `claude --resume` takes; local machine only), Close.
  Pinned agents stay at the top, in pin order, under every sort and plugin view, with a 📌 on the row.
  Pins are saved per machine in the window's own preferences file.
- A name given with Rename outranks names that integrations and plugins report.
- Right-click open space in the agents panel: New chat, which opens a focused tab in the focused
  workspace and runs `claude` (`NEW_CHAT_COMMAND` in `src/client/shell_runtime.rs`).

- The spaces section can be dragged down to 4 rows (heading, one space, `new` / `menu`) at any window
  height; it used to stop at 10% of the sidebar.
- Chat search, in a "search" section of its own between the spaces and the agents. Click its box and
  type: the agents panel lists only the chats that match, best match first, and shows `2 of 41` in
  place of its sort label; the spaces list is untouched. Up / Down / Tab move the highlight, Enter or a
  click on a row goes to the chat (and scrolls the full list to it), Esc or the `×` clears the search.
  Clicking elsewhere gives the keyboard back and keeps the filter. The box is an inset, darker than the
  sidebar (lighter on a light theme; black and white on the 16-colour `terminal` theme), and its text
  is the theme's own pushed toward black or white until it reaches 7:1 on that fill (4.5:1 for the
  placeholder); focus shows on the `/` prompt, not on the fill. A window too short to leave the agents
  panel 5 rows of chats (under 11 rows below the spaces) has no search section.
  A chat is found by its names, its title and its keywords, never by its conversation: the keywords are
  the 24 terms that set the chat's Claude Code transcript apart from the other chats' (TF-IDF over the
  user's and the assistant's text), counted from the transcript without its newest 8 messages. Each
  word is embedded with the static Model2Vec model `minishlab/potion-retrieval-32M`, so a query word
  matches a chat word it is close to in meaning as well as one it is the start of. A background thread
  in the window refreshes the index every 20 s (`src/client/chat_search/`); a refresh with nothing new
  reads only the bytes appended to each transcript.
- A `waiting` agent status, for an agent that ended its turn waiting on background work it started (a job, a
  scheduled wake-up, a reply from another session). `herdr agent list`, `agent get`, the dot (peach), the
  position (between working and idle) and the status text all show it. Nothing reports it by itself: a detection
  manifest rule says `state = "waiting"` (Luna's rules do, in `herdr-config\plugins\luna\manifest.go`), and
  `herdr status server --json` has `agent_status_waiting: true` once the server knows it. A server without it
  rejects such a manifest, so a rule writer checks the capability first. The variant is last in the enums, so
  saved sessions and older clients keep decoding the statuses they know.
- A waiting floor: `herdr pane report-agent <pane> --source ID --agent LABEL --state waiting` says an agent
  waits on something its screen cannot show (a question for the user filed through Luna's ask tool, a plain-text
  question its rollout records, a session timer). It is not lifecycle authority and carries no session: any source
  may report it, including for Claude and Codex panes whose native integrations are session-only, and it only ever
  lifts a pane herdr reads idle (shown done when unseen) to `waiting` - working, blocked and unknown are never
  overridden. The same source's `pane release-agent` drops it, and it lapses 45 s after the last report, so a
  reporter that dies cannot hold a pane at waiting. Luna reports it for every question tab (its inbox list) and
  for a Claude chat between fires of a recurring cron, so a tab is always working, waiting on the user (or its
  timer), or closed - never done at rest. `PaneAgentState::Waiting` is last, like the status.

- `herdr agent prompt` (and `herdr broadcast`, Luna, bet's land wake, the ledger monitors: everything that prompts an agent) sends a
  Claude Code agent a cross-session message, the channel the SendMessage tool uses (`src/claude_peer.rs`: the session's record
  and key in `~/.claude/sessions`, its message pipe). It arrives as "Another Claude session sent a message" and never touches the
  input box, so it cannot land in what the user is typing and submit it; a blocked agent still queues it. A Claude session that
  cannot take a message (no live record or key, a busy pipe) is an error (`agent_prompt_failed`), never typed. Every other agent is
  typed to, and not in the pane the user is working in (`agent_input_busy`; whether its box holds text is not known). Windows only;
  other platforms still type. The receiver treats the message as a teammate's, not the user's own words.

Pins, New chat and chat search change only the window (the client): reopen it to pick them up. The `waiting`
status and the agent prompts also change the server, and a server only takes a new binary by restarting, which
restarts every pane (live handoff is not available on Windows). Pins, New chat and chat search apply to the local machine only, not
to remote machines added with `herdr machine add`.

## Search model

The model is not in the repository. Without it the search still matches letters (and says "letters only"
beside the search heading). Put its three files in `%LOCALAPPDATA%\herdr\models\potion-retrieval-32M`
(129 MB; `potion-base-8M` is 30 MB but retrieves worse; to use it change `MODEL_DIR_NAME` in
`src/client/chat_search/embed.rs`):

```powershell
$dir = "$env:LOCALAPPDATA\herdr\models\potion-retrieval-32M"
New-Item -ItemType Directory -Force $dir | Out-Null
foreach ($file in 'config.json', 'tokenizer.json', 'model.safetensors') {
    curl.exe -fL -o "$dir\$file" "https://huggingface.co/minishlab/potion-retrieval-32M/resolve/main/$file"
}
```

SHA-256, as installed: `model.safetensors` 07609e5bd33aad37900b3fd62f4ec96f6daec88ca4d46b9d8b928bfababf6ea0,
`tokenizer.json` 7d75cbc54318138807c401b0f0c9721117c628b39de8e8e0edb6cb17e0ee7d18, `config.json`
63c00d90824c832c04ec1d02b6a983fb90489bf049f29fbff15ba481b8a432ee.

Related words score 0.3 to 0.7 cosine with this model (`lag`/`lagging` 0.56, `graphics`/`gpu` 0.52,
`sound`/`acoustic` 0.30) and unrelated ones reach 0.25 to 0.30, so loosely related topics ("sound" for an
audio chat) are not found. The thresholds are the constants at the top of `src/client/chat_search/index.rs`.
To see how they rank your own chats (reads the running server, the transcripts and the model):
`CHAT_SEARCH_QUERIES="sunlight;ps5 performance" cargo test --release --bin herdr real_data -- --ignored --nocapture`.

## Build

Needs Rust and Zig 0.16.0 (herdr's vendored libghostty-vt). Set `ZIG` if `zig` is not on PATH.

```sh
cargo build --release --bin herdr
```

Run the tests with `scripts\run-tests-isolated.ps1 [filter]` (PowerShell), not `cargo test`. Upstream runs
them with `cargo nextest` (`just test`), one process per test, because several tests set
`XDG_CONFIG_HOME` / `XDG_STATE_HOME` / `HOME` themselves. As threads of one `cargo test` process they race:
one run failed 18 tests in a poisoned-lock cascade, and in a release build the fallback is the real
`%APPDATA%\herdr`, where two full `cargo test` runs left fixtures in
`%APPDATA%\herdr\agent-detection\codex.toml` (a valid one, and the invalid `id = ` that
`invalid_local_override_falls_back_to_cached_remote_manifest` writes), which shadow the real Codex
manifest. Pointing only the two XDG variables at scratch folders did not stop it. The script runs nextest
with `APPDATA`, `LOCALAPPDATA`, `USERPROFILE`, `HOME` and both XDG variables redirected to a scratch folder
it deletes afterwards (`CARGO_HOME` and `RUSTUP_HOME` are pinned first so the toolchain still resolves);
three full runs passed 3311 of 3311 and touched nothing real. It also unsets
`NoDefaultCurrentDirectoryInExePath`, which makes
`windows_plugin_pane_commands_resolve_from_plugin_root_with_cwd_override` fail. Under memory pressure
`ue-broker` may stop the build ("below 3.0 GB"); retry with fewer jobs when other work has finished.

## Install

Copy `target/release/herdr` (`herdr.exe` on Windows) beside the stock install, and have whatever
opens the herdr window run it. Keep starting the server with the stock binary.
On Windows, rename a running copy aside (`herdr.exe.inuse-<date>`) before replacing it.

## After a herdr update

The patched client must match the server's version. Rebase onto the new tag and rebuild:

```sh
git fetch --tags upstream
git rebase --onto vX.Y.Z v0.9.3 triiodide/agent-context-menu
```

Update the tag named at the top of this file, then push the branch and the tag.
