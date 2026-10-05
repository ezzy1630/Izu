# izu

Built by Ezzy Rappeport.

izu is an original Rust version-control engine with private workspaces for people
and coding agents. The command line, JSON API and MCP server use the same engine.
The native history store works without Git, an account or a running service.
Git interoperability is an optional adapter around an explicitly selected Git
executable and transport.

**Development build:** this is an unpublished 0.1.0 build. Platform and workflow
claims depend on the recorded executable and cases. Passing component tests do
not establish end-to-end readiness; see the [development status](docs/status.md)
for recorded results and remaining work, and [validation](docs/validation.md)
for the verification procedure.

## Build

Use the Rust toolchain pinned in `rust-toolchain.toml`:

```sh
cargo build --locked --release -p izu-cli
./target/release/izu --help
./target/release/izu capabilities --json
```

Building also requires the platform's C compiler and linker for the TLS
cryptography dependency. Git is needed only when using Git interoperability;
native history operations do not launch Git.

The current implementation targets macOS and Linux. Native filesystem barriers,
process ownership and actual workflow tests determine support; a successful
cross-compilation alone does not. Windows is not currently supported.

## Source history

```sh
izu init my-project
cd my-project
izu status
izu checkpoint
izu commit -m "First change" --author-name "Your name" --author-email "you@example.com"
izu diff
izu log
```

A checkpoint preserves the working source without creating an intentional
commit. A commit records an intentional change. Selecting `--path` commits only
the selected source; other edits remain in the workspace. Author flags are
explicit, or can be supplied through `IZU_AUTHOR_NAME` and `IZU_AUTHOR_EMAIL`.

An operation is acknowledged as durable only after the store's persistence
barriers succeed. An error after a visible publication reports uncertainty and
identifies the state to inspect. Cancellation does not roll back an already
published operation.

## Private work and integration

Each managed change has its own source directory. Managed commands bind to its
exact workspace and source. Checkpoints, intentional revisions, references and
integration candidates have distinct roles; a passing check applies to its exact
candidate inputs. A stale target, unresolved conflict, missing check or failed
check prevents landing.

Landing advances a reference. The human checkout retains its files until a
guarded update. Unknown writer ownership prevents source replacement and
workspace disposal. Process groups and resource admission coordinate cooperating
commands; they are not an operating-system security sandbox.

The [CLI guide](docs/cli.md) covers managed launch, selective commits, conflicts,
checks, landing and recovery. The [JSON and MCP guide](docs/agent-api.md) documents
the same operations for agents. A host must invoke the managed launch contract
before starting a thread: installing an MCP server does not automatically change
another application's working directory.

## Recovery and interchange

The local store retains immutable objects and an operation history. Native
bundles preserve the full closure of a selected captured operation and can be
restored at a new location. A full bundle can include private recovery snapshots,
ignored files captured for recovery, historical paths and environment data. Treat
it as a private archive. A local checkpoint or bundle on the same device is not an
independent backup.

Git import and publication are explicit. Unsupported Git features fail instead
of being silently discarded. Publication checks the expected old reference and
reports what the selected transport actually confirmed. See
[Git interoperability](docs/git-interop.md) and the
[bundle format](docs/bundle-format.md).

## Contracts and verification

- [Native encoding and compatibility](docs/format.md)
- [Persistence, faults and recovery](docs/durability.md)
- [Dependency and unsafe boundaries](docs/dependencies.md)
- [Engine operations](docs/engine.md)
- [Diffs and merges](docs/merge.md)
- [Process ownership](docs/processes.md) and [runtime](docs/runtime.md)
- [Prepared environments](docs/environments.md)
- [Acceptance checks](docs/validation.md) and [benchmark method](docs/benchmark-method.md)
- [Contributing and required checks](CONTRIBUTING.md)
- [Build and distribution](docs/distribution.md)

The repository contains no borrowed VCS engine. General-purpose dependencies have
their own safety and foreign-function boundaries; safe Rust in izu does not imply
that every dependency contains no unsafe code. Measurements and supported
capabilities must be tied to the exact source, executable, platform and fixture.

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
See [NOTICE](NOTICE) for project attribution.
