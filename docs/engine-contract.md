# Engine and persistence contract, native version 1

The authoritative Rust declarations are `crates/izu-model/src/lib.rs` and its
modules. This document describes the shared meaning; adapters must not invent
parallel history mutation or identity types.

The product and current native identity domain are izu/IZU. The prior unreleased
EZY prototype is deliberately incompatible; its object/store/head/bundle magic
is refused, with no old-name alias or automatic migration. Old acknowledged
artifacts, binaries and historical fixture evidence remain unchanged. Relative
paths reserve `.izu`, `.izu-recovery`, legacy `.ezy`, legacy `.ezy-recovery` and
`.git` in every component ignoring ASCII case. Engine initialization refuses
legacy metadata/recovery entries before any current-store or source write.

## Identities and metadata

- `ObjectId` is SHA-256 over a complete native frame. `TreeId`, `RevisionId`,
  `OperationId`, `CandidateId`, and `EvidenceId` are distinct wrappers, not
  interchangeable identifiers. They expose `from_object` and `object_id`.
- `ChangeId`, `WorkspaceId`, and `CheckAttemptId` are independent 128-bit identities. Creation
  obtains unpredictable bytes from a platform entropy boundary; byte
  construction itself does not pretend to provide randomness.
- IDs have `from_bytes`, `as_bytes`, `FromStr`, `Display`, and validated serde.
  Text is exact lowercase hexadecimal; abbreviated IDs are an interface lookup
  concern, never an on-disk identity.
- `RefName::new`, `RepoPath::new`, and `SymlinkTarget::new` validate input without
  normalization. String wrappers expose `as_str`; targets expose `as_bytes`.
- `Validate::validate(&self, &Limits)` checks record invariants. Persistence uses
  `encode_metadata<T: Serialize + Validate>(&T, &Limits)` and
  `decode_metadata<T: DeserializeOwned + Serialize + Validate>(&[u8], &Limits)`.
  Both return `Result<_, ModelError>`. The decoder also requires canonical bytes.

## Exact field contract

All fields below are public. Constructors and boundary encoders validate public
record fields; record serde rejects unknown fields and invalid invariants.

```rust
Tree { entries: BTreeMap<RepoPath, TreeEntry> }
TreeEntry::File { blob: ObjectId, mode: FileMode }
TreeEntry::Symlink { target: SymlinkTarget }
TreeEntry::Directory { mode: FileMode }
TreeEntry::Conflict {
    base: Option<ResolvedTreeEntry>, ours: Option<ResolvedTreeEntry>,
    theirs: Option<ResolvedTreeEntry>, reason: ConflictReason,
}
ResolvedTreeEntry::{File { blob: ObjectId, mode: FileMode },
                   Symlink { target: SymlinkTarget }, Directory { mode: FileMode }}
ConflictReason::{Content, Binary, DeleteModify, AddAdd, Mode, Type, Path}
FileMode::{Regular, Executable, Unix { permissions: u16 }}

Identity { name: String, email: String }
Revision {
    change: ChangeId, tree: TreeId, parents: Vec<RevisionId>,
    description: String, author: Identity, created_at_unix_ms: i64,
    origin: Option<RevisionOrigin>,
}
RevisionOrigin::{Bootstrap, Git { object_id: String, raw_commit: ObjectId }}
ChangeState { heads: BTreeSet<RevisionId> }
SourceRecord { path: Option<RepoPath>, tree: TreeId, revision: Option<RevisionId> }
WorkspaceRecord {
    name: String, root: String, head: RevisionId,
    sources: BTreeMap<String, SourceRecord>,
}
RepositoryView {
    changes: BTreeMap<ChangeId, ChangeState>, refs: BTreeMap<RefName, RevisionId>,
    workspaces: BTreeMap<WorkspaceId, WorkspaceRecord>,
    candidates: BTreeSet<CandidateId>,
    evidence: BTreeMap<CandidateId, BTreeSet<EvidenceId>>,
}
Operation {
    parent: Option<OperationId>, view: RepositoryView,
    description: String, created_at_unix_ms: i64,
}
CheckSpec { name: String, argv: Vec<String>, environment: Option<ObjectId> }
IntegrationCandidate {
    target: RefName, expected_target: Option<RevisionId>,
    sources: Vec<RevisionId>, result: RevisionId, result_tree: TreeId,
    checks: Vec<CheckSpec>, created_at_unix_ms: i64,
}
CheckInputs { revision: RevisionId, tree: TreeId, environment: Option<ObjectId> }
CheckOutcome::{Pending, Passed, Failed { exit_code: Option<i32> }, Cancelled}
CheckEvidence {
    candidate: CandidateId, attempt: CheckAttemptId, check: String, inputs: CheckInputs,
    outcome: CheckOutcome, argv: Vec<String>,
    started_at_unix_ms: i64, finished_at_unix_ms: Option<i64>,
}
```

`RepositoryView` and `Tree` implement `Default` for empty construction.
`ChangeState::resolved`, `single_head`, and `is_divergent` expose the explicit
one-head and multiple-head cases. Parent vectors retain their order; duplicate
parents and empty change-head sets are invalid. A revision is immutable, while a
logical change may retain several exact heads. Sorted set ordering never chooses
a winner.

Every slash ancestor of a nested tree entry must be explicitly present as a
`Directory` with its exact mode, or as an unresolved `Conflict`. Missing ancestors
are rejected; capture/materialization never invents an implicit 0755 parent under
an existing tree identity. Selected source capture retains existing parent modes,
and a newly needed parent records its actual source mode. Git import explicitly
maps each subtree to directory 0755 because Git does not store an independent
directory permission mode; that narrower transport policy is documented below.

Tree namespace aliases are rejected on every host using the shared
`RepoPath::namespace_key`: NFD normalization (Unicode 17.0.0), full default Unicode
case folding (Unicode 16.0.0), then NFD again. It preserves exact stored path
spelling and blocks composed/decomposed names, sigma variants, Straße/STRASSE and
long-s/s aliases. Explicit ancestors expose ancestor aliases to the same full-key
check. This is a conservative portable namespace policy, not a claim to model
every host filesystem lookup rule. The engine additionally checks actual local
filesystem collisions before writes.

Conflict alternatives are immutable and nonrecursive; at least one alternative
must exist. A conflicted tree/revision can be durably captured and archived.
Materialization, integration publication and Git export reject unresolved
conflicts. Explicit resolution creates a new immutable tree and revision rather
than editing an earlier object. File blobs in every alternative remain retained.
If a conflict is an ancestor path, resolving it must also produce a tree with
valid descendant relationships.

`RevisionOrigin::Bootstrap` explicitly marks the empty native initialization
baseline. It has no revision parents; the engine also verifies its tree is empty.
Git export skips only bootstrap ancestors and rejects a bootstrap export tip.
Real empty commits remain real commits. No author or description heuristic is
permitted. Checkpoint/capture updates source-tree state without advancing the
workspace history baseline or named references. Intentional commit publishes a
new exact revision and advances the selected baseline; these state changes
distinguish intent without parsing an informational operation description.

A source with `path: None` represents the workspace root; `Some(path)` is a
relative subtree. `WorkspaceRecord.head` is its exact history baseline.
`sources["working"].tree` can record a later captured working tree. A source's
`revision: Some(id)` asserts that the referenced revision has exactly that tree;
the engine verifies that assertion by loading the referenced object.

## Durable concurrent state

An immutable operation embeds the complete new repository view and its previous
operation ID. One durable HEAD chooses the current operation. The store verifies
object kind and content identity; operation publication verifies its parent is
the current locked HEAD. An initial operation has no parent and requires no HEAD.

The engine acquires the store's exclusive transaction lock, reads the latest
operation, checks the expected workspace/source/ref state, applies its change to
that latest view, writes immutable objects, and publishes the next operation.
An unrelated writer's changes must survive. A stale expectation returns a typed
conflict or retries after rereading; it cannot silently overwrite the view.
Restoration/undo publishes a new operation whose parent is the current HEAD and
whose view preserves intervening work according to the requested scope.

Objects and their directory entries must be durable before HEAD is acknowledged.
The store must persist HEAD's replacement directory entry too. If replacement is
visible but durability is uncertain, it reports that distinct state; an ordinary
failure must not imply nothing happened. Cancellation before publication does
not publish a partial view. Cancellation after a durable publication cannot make
the acknowledged operation disappear.

Large blob streams may run outside the short publication lock only when the
store's writer lease or equivalent protects unpublished objects from collection.
Until that protocol is implemented, put them while the appropriate retention
lock is held. Garbage collection and writer publication cannot race unchecked.

## Workspace locations and restore

`WorkspaceRecord.root` is a local UTF-8 absolute POSIX locator retained in the
historical operation. Its presence is not authority to write or execute there.
Repository discovery identifies the actual current local workspace before any
materialization or process launch. A clone/archive restore at another location
preserves historical operations exactly, then publishes a new local operation
with explicit workspace-location remapping. It must not materialize old absolute
locations from an imported operation. Stale or ambiguous locations fail until an
explicit workspace selection/remap is provided.

## Candidates and evidence

Candidates are immutable objects containing exact source revisions, expected
named-reference state, result revision and tree, and the complete check commands.
`expected_target: None` means the named reference must still be absent. Before
publication the engine rereads all guarded source state, verifies the result
revision's tree, and verifies the target still matches. Checking a source or an
earlier candidate does not prove the integration result.

Evidence names its immutable candidate, fresh attempt token, exact revision/tree,
argv and optional environment digest. A missing digest means environment identity is unknown, not
equal to any expected environment. Evidence is accepted only when candidate,
check name, command, revision, tree, required environment and successful outcome
all match. Missing evidence remains unknown; a candidate with no check specs
makes no test-passed claim. Runtime execution must use the exact candidate tree
and report process failure, cancellation or successful completion truthfully.
`CheckEvidence::validate_for_candidate(id, candidate)` implements those exact
identity, command, required-environment, valid lifecycle and successful-outcome
comparisons. `validate_inputs_for_candidate` checks the same requested bindings
for a pending or terminal record without claiming execution succeeded.

Before launching a check, the engine generates a fresh `CheckAttemptId`, writes
an immutable `Pending` evidence record, and publishes an operation replacing the
current view's prior record for that candidate/check. Pending requires
`finished_at_unix_ms: None`; every terminal outcome requires `Some(time)` at or
after its original start. Invalid combinations fail record serde and the native
codec. These are validated public record fields, not a claim that every invalid
Rust field combination is impossible to construct.

The current view contains exactly one active evidence record per candidate/check
name. Earlier pending and terminal records remain reachable through earlier
operations, preserving audit history. Integration considers the current active
record only; it must never search history for any previous pass. If launch,
execution, cancellation or publication crashes, pending evidence remains unknown
and prevents the old success from authorizing integration. A rerun creates a new
token and atomically replaces the active record before launch.

A terminal result preserves the pending record's attempt, candidate, check,
inputs, argv and original start time. `terminal.validate_for_pending_attempt`
rejects older tokens, changed bindings, non-pending active records and nonterminal
completions. Under the publication lock the engine rereads the active evidence
pointer and source guards, invokes that comparison, then replaces only the exact
current pending attempt. An earlier process cannot overwrite a newer attempt or
restore an older pass. Runtime still must verify the actual exact tree/environment
and detect mutation; writing these asserted IDs alone does not prove it executed
unchanged inputs. Active-attempt compare-and-publish requires engine/store tests;
model tests establish the pure schema and comparison behavior.

The attempt fields finalize the unreleased version-1 draft. Prior development
evidence lacking an attempt token is rejected, not assigned a fabricated token or
treated as current successful evidence. No implicit legacy upgrade or migration
is implemented. Published incompatible schemas would require a new format version
and explicit migration as described in the format contract.

## Object graph and retention

Closure traversal verifies every referenced object with its expected kind:

- Trees reference file blobs, including all resolved conflict alternatives;
  symlink targets and directory modes are inline.
- Revisions reference their tree, ordered parent revisions, and optional Git
  origin raw-commit blob.
- Operations reference their parent operation plus all view changes' revision
  heads, ref revisions, workspace heads, source trees/revisions, candidate IDs,
  and evidence IDs. Evidence-map candidate keys reference candidates too.
- Candidates reference expected-target/source/result revisions, result tree and
  optional check-environment blobs.
- Evidence references its candidate, exact input revision/tree and optional
  environment blob.

Archive validation or collection cannot assume the working tree alone covers
the closure. Retained operation history, divergence, candidates, evidence,
reflike names and all live/unpublished writer roots require retention. Arbitrary
environment manifests are immutable blobs whose exact bytes determine identity.
`ReferencedObjects::visit_references` enumerates these typed edges without
allocating. `decode_object_metadata` dispatches by the exact framed kind and
rejects blob-as-metadata decoding. Callers deduplicate edges and enforce object
existence, kinds, hash integrity and retention policy.

## Native modes and Git interoperability

`Regular` means exactly `0o644`, `Executable` exactly `0o755`;
`Unix { permissions }` preserves another permission value in `0o0000..=0o7777`.
Use `FileMode::from_unix_permissions` and `unix_permissions`; the constructor
chooses the unique representation. Native import/export preserves all supported
POSIX bits, including special bits. Preserving bytes does not grant permission
to apply them. Materialization rejects special bits (`0o7000`) by default;
separately authorized opt-in would require explicit support and verification.
It must not silently grant setuid/setgid execution permissions from imported
history or silently normalize them away. Ownership, ACLs, xattrs, hardlink identity and
special device/socket/FIFO objects are unsupported in version 1 and must never be
silently treated as ordinary files.

Git's regular/executable mode mapping is narrower. Git import can retain exact
standard Git modes and records every subtree as an explicit directory 0755;
export of other native permission modes must state and apply
an explicit policy or reject the conversion. It cannot promise arbitrary native
mode roundtripping. Original Git commit bytes live in a separate blob; raw
author/committer records, timestamp offsets, message bytes and extra headers
remain recoverable there. Native textual fields are UTF-8; an adapter rejects
unsupported non-UTF-8 textual imports rather than silently converting them.

No Git engine code or another VCS implementation defines the native object
identity, history engine, or these types.

The namespace utility dependencies are pinned `caseless` 0.2.2 (full-fold data
Unicode 16.0.0) and `unicode-normalization` 0.1.25 (normalization data Unicode
17.0.0). The current normalizer resolves safe-Rust `tinyvec` 1.13.3. Source review
found no unsafe or FFI in `caseless`; normalization denies unsafe and `tinyvec`
forbids it. Folding uses a bounded iterator with a two-character queue; collected
keys use fallible reserve. Standard-library and normalization internal allocation
limits remain as described in the format contract. These are general Unicode
utilities, not a VCS engine. Version assertions protect the namespace policy from
a silent dependency-data change.
