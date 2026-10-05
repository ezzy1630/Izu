# Build and distribution

Build the command line from the complete source workspace with the pinned Rust
toolchain and lockfile:

```sh
cargo build --locked --release -p izu-cli
./target/release/izu --version
./target/release/izu capabilities --json
```

A C compiler and linker are needed for Ring, the TLS cryptography dependency.
Native history needs no Git installation or account. Git exchange additionally
requires an explicitly selected Git executable or supported transport.

The current platform scope is macOS and Linux. Match a binary's operating system,
CPU architecture and recorded library requirements to the destination. A Linux
container build does not establish compatibility with every distribution or
libc version. Build from source when no matching validated binary is available.
Windows native persistence and process ownership are unsupported.

Before distributing a binary, run the [required checks](../CONTRIBUTING.md) and
the [CLI acceptance gate](validation.md) against its exact SHA-256 digest. Keep
the frozen build source, lockfile, compiler identity, command, dependency audit,
raw results and any excluded coverage with it. Do not substitute a later rebuild
without validating that executable. Fault-injection features are for development
only; the store rejects them in a release build.

A local package should contain the executable, `LICENSE-APACHE`, the project
attribution in `NOTICE`, dependency license notices, a source/build manifest and
file checksums. Dependencies retain their own licenses. Include the source
workspace or a separately identified source archive. An archive checksum verifies
bytes; it is not a publisher signature or a notarization. Local validation does
not publish a release, sign a binary or configure a host's agent integration.

Native format and exchange compatibility are documented in
[format](format.md), [bundles](bundle-format.md) and
[Git interoperability](git-interop.md). Unknown native versions are refused;
there is no automatic migration from the earlier EZY prototype. Preserve old
data and its matching reader. Ordinary workspace close retains source and
history. This version has no history garbage collection or destructive workspace
removal command.

For agent hosts, configure the documented [managed launch contract](agent-api.md)
before starting work in a private directory. MCP discovery alone cannot bind
another application's thread or change its working directory. Resource admission
and owned process groups coordinate cooperative jobs; they are not a security
sandbox for untrusted programs.
