# bashward

checkpoint and rewind for **bash side-effects** in claude code. fills the gap claude code's own `/rewind` does not cover.

[![crates.io](https://img.shields.io/crates/v/bashward.svg)](https://crates.io/crates/bashward)
[![license](https://img.shields.io/crates/l/bashward.svg)](#license)

## the gap

claude code's [`/rewind`](https://code.claude.com/docs/en/checkpointing) restores edits made through file-editing tools. it does **not** restore changes made by `Bash`. anthropic's own docs:

> Bash commands are not tracked. If Claude runs `rm`, `mv`, or `cp`, those changes are permanent.

so when claude runs a bash one-liner that nukes your build artifacts, your work-in-progress file, your env script, your migration that was about to run, `/rewind` cannot help. there are people on dev.to and HN with stories of "claude lost my 4-hour session" that all trace back to this.

bashward is a tiny rust binary that closes the loop. it hooks into claude code's `PreToolUse` event for the `Bash` tool, parses the command, identifies write paths (`rm`, `mv`, `cp`, `dd`, `sed -i`, `>` and `>>` redirections, `truncate`, `tee`), and snapshots them via APFS clonefile *before* the command runs. each transaction is logged. when something goes wrong:

```sh
bashward list
000018ac  2026-05-03 20:23:44  prebash   1 paths  `rm /Users/me/important.txt`

bashward rewind 000018ac
rewinding transaction 000018ac1786 (1 path(s))
  restored /Users/me/important.txt
```

## install

```sh
cargo install bashward
bashward install                   # writes the PreToolUse hook into ~/.claude/settings.json
```

remove with `bashward uninstall`. it is idempotent in both directions.

## what it does, exactly

| user action                      | bashward action                                           |
|----------------------------------|-----------------------------------------------------------|
| claude runs `rm -rf foo/`        | snapshot `foo/` first, then let the rm run                |
| claude runs `mv a.txt b.txt`     | snapshot both, then let the mv run                        |
| claude runs `dd of=/tmp/disk`    | snapshot `/tmp/disk` first                                |
| claude runs `sed -i 's/x/y/' f`  | snapshot `f` first                                        |
| claude runs `echo > /etc/...`    | snapshot the redirection target first                     |
| claude runs `ls`, `git status`   | nothing, the heuristic catches no write paths             |
| `bashward snap path1 path2`      | take a manual snapshot whenever you want                  |
| `bashward rewind <id>`           | restore one transaction (full or short prefix)            |

storage layout:

```
~/.bashward/
  log.jsonl              one transaction per line
  snaps/<id>/<path>      one snapshot tree per transaction
```

snapshots use APFS `clonefile` on macos (`/bin/cp -c -R`), so a snapshot of a 1 GB directory takes a few milliseconds and roughly zero disk space until something diverges. on linux the v0.1 fallback is `cp -a`, which is correct but not space-efficient; overlayfs and btrfs snapshot paths are planned.

## hook payload

claude code passes a json payload describing the tool call on stdin. bashward reads it, picks out `tool_input.command`, runs the heuristic, snapshots, and emits `{"continue": true}` so the bash call proceeds without further delay. it never blocks the call, so the hook is observe-and-pass-through.

## commands

| command                    | description                                                                    |
|----------------------------|--------------------------------------------------------------------------------|
| `bashward install`         | write PreToolUse hook into `~/.claude/settings.json` (idempotent)              |
| `bashward uninstall`       | remove the hook                                                                |
| `bashward snap <paths..>`  | manual snapshot; useful before risky one-offs                                  |
| `bashward list`            | list transactions, newest at the bottom, with command preview                  |
| `bashward rewind <id>`     | restore a transaction by full id or unique short prefix                        |
| `bashward prune --days 7`  | drop snapshots older than N days                                               |
| `bashward doctor`          | print resolved paths, OS, snapshot method, transaction count                   |

## things to know

- the heuristic is "good enough", not formal command parsing. it errs on the side of snapshotting; if a command writes somewhere bashward doesn't recognize, you keep no protection. open an issue with the command pattern.
- snapshots persist forever by default. `bashward prune --days 7` cleans them.
- v0.1 is macos-first. linux works but uses a non-deduplicating `cp -a`. expect `overlayfs`/`btrfs` paths in 0.2.
- bashward never modifies your bash command. the hook always returns `continue`. think of it as a safety net, not a sandbox.

## related crates

other small rust pieces shipped alongside this one:

- [`skill-scan`](https://github.com/f4rkh4d/skill-scan) local prompt-injection scanner for claude skills, MCP, AGENTS.md
- [`pluvgo`](https://github.com/f4rkh4d/pluvgo) fast neovim plugin manager, single rust binary, no neovim required to install
- [`mlkem-rs`](https://github.com/f4rkh4d/mlkem-rs) FIPS 203 ML-KEM (post-quantum kem) in pure rust
- [`mlkem-tls`](https://github.com/f4rkh4d/mlkem-tls) X25519MLKEM768/1024 hybrid kem per draft-ietf-tls-ecdhe-mlkem
- [`skl`](https://github.com/f4rkh4d/skl) package manager for AI agent skills

## license

dual-licensed under MIT or Apache-2.0, at your option.
