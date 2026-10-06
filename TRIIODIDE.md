# Triiodide herdr patches

Stock [herdr](https://github.com/herdrdev/herdr) plus client-only changes to the agents panel. The branch
`triiodide/agent-context-menu` sits on the upstream release tag it was built from (now `v0.9.3`).

- Right-click an agent row: Pin / Unpin, Rename, Clear name (after a rename), Close.
  Pinned agents stay at the top, in pin order, under every sort and plugin view, with a 📌 on the row.
  Pins are saved per machine in the window's own preferences file.
- A name given with Rename outranks names that integrations and plugins report.
- Right-click open space in the agents panel: New chat, which opens a focused tab in the focused
  workspace and runs `claude` (`NEW_CHAT_COMMAND` in `src/client/shell_runtime.rs`).

Only the window (the client) changes. The server can stay the stock release of the same version, so
installing the patched binary never restarts running panes: reopen the window to pick it up.
Pins and New chat apply to the local machine only, not to remote machines added with `herdr machine add`.

## Build

Needs Rust and Zig 0.16.0 (herdr's vendored libghostty-vt). Set `ZIG` if `zig` is not on PATH.

```sh
cargo build --release --bin herdr
cargo test --release --bin herdr client::shell
```

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
