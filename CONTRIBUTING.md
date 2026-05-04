# contributing to bashward

short version: open a PR, keep it focused, run `cargo fmt` and
`cargo clippy --release -- -D warnings` before pushing.

## getting set up

```sh
git clone https://github.com/f4rkh4d/bashward
cd bashward
cargo build --release
cargo test --release
```

stable rust toolchain, MSRV is pinned in `Cargo.toml`.

## what's a good PR

- one logical change per PR. if you find two unrelated bugs, two PRs.
- includes a test that fails before the change and passes after.
- runs `cargo fmt` so the diff is just the change, not whitespace.
- runs `cargo clippy --release --all-targets -- -D warnings` clean.
- updates the changelog if user-visible.

## what's a good first PR

- a typo or broken link in the README.
- an example program in `examples/` that exercises one specific shape
  of the api.
- a test that covers an edge case the existing suite misses.
- a benchmark that compares against the audited reference (where one
  exists) on your machine.

## what to skip without asking first

- algorithmic changes to anything inside `src/`. open an issue first
  so we can talk about correctness implications.
- adding a dep. small focused crates only; ask first if you think a
  new transitive will help.
- formatting-only PRs across the codebase; rustfmt does that on its
  own per-commit.

## reporting a security issue

email **hello@frkhd.com** with subject `bashward security`. coordinated
disclosure preferred. please do not file a public github issue for
security findings.

## license

by contributing you agree that your contribution is licensed under the
same dual MIT + Apache-2.0 license as the rest of the crate.
