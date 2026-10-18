# izu

Built by Ezzy Rappeport.

izu is an original Rust version-control engine with private workspaces for people
and coding agents. The command line, JSON API and MCP server use the same engine.
The native history store works without Git, an account or a running service.
Git interoperability is an optional adapter around an explicitly selected Git
executable and transport.

| Workflow | Git alone | izu |
| --- | --- | --- |
| Parallel work | Separate worktrees | Private workspaces with managed jobs, queues and concurrency limits |
| Saving unfinished work | Explicit stash or commit | Checkpoints separate from commits, captured around managed execution |
| Integrating changes | Merge; check policy supplied separately | Checks run on the exact merged candidate; stale or failed results block landing |
| Revising a change | New commit identity | Stable change identity across immutable revisions |
| Agent access | CLI; integrations supplied separately | Shared Rust engine exposed through CLI, typed JSON and MCP |
| Dependency and build files | Reuse arranged separately | Verified prepared environments with private writable copies and copy-on-write cloning where supported |

On filesystems that support copy-on-write cloning, prepared dependencies and build
outputs can share underlying storage across private workspaces while remaining
independently writable. Unsupported filesystems fall back to copying. Both Git
worktrees and izu workspaces share their respective history stores; this is not a
claim that izu uses less storage overall. See [prepared environments](docs/environments.md).

**Development build:** this is an unpublished 0.1.0 build. Platform and workflow
claims depend on the recorded executable and cases. Passing component tests do
not establish end-to-end readiness; see the [development status](docs/status.md)
for recorded results and remaining work, and [validation](docs/validation.md)
for the verification procedure. Installation, upgrades and removal are covered in
[distribution](docs/distribution.md); see the [unreleased notes](docs/release-notes.md)
for capability and qualification limits.

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

Release qualification focuses on macOS ARM64 and x86-64, and Linux ARM64 and
x86-64. Exact distribution/libc requirements and support depend on native
workflow and extracted-package checks; cross-compilation alone does not qualify
a target. Windows work is on hold and Windows is unsupported for this release.
The owned repository, prepared cache and managed workspaces require native
persistent storage. Docker Desktop shared-folder cache import is unsupported;
container workflows use an owned persistent native volume. Its historical
identity failure remains open; native ext4 passes do not diagnose it.

The first-release Git boundary covers complete ordinary SHA-1 branch and tag
graphs, modes, symlinks, merge ancestry and unchanged signature bytes; preserving
a signature does not verify it. Submodules, LFS content download/upload and Git
working-tree conversions are deferred. Explicit LFS pointer interchange preserves
pointer bytes and warns that external content is unavailable. Attribute files
are preserved, with committed-byte workspace semantics reported explicitly.
Unsupported formats and content are rejected before native publication. See
[Git interoperability](docs/git-interop.md) for the full restrictions.

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

For an agent launch, checked Cargo landing and a verified backup of unfinished
work, follow the [daily-use guide](docs/daily-use.md). Local packages include
`izu-agent`, `izu-agent-verify`, `izu-backup` and `izu-cargo-check`; these helpers
require Python 3.11 or newer and call the shared native engine.

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
