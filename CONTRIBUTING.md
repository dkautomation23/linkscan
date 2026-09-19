# Contributing

## Build and test

```bash
cargo build --release
cargo test
```

CI runs the same steps against a pinned toolchain and fails on any warning:

```bash
rustup toolchain install 1.98.0 --profile minimal --component clippy
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Run `cargo clippy --all-targets -- -D warnings` locally before opening a PR.
`Cargo.lock` is committed and CI builds with `--locked`; add dependencies
with `cargo add` rather than hand-editing the lockfile. The 15 tests run with
no network access — if a test you're adding needs a real HTTP response, mock
it rather than reaching out to a live site (see the existing tests in
`src/check.rs` for the pattern).

## Where tests live

There is no `tests/` directory — `src/check.rs` (the broken /
could-not-verify / redirected classification) and `src/extract.rs` (pulling
links out of HTML) each carry their own `#[cfg(test)] mod tests` block. Put a
new test next to the code it exercises.

## Adding a new classification rule

Every rule about what counts as broken, unverifiable, or a same-domain
redirect starts with a failing test in `src/check.rs` that encodes the exact
response (status code, header, redirect chain) it should handle — before any
classification code changes. The `crates.io` case in the README (a bot-filter
404 that isn't a dead link) is the model: reproduce the response shape as a
test first, then fix the classifier to pass it.

## Commit style

Match the existing log (`git log --oneline`): `Area: what changed`, lower
case after the colon, imperative, no trailing period, no conventional-commit
prefixes. Examples from this repository:

```
Run the tests in CI on every push
README: state the Rust version CI actually proves
Release workflow: build a binary for Linux, macOS and Windows on a tag
```

## Pull requests

If a change affects what linkscan calls broken vs. unverifiable, update the
"Honest limits" section of the README in the same PR. Small, focused PRs;
say what site or fixture you tested against.
