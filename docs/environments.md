# Prepared environments

`izu-environment` is the optional preparation layer for original izu source
workspaces. It imports explicitly trusted, quiescent dependency directories and
warm file outputs, publishes a verified native artifact, and materializes private
writable views in exact managed workspaces. Source history still belongs to the
shared engine. The native cache is outside default source bundles.

Parsing a recipe, opening a repository, discovery, status and materialization do
not execute the recipe. Import accepts an existing prepared directory; the caller
must stop its installers, watchers and database writers first. This release does
not infer quiescence or run dependency installers automatically. Run trusted
commands explicitly through the runtime, stop/reap their process group, and then
import. No global package store is created or modified by this layer.

## Explicit recipe

Recipe JSON schema version 1 has these fields:

| Field | Meaning |
|---|---|
| `schema_version` | Exactly `1` |
| `source_identity` | Captured source tree ID as 64 lowercase hexadecimal characters |
| `lockfiles` | Relative paths and SHA-256 digests of declared lockfile bytes |
| `toolchain_identity` | SHA-256 identity supplied by the trusted caller |
| `platform` | Non-secret `os`, `architecture`, and `abi` identifiers |
| `recipe_identity` | SHA-256 identity of the approved preparation definition |
| `trust_domain` | Explicit non-secret identifier for cooperating callers |
| `argv` | Explicit intended preparation argv; never executed implicitly |
| `dependencies` | Relative directory roots to import |
| `outputs` | Relative warm-output roots; absent roots become private empty directories |

Paths must be bounded UTF-8 relative paths with ordinary components. Metadata
names `.izu`, `.git` and `.izu-recovery`, dot components, duplicate paths and
overlapping declarations are rejected, including canonical case-folded and Unicode
aliases. Lockfiles cannot lie inside materialized
roots. Declarations intentionally constrain exactly what is imported; the rest of
the prepared source directory is not copied.
Shared ancestors must also use the same literal spelling across directory and
lockfile declarations. `vendor/one` and `VENDOR/two` are refused even though the
two final directory names differ.

The key hashes a deterministic identity containing schema, source, sorted declared
lockfiles, toolchain, platform, recipe, trust domain, argv digest and directory
declarations. Changing any of those changes the key. The cache manifest stores
argv's digest, not its raw arguments, and stores no environment values. Callers
must supply non-secret labels and credential-free recipe identity inputs. The
toolchain and ABI are caller declarations: this layer does not execute a tool to
attest them. OS and architecture must match the running process. Lockfile content
is verified both when importing and before materializing into the locked target.

Relocatability is part of the approved recipe. Copying `node_modules`, `.next` or
`target` is not proof that a specific package manager, native addon or build cache
will work after relocation. Absolute symlinks and links that lexically escape a
declared root are refused. Internal relative symlinks are preserved literally.
Database snapshots require an explicitly designed, stopped-writer recipe and
database-specific restore verification; no generic portable database or large
build-cache promise is made.

## Native cache and publication

The caller supplies the cache root, normally `.izu/environments/v1` beneath the
repository's metadata directory. Its layout is:

```text
artifacts/<key>/manifest.json
artifacts/<key>/READY
artifacts/<key>/payload/<declared-root-index>/...
artifacts/.build-<key>-<random>/...
locks/<key>.lock
locks/admission.lock
```

`manifest.json` contains the versioned identity and an ordered entry inventory:
type, relative path, original POSIX mode, byte length, SHA-256 file digest or
literal symlink target, and native device/inode identity. `READY` binds the key,
manifest digest and manifest inode. This cache is native and device-specific,
not an exchange format. Each reuse enumerates the entire payload, rejects extra
or missing entries and replacements, verifies modes and hashes regular files.
Regular cache files must have a single hard-link count. Corrupt ready keys are
refused, never overwritten as an automatic repair.
An active cache borrow retains the original artifact and payload descriptors;
their named entries are rechecked before materialization or starting-state proof.

Preparation uses a unique stage and holds a permanent advisory key lock plus an
admission lock. Files and directories are synced before atomic no-replacement
publication; the containing artifacts directory is synced before a durable
receipt. A fresh import then reads the published artifact through the full reuse
verifier under the same exclusive key lease, compares its complete summary
(including manifest digest) with the originally prepared summary, and rechecks
the borrowed artifact's locators before acknowledging success. A failure during
that readback or acknowledgement, including cancellation, returns
`PublicationIdentityUncertain` with the published path and underlying cause;
the artifact is retained. These checks do not diagnose the cause of an identity
mismatch or establish identity stability across filesystem reopen or remount.
A visible publication followed by a failed persistence barrier returns
`DurabilityUncertain`. Ready metadata is read-only as a cooperation aid. The same
user can change its permissions; this is not a security boundary.
Cache and writable-view boundaries reject known memory-backed filesystem types
before creating their writable namespace. Successful sync on tmpfs does not
establish persistence. Classification cannot prove persistent hardware beneath
an otherwise ordinary filesystem.

The cache root, artifacts directory and lock directory retain pinned descriptors.
Their path locators must still identify those directories when acknowledging an
operation. Publication also compares the staging name with its original pinned
directory before rename and compares the installed name before and after the
directory persistence barrier. A replaced staging name is refused. A replacement
after publication returns `PublicationIdentityUncertain`; it cannot produce a
successful receipt for the replacement. Moved originals and unfamiliar entries
are preserved. These observations do not make same-user namespace edits atomic
with the following syscall.

`CacheStatus` distinguishes missing, building/in-use, incomplete and verified
ready state. A killed or cancelled build is retained under its staging name and
cannot be reused as ready. Reimport creates a new stage and publishes a newly
verified artifact. Automatic garbage collection is deliberately absent:
`RetentionPolicy::RetainAllArtifactsAndIncompleteStages` prevents deleting unique
state or artifacts used by borrowers. Cache borrowers hold shared key leases
through verification and materialization, so independent views may read the same
immutable artifact concurrently. Preparation requires an exclusive key lease.

## Private materialization

`WorkspaceTarget::checked` obtains the engine's live-writer exclusion lease and
source-writer lock, pins a no-follow directory descriptor, and captures source
identity. The primary human source workspace is refused. All target selection,
staging and publication occur while those guards remain held. Managed runtime
writers and engine restore/close share that protocol. External editors and
unmanaged processes are outside the cooperative protocol.

Every declared destination must be absent or an empty directory. Existing files,
untracked content, nonempty directories, symlink parents and symlink destinations
are preserved by rejecting the operation. The implementation traverses each path
component relative to pinned descriptors and never recursively deletes a target.
It stages writable private roots in the protected
`.izu-recovery/environment-staging` namespace, rechecks source and lockfile
identity, then creates any missing destination parents with persistence barriers
and publishes each root with an atomic no-replacement rename. Creating parents
after the fresh source capture prevents an empty new parent from invalidating
that capture.

Multiple declared roots are individually atomic, not one filesystem transaction.
Failure reports the roots already published and retains incomplete staging
paths. Readback verifies that installed root inodes still identify the staged
roots. Success removes only the operation's now-empty stage. Choose ignored
dependency/output paths in the source workflow; no ignore file is silently edited.
The protected recovery parent, staging parent and stage name are revalidated
before publication and empty-stage cleanup. A substituted directory is preserved.
Each staged root retains its original descriptor. Nested destination parents are
checked against their rooted locators before rename; installed roots and parents
are checked after publication and again after the cleanup persistence barrier.
Post-publication replacement reports identity uncertainty with the paths already
published. Staging paths in a failure are locators and may no longer name the
original moved directory.

Apple uses Rustix's safe `fclonefileat` wrapper; Linux uses its safe `FICLONE`
wrapper. Only a successful clone syscall reports `Clone`. Unsupported filesystem
or cross-device clones fall back to bounded streaming copy and report `Copy`;
mixed results are explicit. Other errors propagate. Destination regular files
have distinct inodes and are made writable, with original executable bits
retained. No writable file is shared by hard link. `.next`, `target`, logs and
database writes in one view therefore do not mutate another view or the cache.

Limits bound file bytes, total artifact bytes, entries, depth, manifest allocation,
cache logical bytes and lock waits. Streaming buffers are 64 KiB and metadata
buffers use fallible reservations. Cancellation is checked between entries and
chunks; an in-progress filesystem syscall is not forcibly interrupted. Admission
uses conservative logical-byte reservations and filesystem free-space checks so
copy fallback has room. These are cooperative budgets, not OS hard limits.

## Environment-bound checks

An environment recipe or cache key alone is not execution attestation.
`EnvironmentCache::binding` first verifies the ready artifact and produces a
bounded versioned `EnvironmentBinding`: the validated recipe identity, cache key,
exact native manifest digest and intended `StartingFileContents` verification
scope. Its encoded size is limited to 1 MiB. It contains the argv digest, not raw
arguments or environment values. Parsing a binding executes nothing.

The shared facade's `izu environment bind RECIPE --trust-recipe` writes these
bytes as a durable local native IZU Blob. The returned Blob object ID is distinct
from the cache key. This object is initially unreferenced: creation of a binding
does not publish a history operation. A candidate's `CheckSpec.environment`
references that Blob and roots it in the candidate's native object closure.
Generated cache data remains excluded from default source bundles. A referenced
binding Blob is included as ordinary referenced metadata; importing it does not
create the native artifact it describes.

Binding uses the repository's `.izu/environments/v1` cache, matching the runtime
resolver. It does not accept an arbitrary cache locator. The native manifest
includes device/inode identities, so bindings do not promise portable cache
restoration. A missing or different artifact requires explicit preparation/import
and a fresh binding; it is never silently accepted under an old digest.

For a bound candidate check, the runtime reads the exact native Blob through a
bounded sink, parses it, and materializes its referenced verified artifact into
the fresh private check workspace. `materialize_binding` checks both source and
manifest digest before any destination is published. Before the runtime releases
the command, `verify_starting_environment` uses its already-held engine writer
lease and pinned source descriptor. It captures source before and after, verifies
lockfile bytes, inventories every declared private root, and checks every file's
bytes, mode, type, literal link target and absence of hard-link/cache inode aliases.
Extra entries, replaced source locators, stale source, tampering and changed cache
locators are refused. Verification acquires no second live-writer lease.

The returned `StartingEnvironmentProof` can only be constructed by verification
and retains a shared cache lease. Its serializable receipt records the actual
starting-content digest, source, workspace, key and manifest digest. The runtime
records that receipt before command release and can retain the proof through
process-group reap. OS and process architecture are observed. ABI and toolchain
identity remain explicitly declared; no executable or compiler probe is claimed.

The receipt describes the files before execution. Warm outputs and dependencies
are still writable, and later output changes do not turn that receipt into proof
of an immutable environment throughout the job. This is a cooperative protocol,
not isolation from arbitrary external editors or hostile code as the same user.
Checks without an environment binding keep their environment/toolchain unknown.

## Evidence and measurement

The crate tests import to two managed workspaces, require actual native clones
when `IZU_REQUIRE_CLONE=1`, modify one dependency and warm output, and read back
the second view, preparation and cache. They also cover forced-copy mode,
executable bits, empty/missing nested roots, untracked preservation, symlink
escape, wrong content, same-content inode replacement, key invalidation, source
and lockfile mismatch, active writer leases, bounded input, cancellation, quota
rejection, concurrent import and killing a real child importer before publication.
Focused regressions cover aliased ancestor spellings, nested-parent replacement
and substituted empty-stage cleanup. Linux tests use an intentional bounded
`/dev/shm` fixture to verify that cache creation and private-workspace source
publication refuse known volatile storage even when sync succeeds. The workspace
refusal occurs before registration or environment materialization; the tests
require unchanged repository state, source and cache, and an empty unregistered
target.

The `views` example measures 1, 8 and 30 views with forced copy and native clone
preference. It records import/setup/resume wall time, complete content readback,
all writable file inode uniqueness and isolated mutation. The fixture is binary
files plus one warm output; it does not pretend to install real dependencies or
validate a database. "Cold" means absent artifact and destination; OS page cache
is not controlled.

```sh
cargo run -p izu-environment --example views -- \
  --root /existing-parent/empty-owned-bench --views 1,8,30 \
  --payload-mib 16 --policy both
```

`allocated_file_bytes_sum` sums `st_blocks * 512`. Shared clone extents can be
counted once per file, so this value is not unique physical use or a savings
measurement. The example also records filesystem available-byte changes. Those
deltas count allocation only when the caller runs on an exclusive isolated
filesystem, syncs it, and accounts for filesystem metadata; a busy shared APFS
container cannot establish unique physical savings. `filesystem_available_bytes`
provides the same safe `fstatvfs` observation for an isolated APFS-image experiment.
Benchmark directories are retained for inspection; no unknown state is deleted.
