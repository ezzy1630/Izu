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
Windows work is on hold and unsupported for this release. The owned repository,
cache and managed workspace roots require native persistent storage. Container
workflows use an owned native volume and a reaping init (for example Docker
`--init`); shared-folder cache import is unsupported and remains an open issue.

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
history. Selected-object reclamation requires explicit object IDs and excludes reachable
history, recovery records and unresolved writers. See [storage reclamation](storage-reclamation.md).
There is no automatic history pruning or destructive workspace removal command.

For agent hosts, configure the documented [managed launch contract](agent-api.md)
before starting work in a private directory. MCP discovery alone cannot bind
another application's thread or change its working directory. Resource admission
and owned process groups coordinate cooperative jobs; they are not a security
sandbox for untrusted programs.

## Reproducible local candidates

The checked-in builder creates a new, unpublished candidate from a frozen source
snapshot and builds `izu` and the acceptance harness from that snapshot. It
requires Python 3.11 or later, the pinned Rust toolchain, a C compiler/linker,
and the toolchain's `rust-docs` component (for Rust library notices). It runs the
release build with the locked dependency graph and default features; ambient
Rust flags are refused. It does not replace the required checks or dependency
security audit.

For the project's isolated toolchain, invoke it from the intended checkout:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/package-release.py \
  --output "$PWD/.artifacts/local/candidate-1" \
  --cargo-wrapper /absolute/path/to/main-checkout/scripts/izu-env --offline
```

In a fresh clone, omit `--cargo-wrapper` to use the pinned toolchain through
`cargo` and `rustc`; omit `--offline` when dependency downloads are needed. The
output directory must be new and inside that source checkout's `.artifacts/local`.
The builder refuses source symlinks, unusual filesystem entries and sources over
100,000 files or 2 GiB. It checks the live and frozen source identities before
and after building. Failed attempts retain owned build data and logs for review.

The archive includes the complete collected source, both exact binaries,
`SOURCE-MANIFEST.json`, `BUILD-MANIFEST.json`, the lockfile, `DEPENDENCIES.json`,
project notices, and the selected compiler dependency license texts. Dependency
notices and unpacked dependency files are checked against the cached upstream
crate archives, whose SHA-256 values must match Cargo.lock. The inventory
records upstream package checksums and SPDX license expressions; it is not a
legal review or an advisory scan. Rust library notices come from the exact
compiler sysroot. Compiler version, build command, source digest and binary
digests are recorded. Raw build output remains beside the archive.

Tar entry ordering, owner IDs, modes, timestamps and gzip headers are fixed.
Compiler source paths are remapped to stable logical paths. This makes archive
creation deterministic for identical collected files and compiled bytes; it
does not promise identical binaries across compilers, operating systems, linker
versions or environments. To establish reproducibility for a particular build,
run the builder twice in new directories with the same inputs and compare both
archive SHA-256 values. Keep both raw build logs.

Verify and exercise the extracted bytes using the digest returned by the builder:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/release-verify.py \
  --archive "$PWD/.artifacts/local/candidate-1/izu-0.1.0.tar.gz" \
  --expected-sha256 EXACT_ARCHIVE_SHA256 \
  --work-root "$PWD/.artifacts/local/extracted-1"
```

The verifier preflights all entries before writing payloads. It rejects absolute
and traversal paths, duplicate or overlapping entries, links, special files,
unexpected modes and archives over 100,000 entries or 2 GiB expanded. It checks
the exact checksum inventory, source manifest, lockfile and binary digests, then
runs the extracted `izu-lab` with the extracted `izu` using the `cli-workflows`
acceptance scope. It records raw output, the harness report, its digest and the
explicit exclusions in the new work directory, and rechecks package data and
archive bytes afterward. Only storage crash/restart and disk-full remain outside
that named scope; attach their independent evidence before a complete release
claim. `--data-only` performs no execution and records no acceptance success.
An archive digest must come from a trusted channel before executing a package;
internal checksums cannot establish its publisher.

Run the archive input checks with
`PYTHONDONTWRITEBYTECODE=1 python3 scripts/release-tests.py`. They exercise
traversal, links, duplicates, path overlap, resource limits, extra/changed payloads
and source changes. They do not establish product acceptance. Packaging never
installs, signs, notarizes, publishes or changes host integration.


Run the complete reproducibility and extraction qualification on each native
host with `python3 -B scripts/qualify-release.py --output "$PWD/.artifacts/local/qualification"`.
It creates two independent packages, compares their identities and runs all 15
CLI workflows from each actual extraction. Supply the absolute `--cargo-wrapper`
for the project-local toolchain and `--offline` when dependencies are cached.
CI performs this same qualification on macOS ARM64/x86-64 and Linux x86-64;
Linux ARM64 uses an owned native-volume runner. A workflow definition alone is
not evidence that any run passed.


## Install, upgrade and remove

Choose the archive for the exact operating system, architecture and stated libc
floor. Obtain its SHA-256 from a trusted release channel, then use the verifier
above in a new directory. `--data-only` checks package data without executing it;
full verification also exercises the extracted binaries. Read `BUILD-MANIFEST.json`
and the release qualification matrix before installing. An archive checksum
does not establish publisher identity. macOS ad-hoc linker signatures are not
Developer ID signatures or notarization; a Gatekeeper-rejected candidate is not
a qualified downloaded macOS distribution.

Install verified bytes into a new version directory you explicitly own. For
example, after choosing a qualified artifact and verifying it:

```sh
version_root="$HOME/.local/opt/izu/0.1.0-TARGET"
mkdir -p "$version_root/bin"
install -m 755 VERIFIED_PACKAGE/bin/izu "$version_root/bin/izu"
export PATH="$version_root/bin:$PATH"
izu --version
izu capabilities --json
```

Replace `TARGET` and `VERIFIED_PACKAGE` explicitly. Do not reuse an existing
version directory or overwrite an executable being used by a job. No installer
edits shell configuration, Git configuration or agent-host settings. `izu-lab`
is an optional acceptance tool; ordinary native workflows need only `izu`.
Use the README source-history and private-workspace commands for first use.

Before an upgrade, inspect managed jobs and unresolved writer ownership. Let
owned work finish or cancel through its owning controller; retain stopped-source
receipts. Create a native bundle backup and record `izu format inspect`. Verify
the new archive and run its acceptance in isolation, install into another new
version directory, then select it in the calling shell's PATH. Preserve the
previous executable and its build manifest. New metadata v2 needs explicit
HEAD-leased migration when starting from v1; there is no automatic downgrade.
The cooperative runtime registry feature also requires a compatible runtime.
Never alternate incompatible old/new writers on the same metadata.

For rollback, restore the pre-upgrade native bundle into a new directory with
its compatible reader; preserve both the original and newer repository. Do not
replace current metadata with an old snapshot. Recovery and writer-stop guidance
is in [durability](durability.md) and [agent API](agent-api.md).

Removal deletes only the explicitly installed version directory and any launch
link you created for it after checking that link's target. Remove its PATH entry
from your own shell configuration if you added one. Repository source, `.izu`
history, bundles, private workspaces, credentials and unrelated toolchains are
not installation files and must remain preserved. No background service is
installed by the standalone command line.

## Native CI evidence

The macOS ARM, macOS Intel and Linux x86 runners execute independent validation
and package jobs for the same commit. Each job retains its existing 30-minute
limit. A release candidate requires both jobs to pass on every required native
host; a successful package job cannot replace failed validation.

Validation retains formatting, strict default and fault-injection Clippy,
workspace and persistence-failure tests, Python host interruption regressions,
and the 15 CLI workflows against the exact release executable. The package job
fetches locked dependencies, then builds two fresh default-feature archives
offline and verifies all 15 CLI workflows from each extracted package. It does
not reuse the validation job's executable. Artifacts are named
`validation-<runner>-attempt-<n>` and `packages-<runner>-attempt-<n>` so reruns
retain distinct raw evidence, including failed MCP deadline captures.

Before package compilation, `scripts/native-build-identity.py prepare` refuses
ambient compiler, flag and Cargo configuration overrides, selects explicit native
C and Rust linker tools, and records tool paths, hashes, versions and the macOS
SDK or Linux libc. The package step applies that recorded selection. After both
packages qualify, `finish` binds their source manifest, archive and executable
hashes to the selection and records Mach-O load commands or ELF requirements and
dynamic dependencies. The sidecar also records the runner image and executed OS.
Binary minimum-version metadata does not establish support for an older OS or
libc than the native host that actually ran the checks. The helper's `observe`
mode can inspect existing qualified packages, but labels effective historical
tool selection as unproven; it cannot retroactively create build provenance.
