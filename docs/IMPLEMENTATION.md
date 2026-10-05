# Original Rust implementation

Status: implementation in progress, no release readiness claim.

The user superseded the earlier Jujutsu-based design: izu is implemented from scratch in Rust, with an original native format and engine. Existing source code from another VCS must not be copied. Git is optional interoperability at a transport boundary, not the history engine.

Crate boundaries:
- izu-model: validated IDs, paths, source/tree/revision/change/workspace/operation records, limits, native format contracts.
- izu-platform: descriptor-relative filesystem operations and explicit persistence capabilities.
- izu-store: content-addressed immutable persistence, durable atomic state publication, locking, streaming blobs, integrity and recovery.
- izu-merge: bounded pure diff and three-way merge algorithms with explicit conflicts.
- izu-engine: repository discovery/init, capture, checkpoint/commit, changes, history, references, workspace source operations, candidate validation/integration and recovery.
- izu-process: bounded subprocess I/O, retained process ownership and observable cleanup outcomes.
- izu-runtime: optional owned process execution, resource admission, durable writer ownership and check evidence.
- izu-environment: explicitly prepared dependency artifacts and private clone/copy materialization.
- izu-git: Rust interoperability adapter using explicit Git protocol/tool boundaries, import/export and conditional publication.
- izu-bundle: versioned full-history archives and guarded cold restoration at a new location.
- izu-api: the shared typed operation facade used by every interface.
- izu-cli: human CLI, JSON output and agent/MCP stdio interface, no independent history implementation.
- izu-lab: executable acceptance and workflow benchmarking harness.

All acknowledgement and failure semantics, unsafe boundaries, and supported-platform claims require evidence. Unknown checks, unsupported features and incomplete backup state remain explicit. No unimplemented command may report success.

The shared contract is documented in [engine-contract.md](engine-contract.md).
Parallel owners coordinate API changes explicitly; the integrator reviews and
validates changes from isolated worktrees. Commits and remote publication require
the user's authorization.
