# 0.1.0 release candidate — unreleased

This candidate preserves the original Rust history engine and shared CLI,
JSON/MCP and runtime APIs. It has not been installed as a daily driver, signed
with Developer ID, notarized or published as a binary release.

The continuing implementation adds native tags with explicit metadata-v2
migration, annotated/nested Git tag round trips, unchanged signed commit and
embedded merge-tag byte preservation, selected SSH transport, exact leased
remote publication/readback and interrupted-publication uncertainty. Signatures
are opaque; preservation does not verify their authenticity. New content never
inherits an old signature.

Reliability work includes atomic runtime readiness, MCP backpressure/shutdown
handling, bounded authenticated fallback when a private Unix socket pathname is
too long, prepared-cache publication sealing after rename, and selected-object
reclamation with reader/writer retention leases. Cooperative host interruption
uses an explicit bounded worker handoff and retains writer intent when shutdown
cannot be established. Native filesystem barriers and replacement identity
checks remain enforced.

The first-release platform scope is macOS ARM64/x86-64 and Linux ARM64/x86-64;
actual support depends on each final package's native evidence. Windows is on
hold. Owned repositories, caches and managed workspaces require native persistent
storage. Docker Desktop shared cache imports are unsupported and their identity
failure remains unresolved. The qualified Linux userland must match the reported
ABI; observed Ubuntu 24.04/glibc 2.39 results do not qualify older libc.

Approved Git limits defer submodules, LFS payload hydration/upload and Git
working-tree conversions. Explicit pointer interchange and attribute files are
preserved with visible limitations. Other rejected layouts, formats and metadata
are listed in [Git interoperability](git-interop.md); complete Git adoption is
not claimed.

Qualification is still incomplete. Historical first-discovery MCP and Linux
30 ms storage acquisition failures remain unexplained. A later Linux ARM64
candidate also recorded four 30-second managed-controller launch timeouts;
those are separate from the diagnosed cache ancestor guard and process-test
phase assumptions. Passing reruns do not establish their causes. Actual host,
remote protection/SSH, final native packages, representative performance and
release approval remain bound to their exact recorded artifacts.

Persistent virtual HFS+/ext4 ENOSPC, corruption, process crashes and remount
readback are evidence for their tested failure models, not physical power-loss
proof. Benchmarks report distributions, disk use, sampled aggregate memory and
host/cache limitations; no general speed or total-storage superiority is claimed.

See [distribution](distribution.md) for verification, installation, upgrade and
removal, [format](format.md) for migration, and [development status](status.md)
for the current qualification boundary.
