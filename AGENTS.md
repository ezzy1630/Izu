# izu

This is an original Rust version-control product. Do not copy or depend on Jujutsu, Git implementation code, or another VCS engine. Standard general-purpose Rust libraries are allowed, with dependency and unsafe review. All interfaces call the shared Rust engine/API; do not duplicate history mutations in CLI, MCP, or transport adapters.

Follow the user's session instructions. Commit, push, publish, change remote resources, or change credentials or permissions only when the user has authorized the exact action and target. Commits inside owned disposable test fixtures are allowed when needed to test Git interoperability. Preserve other work.

Parallel writers use their assigned isolated worktree and explicit file ownership. Do not edit outside that ownership. Request contract changes from their owner. Integrator owns root manifests, lockfile, and cross-crate integration. Return concise reports with exact changed paths, commands, test results, defects and limits; reports are not proof.

Safe Rust by default. Typed errors, validated boundaries, checked sizes/arithmetic, bounded input, cancellation and deliberate allocation behavior. No production unwrap/expect/panic on untrusted input. Any required unsafe/FFI boundary must be small, documented, reviewed and tested. Do not weaken gates.

Durable acknowledgement requires object and state persistence, including directory entries. Concurrent writers must not lose history. Partial failures after a visible state update must report uncertainty honestly. Test crash points, corruption, disk-full and concurrent processes. Preserve unique work before workspace removal.

Use Cargo tests for meaningful behavior, run actual CLI workflows, bind evidence to exact binaries. Benchmark before optimizing. No assembly or C subsystem without demonstrated need. Document versioned native and exchange formats independent of Rust layout.

The project root is this checkout. Keep isolated Rust tools, caches, agent worktrees and temporary directories under `.artifacts/local` inside the project. Do not depend on another checkout or an external drive.

For the existing isolated development setup, run commands from the intended checkout with the main checkout's `scripts/izu-env cargo ...` helper. From a worktree, resolve the helper's absolute path from the main checkout. Fresh clones without that isolated toolchain use the pinned Rust toolchain and commands in README.md and CONTRIBUTING.md.

The helper selects the local toolchain, two build jobs, a private `target-izu-internal` directory in the current checkout, and internal temporary storage. Always set the tool working directory explicitly. Agent worktrees live under `.artifacts/local/lanes` in the main checkout. The local validation campaign retains a private historical path map at `.artifacts/relocation-20261004/path-map.json`. Use `python3 scripts/record-verification.py UNIQUE_LABEL COMMAND ...` from the main checkout with its isolated toolchain provisioned for fresh source-bound verification.

For the existing local validation campaign, its private freeze, package and report helpers are in `.artifacts/relocation-20261004/operations-r5`; earlier artifact helpers remain historical records. These generated files are not included in public clones. Rebuild tests and create fresh fixtures after relocation. Native device and inode identities in earlier receipts belong to their recorded filesystem. The public development status is documented in docs/status.md.
