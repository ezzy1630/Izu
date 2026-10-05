# Contributing

izu's version-control engine is original Rust code. Its native model, storage,
history operations, merge algorithms, runtime and interfaces are developed in
this workspace. General-purpose libraries may be used; another VCS engine is
not an implementation dependency.

Use the toolchain pinned in `rust-toolchain.toml` and the checked-in lockfile.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p izu-store -p izu-engine -p izu-bundle -p izu-runtime --features izu-store/fault-injection,izu-engine/fault-injection,izu-bundle/fault-injection,izu-runtime/fault-injection --locked
cargo clippy -p izu-store -p izu-engine -p izu-bundle -p izu-runtime --all-targets --features izu-store/fault-injection,izu-engine/fault-injection,izu-bundle/fault-injection,izu-runtime/fault-injection --locked -- -D warnings
cargo build -p izu-cli -p izu-lab --release --locked
```

Keep the CLI and agent interfaces thin. New history behavior belongs in the
shared engine and must have the same result through every interface.

Changes to a native format need a documented encoding, compatibility decision,
known vectors, malformed-input tests and an explicit migration or refusal path.
A Rust struct's layout does not define a persisted or exchanged format.

Data-loss, corruption and concurrency fixes need a reproducer. Test failure
before and after publication separately: an error after a visible update must
not tell the caller that nothing changed. Preserve the receipt needed for
recovery. A passing process-crash test is not physical power-loss evidence.

Measure a relevant workflow before optimizing it. Record the source and binary
identity, fixture, platform, resource limits and raw samples. Compare physical
allocation as well as logical file sizes. Recheck the affected behavior after
the change.

Do not commit credentials, generated repositories, local benchmark fixtures or
build outputs. Tests use disposable owned directories and local Git remotes.
They must not read personal Git configuration, send messages or contact a real
project remote.

Run `izu-lab` against the exact release binary and its SHA-256 digest for the
command-line workflow gate. `acceptance --scope cli-workflows` requires the real
CLI, JSON, MCP, Git, bundle and concurrent-worker cases. Its report retains the
separate crash and full-disk coverage requirements; passing that scope does not
mean full acceptance. Run the fault-injection suite separately and record real
full-disk evidence only from an owned, bounded filesystem. The CI workflow shows
the complete command. Set `TMPDIR` to owned test storage when placement matters.
