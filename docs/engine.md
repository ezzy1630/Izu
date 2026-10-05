# Native source engine

`izu-engine` is the original source and history implementation shared by the
human CLI, machine API, runtime, Git adapter, and bundle adapter. It does not
use Git or another VCS engine for native operations. `izu-model` defines
validated native objects, `izu-store` owns durable object/operation storage,
`izu-platform` owns safe descriptor-relative filesystem access, and `izu-merge`
provides bounded byte diff and three-way merge.

## Source and history

A repository starts with an empty, explicit `RevisionOrigin::Bootstrap` revision
and a `main` reference. Initialization leaves existing source untouched and does
not capture it. A private workspace has a regular `.izu` locator marker; the
primary root has the native `.izu` directory. Discovery searches parents and
opening selects the workspace registered at the exact canonical root.
An existing source directory or a new final directory under existing parents
can be initialized. New directory creation is exclusive and descriptor-relative;
its parent is synchronized before native metadata publication. Initialization
does not create missing ancestors or overwrite an existing file or create race.
Initialization refuses an existing legacy `.ezy` or `.ezy-recovery` entry of
any type before creating `.izu`. Legacy prototype data remains untouched;
IZU format 1 does not migrate or accept the old EZY object magic.

`WorkspaceExpectation { head, working_tree }` identifies a committed baseline
and a captured working source. `checkpoint` captures source into the `working`
record without creating a revision, moving the workspace head, or moving a
reference. `commit` creates an intentional revision with an explicit `Identity`
and a new stable `ChangeId`. `revise` creates another immutable revision of the
same change. Bootstrap identity is a declared internal service actor and cannot
stand in for an intentional author.

`commit_to_ref` adds `RefExpectation { name, expected }` and publishes the new
workspace head and owned reference in one operation. An existing reference
expectation must equal the workspace head; divergence requires explicit
reconciliation. Plain private commits do not move a reference. `update_ref`
and `import_revisions` require expected-old reference values, with `None`
meaning an absent reference. They never change workspace files.

`Selection::Paths` includes named paths and their descendants. Selective capture
overlays those paths on the saved working source; selective commit overlays
them on the committed baseline. Unselected files, dirty changes, and checkpoint
state stay independent. Required parent directories are explicit tree entries
with exact modes. A new selected child captures missing ancestor modes;
existing unselected ancestor metadata stays at its baseline value.
Selective restoration recreates only missing ancestors required by selected
target entries, using their recorded directory modes. Existing unselected
directory modes, including ignored source, remain observed values. Non-directory
ancestors and unknown source collisions are refused before source changes.

Trees preserve regular files, executable and arbitrary POSIX permission modes,
raw symlink targets, explicit directories, and unresolved conflict alternatives.
Paths must be valid UTF-8 POSIX relative paths. Internal `.izu`, `.git`,
`.izu-recovery`, legacy `.ezy`, and `.ezy-recovery` components are protected.
The shared namespace key uses Unicode
normalization and full case folding; ambiguous names are rejected even on a
case-sensitive host. This is a conservative portability contract rather than a
claim to represent every host filename. Imported special permission bits remain
in history, but materialization refuses setuid, setgid, and sticky bits.

Ordinary capture honors root and nested `.gitignore` and `.izuignore` rules for
new entries. Files already in the committed or working tree remain tracked even
if newly ignored. `capture_all` includes ignored content for recovery and owned
environment checks; it does not admit that content into the ordinary working
source. Native metadata and recovery directories are never traversed.
Regular files with hardlink identity are unsupported: ordinary and full recovery
capture refuse them with `UnsupportedEntry` rather than saving independent file
entries. Descriptor metadata and link count are checked around each read;
observed link changes cannot receive a successful capture acknowledgement.

## Publication and concurrency

Source blobs and trees may be prepared before the metadata transaction. All
finalized immutable objects remain retained; this initial engine has no garbage
collection or history deletion operation. Every metadata mutation acquires a
store transaction, reads the latest operation/view, validates the expectations
of the touched workspaces and references, applies the change, and publishes an
operation whose parent is the locked current head. Independent workers never
publish a stale whole-view replacement.

Capture stages its blobs and canonical tree in one bounded object batch. The
addresses used to encode the tree are provisional until the store completes its
ordered data and directory persistence barriers. Capture returns only after
that batch succeeds; a failed batch cannot advance the operation head. Strict
individual object imports keep their own durability acknowledgement contract.

A successful receipt names the exact durably published operation and source
revision/tree. Object and directory persistence use the store/platform's
supported host durability capability. `PublicationUncertain` names a visible
operation whose durability or source namespace acknowledgement could not be
confirmed. Restoration errors preserve
`recovery_operation()` separately; `uncertain_operation()` names the actual
visible final operation, if any. A visible write is never reported as rolled back.

`fork_managed_workspace_receipt` returns a `WorkspaceForkReceipt` with the new
workspace and the exact fork operation. A caller composing a fork with a launch
can retain that publication if a later step fails, even after another controller
advances history. The state-returning fork methods use the same implementation.

Fresh forks use a directory-only persistence batch after verifying that their
pinned source root is actually empty and its locator still names that root.
The target is provisional and unregistered; caller-selected targets can remain
visible to other processes during preparation. Each regular file is created
exclusively at its final name, then streamed, given its recorded mode and
strongest-synced while its original descriptor remains open. The resulting name
must still identify that file. Symlinks are exclusively created at their final
names. No fresh failure path or destructor unlinks named source: an error can
leave zero-length or partial files, symlinks and directories available for
inspection or explicit recovery. Internal temporary hardlink aliases are absent.

Bounded device/inode records track observed identities for every published
entry. All participating source directories and entries must share the selected
source device. Each changed directory is kernel-submitted once; the source-root
strongest barrier and layout checks complete before marker persistence, full
source verification, root-identity checking and native workspace registration.
An observed late replacement fails while retaining displaced and unknown source.
These records cannot detect inode reuse or replacement before initial capture;
they do not provide continuous exclusion or an OS sandbox. Per-file descriptors
remain open through their data barrier and final-name comparison; tree traversal
uses a constant number of descriptors.

The fresh-fork boundary rejects destinations classified as known volatile by
`izu-platform` before source materialization. The classifier does not attest
arbitrary filesystems or physical storage. A late device/layout change fails
with source retained; it never restarts a different materialization mode.
Source-directory persistence and the metadata store's acknowledgement remain
separate barriers even when those two roots reside on different devices.
Restore and archive materialization keep their existing strict recovery protocol.
Fresh macOS and Linux default/fault engine suites, strict Clippy and all 15
CLI workflows passed for the isolated change. On internal APFS, three paired
512-file samples of 4096 bytes each reduced median workspace creation from
8.54 to 3.29 seconds and candidate-file checks from 11.09 to 5.87 seconds.
Both sides used the same frozen worker, fixture, 30-second command deadline
and unchanged executable/source snapshots. OS cache and other applications
were uncontrolled; RSS and the full cooperative benchmark were not measured
by this comparison. These local measurements do not replace integrated
acceptance or establish filesystem/device durability.

Permanent per-workspace native locks serialize native capture, checkpoint,
commit, restoration, and closure. A separate live/materialization lock permits
a managed child to checkpoint or commit while preventing restoration, closure,
and owned environment materialization during its selected process group.
`lock_workspace` holds both locks and a pinned source `Directory`. Its `capture`
method uses that descriptor rather than reopening the workspace pathname.

`lease_workspace` or `lease_writer` creates a durable `WriterIntent` and holds
the live lock. Dropping the lease leaves an unknown intent: a controller crash
does not prove its descendants stopped. Only `finish_stopped`, after the owner
has stopped and reaped the group, clears the intent. Explicit recovery uses
`acknowledge_writer_stopped` with the exact persisted `WriterToken`; ordinary
store recovery never clears it. These locks coordinate native and managed
writers, not arbitrary external editors or unregistered processes.

The writer lease separately retains the actual source `Directory` and the
repository descriptor. `source_directory()` exposes the pinned source for
descriptor-relative environment verification. `capture_leased` acquires only
the native source lock, verifies repository identity, exact workspace state,
and the current root inode before and after capture, and reads through that
source descriptor. A foreign lease, changed workspace, root replacement, or
symlink substitution fails; it never reacquires the caller's live lock.

Ordinary capture, status, checkpoint, commit, revise, and closure also retain
the original source descriptor through their receipt. A shared publication
guard checks the source locator and repository metadata directory, plus the
exact repository/workspace binding of private markers, before publication and
after a visible result. A prepublication substitution is refused without moving
HEAD. A substitution during publication returns `PublicationUncertain` with
the exact visible operation and retained source tree and locator context.
An incomplete close after its source was saved additionally identifies the
retained recovery operation. These checks do not acquire a live writer lease or
require files to remain unchanged after an immutable observed capture; native
checkpoint and commit remain allowed with a live or unknown managed writer.

## Restore, update, undo, and revert

`restore` is an explicit source replacement. Before files change, it captures
eligible source and a separate full recovery source, publishes the recovery
record, and retains exact originals. It moves replaced files and symlinks with
no-overwrite rename into `.izu-recovery/<operation>/`; it never recursively
deletes unknown source. File and symlink installation uses exclusive staging
inside that protected recovery directory and no-overwrite links. Symlink
parents are never followed. Directory removal is empty-only.

Materialization preflights all result paths, entry kinds, blob lengths,
aggregate bytes, unresolved conflicts, and permission restrictions before
changing source. Stream writes are bounded again during copying. After changes,
the engine recaptures through the pinned root and verifies the exact intended
tree before final metadata publication. A changed root pathname cannot cause
writes to a substitute directory and prevents a misleading final acknowledgement.

Unselected and unknown files are preserved. A collision with an unknown path
is an error, and ignored recovery content never becomes ordinary tracked
source. `update_workspace` additionally requires eligible source to match the
committed baseline under the same lock, including eligible untracked entries.
It preserves ignored private files and refuses dirty updates.
Known baseline differences return `UncommittedSource` with the workspace and
relative path; preserve, commit, or reconcile those changes before replacement.
`SourceChanged` instead reports an edit detected during capture or a replaced
source root.

`undo` reverses only the selected workspace source effect of an exact operation.
It refuses an unrelated operation, intervening workspace source metadata, or
new selected disk edits. It never rewinds other workspaces or references. This
is workspace-and-path scope; the operation schema does not provide actor-based
history filtering. Explicit `restore` remains the way to select an older source
after intervening work.

`revert` applies a single-parent revision's inverse three-way delta to selected
clean paths of the current committed source, preserving later independent
changes and unselected dirty source. It creates an intentional revision. A
conflict returns `RevertPreparation::Conflicted` with a durably registered
revision/tree while leaving files and workspace head unchanged. `revert_to_ref`
uses the same expected-reference protection as `commit_to_ref`; a clean result's
head and owned reference are published together. Reverting a merge without an
explicit mainline is refused; this API does not guess one.

Restoration is not an atomic filesystem snapshot. An external edit, source
error, cancellation, storage failure, or failed final metadata publication can
leave partial materialization. Preserve the recovery operation and original
files, inspect the current source and operation head, then reconcile explicitly.
Do not retry on the assumption that source was rolled back.

`fork_workspace` materializes an exact revision into an explicitly empty target,
pins it during writes, verifies it, and publishes the registered workspace.
`fork_managed_workspace` allocates an owned private target under
`.izu/workspaces`. A failed fork leaves prepared source available for inspection.
`close_workspace` saves full final source in operation history and removes only
registration. It leaves files, ignored content, marker, and all history intact;
the primary source workspace cannot be closed. Deleting a workspace directory
or history is not an engine capability.

## Merge candidates and checks

`prepare_merge` computes a bounded common-ancestor graph and three-way tree
merge for an expected target reference and exact source revision. Unrelated
histories have an empty base. Ambiguous multiple merge bases fail explicitly.
Modes, entry types, symlink targets, deletion/modification, binary data, and
file/directory overlaps produce native conflict alternatives when unresolved.
Unsupported path aliases fail without replacing either source history.

Both clean and conflicted results are immutable revisions/trees attached to a
durably registered `IntegrationCandidate`. Candidates bind target, expected
target revision, exact source revisions, result revision/tree, and declared
`CheckSpec` entries. They do not materialize or change the primary source.
`resolve_candidate` creates a new immutable result revision of the same stable
change and a new candidate. `resolve_conflicts` similarly revises a registered
native conflict revision without changing files. Resolutions must name actual
conflicts or their descendants; choosing a file over a conflicted directory
requires explicit descendant removals. No subtree is implicitly deleted.

`start_check(candidate, name)` durably publishes a unique `CheckAttemptId` and
`Pending` evidence before admission or launch. It replaces the current pointer
for that declared check, so an old pass cannot satisfy the gate during a rerun
or after a crashed attempt. Terminal `record_evidence` requires the active token,
candidate, check, revision, tree, argv, environment option, and original start
timestamp to match exactly. An older completion is rejected. Repeating an
already accepted identical terminal result is idempotent. Ordering comes from
durable publication, never wall-clock timestamps.

`land` requires at least one declared check, exactly one current passing record
for each required check, a conflict-free result, matching immutable candidate
inputs, and an unchanged target reference. Pending, failed, cancelled, absent,
or ambiguous evidence cannot pass. It updates only the target reference; the
human workspace is not refreshed behind an editor or runtime writer.

The engine validates check bindings and lifecycle, not external command
execution. The trusted runtime caller must launch the declared command and
verify source before and after it before recording a pass. `environment: None`
does not attest an environment identity. A pinned environment that the runtime
cannot realize or verify is unsupported, not a verified check environment.

## Traversal, portability, and recovery

`revision`, `tree`, `blob`, `read_blob`, `object_info`, and `read_object_to`
provide typed immutable traversal and streamed export. `put_blob`, `put_tree`,
`put_revision`, `put_object`, and `import_object` prepare immutable imports;
`import_revisions` performs the one guarded history/reference publication.
Raw imported objects never adopt foreign workspace locations or change HEAD.
Bootstrap emptiness and source revision/tree bindings are verified across
objects, not inferred from descriptions or author strings.

`put_blob` acknowledges an exact durable local object. This store retains
finalized objects even before a history reference exists. An environment binding
created this way is initially unreferenced; using its ID in a candidate's
`CheckSpec.environment` makes it reachable from native history and the archive
closure. A local object receipt alone is not an operation or archive receipt.

Cold archive restoration starts with absent HEAD. `initialize_archive_at`
accepts the caller's pinned, empty staging `Directory` and a locator used only
for records and errors. `adopt_archive_at` verifies its `.izu` descriptor is the
pinned store, validates the archived closure, bootstraps its original operation
chain, materializes only the explicitly selected working source through the
staging descriptor, and appends a remapping operation for the final root.
Historical workspace paths are never touched. The bundle adapter owns final
no-overwrite directory publication and its parent synchronization.

`verify` checks typed reachable objects, revision/tree and candidate/evidence
bindings, change membership, and bootstrap semantics. `recover` delegates
store verification and inactive owned-temp cleanup; unknown entries and writer
intents remain explicit. Unsupported versions, corrupt objects, incompatible
paths, permission failures, lock timeouts, and source limits return typed
failures rather than successful placeholder results.

Source capture bounds visited entries, depth, individual blob bytes, aggregate
bytes, and ignore input. History reads also bound aggregate metadata bytes.
Merge reads have a separate blob budget. Large blobs stream through the store;
byte text merging is intentionally bounded. Cancellation is checked before
publication and during source/storage/merge traversal. Reads compare file
identity, size, permissions, and timestamps before and after capture; this
detects observed mutation but does not claim a cross-file filesystem snapshot.

The implemented host boundary is safe Rust on supported Unix durability and
descriptor APIs. Other hosts fail with explicit unsupported capability errors.
Native format/version validation is owned by `izu-model` and `izu-store`;
portable exchange and Git interoperability have their own adapter contracts.

## Verification

`crates/izu-engine/tests/native_flows.rs` exercises real filesystem and engine
flows: reopen/readback, selective edits and nested directory fidelity, ignore
admission, independent repository instances, stale references, simultaneous
restores, symlink parents, writer intents, cold archive history, substituted
staging locators, persistent conflicts, inverse reverts, pending/current check
ordering, clock rollback, and materialization budgets.

Run the crate's normal and release tests plus strict all-target Clippy. The
development-only `fault-injection` feature additionally exercises actual visible
final-publication uncertainty; the store intentionally refuses that feature in
release builds. Local engine checks do not prove the integrated CLI, runtime,
Git transport, bundle publication, remote services, or another operating system.
`crates/izu-engine/tests/core_regressions.rs` preserves the core-review failures
and checks ancestor modes and collisions, unsupported hardlinks, live/unknown
writer captures, and source, metadata, and private-marker substitution before
publication and at `HeadReplaced`, including both closure operations.
