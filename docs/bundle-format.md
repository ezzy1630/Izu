# izu native bundle, version 1

An izu bundle is one local, language-independent artifact containing an exact
immutable operation and its entire transitive native object closure. It is not a
network transport, synchronization protocol, running-process snapshot, database
backup, cache backup, or copy of every file currently present on disk.

The root is an already captured operation. Working source edits made after that
operation, ignored source that was never captured, unpublished immutable objects,
unreachable objects, external data, ownership, ACLs, extended attributes, hardlink
identity, special files, and local configuration outside the native object graph
are omitted. `bundle create` does not create a new checkpoint. Capture the desired
source with the shared engine before choosing the root. Partial or selective
projection is unsupported in version 1; there is no partial bundle labeled as a
complete backup.

The closure contains captured source bytes, native file and directory permissions,
inline symlink targets, revisions and their ordered parents, stable change heads,
named references, source/workspace records, all parent operations, candidates,
every recorded evidence outcome, check commands, referenced environment blobs,
and original Git commit blobs named by native revision origins. Historical
absolute workspace locations and other metadata remain intact. Sensitive source
or metadata already captured in this graph is also included. Bundling does not
automatically contact a network service or gather credentials from the machine.

The artifact is a private, complete native archive, not a secret-filtered sync
artifact. Historical ignored recovery files such as `.env` and referenced
environment blobs are included if they were captured as native objects. No
privacy filter removes fields or bytes while retaining their original hashes.
Generated environment caches under `.izu/environments/v1`, incomplete outputs
under `.izu-recovery/environment-staging`, and other uncaptured runtime files are
not copied merely because they exist on disk. A bundle is never implicitly
uploaded; sharing it is a separate explicit action by its owner.

## Wire layout

All integer lengths and counts are unsigned big-endian values. Nothing depends
on Rust struct layout, enum representation in memory, filesystem filenames, an
archive extraction utility, compression, or a host endianness.

| Field | Bytes | Meaning |
| --- | ---: | --- |
| Magic | 8 | `49 5a 55 42 4e 44 31 00`, ASCII `IZUBND1` then NUL |
| Bundle version | 2 | Exactly `1` |
| Flags | 2 | Exactly `0`; unknown flags are rejected |
| Manifest length | 4 | Exact byte length of the following canonical JSON |
| Manifest | variable | Native canonical JSON, schema below |
| Object records | variable | Exactly `object_count` records, schema below |
| Trailer magic | 8 | `49 5a 55 45 4e 44 31 00`, ASCII `IZUEND1` then NUL |
| Trailer digest | 32 | SHA-256 of all bytes preceding the trailer magic |
| End | 0 | EOF is required; trailing bytes are rejected |

The canonical manifest has exactly these mandatory fields:

```json
{"bundle_schema":1,"native_schema":1,"object_count":1,"payload_bytes":123,"root_operation":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","scope":"root_closure"}
```

The numbers and identity above are illustrative. `object_count` is positive and
equals the actual number of records. `payload_bytes` is the checked sum of their
payload lengths. `root_operation` is an exact lowercase, 64-digit native operation
ID. Unknown fields, schemas, scopes, duplicate keys, noncanonical integer/string
forms, and noncanonical JSON are rejected. Canonical JSON follows the native
codec: compact JSON, sorted UTF-8 object keys, integer numbers only, and the
documented `serde_json` string escape convention.

Each record is the object's 32 raw identity bytes followed by its complete native
frame: eight bytes `IZUOBJ1` plus NUL, a one-byte object kind, an eight-byte payload
length, and exactly that payload. Kind tags are blob `1`, tree `2`, revision `3`,
operation `4`, candidate `5`, and evidence `6`. The identity is SHA-256 over the
complete native frame, including its kind and length. Metadata must satisfy its
original native schema and canonical encoding. Fields cannot be removed or
rewritten while retaining an old hash.

Native schema 1 denotes the finalized native record schema. Earlier development
records missing mandatory check-attempt tokens or explicit parent directory
entries are rejected. Bundling does not silently migrate their bytes or identity.

The writer orders records by raw object identity for reproducibility. Consumers
accept any record order. Every identity occurs once; even an identical duplicate
is rejected. Different bytes claiming the same identity are rejected too.

## Validation and resource policy

Verification checks header/version/flags, the manifest schema, every native frame
kind/length/hash, canonical typed metadata, exact record and byte totals, trailer,
EOF, and the complete rooted closure. Every graph edge names its required kind.
Missing objects, unexpected kinds, unreachable extra records, cycles, asserted
revision/tree mismatches, logical-change/head mismatches, and evidence-map/candidate mismatches are errors.
Conflict entries retain and traverse the file blobs in every alternative.

Metadata is decoded only after its length is bounded. Blobs are hashed and copied
in at most 64 KiB chunks; their payloads are never buffered as whole objects by
the bundle codec. Traversal is iterative and bounded. Checked arithmetic is used
for counts, lengths, byte totals, depth, and offsets. Allocation for metadata,
object indexes, references, and traversal is reserved fallibly. Cancellation is
checked during traversal, object copying, verification, staging, and immediately
before publication. Cancellation after publication cannot undo a durable result.

Default bundle budgets are one million objects, 64 GiB total artifact bytes,
64 KiB manifest bytes, four million reference edges, 512 MiB charged graph
allocation, and 100,000 graph depth. Native policy separately defaults to 16 MiB
per metadata payload and 64 GiB per blob. The charged graph budget includes node,
edge, queue, binding, ordering, traversal, conservative index overhead and a
conservative charge for the retained root-operation record; the bounded metadata
decoder's temporary allocations are additional. Consumers can lower budgets
without changing object identity.
Destination materialization also observes the engine's source limits; an archive
being well formed does not imply every destination can materialize it.

`inspect` checks only the bounded header and manifest. Its `Inspection` result is
not a verification result. `verify` checks the entire artifact and returns the
root, counts, byte totals, depth, and trailer SHA-256. A digest proves integrity
against the artifact's own identities, not the author's identity or authenticity.

## Creation and restoration

Creation writes a private mode-0600 owned temporary file in the destination's
directory. It checks every immutable source identity, persists the completed
file through `izu-platform`, creates the final entry atomically without replacing
any existing entry, removes staging, and persists the parent directory. A
`Publication::Durable` receipt is produced only after those barriers succeed.
If a post-publication persistence step fails, the receipt instead states
`VisibleButUncertain`; callers must not present it as a durable acknowledgement.
The directory-relative platform boundary rejects symlink path components and
unsupported publication platforms. Creation and restoration reject a known
volatile destination filesystem before creating staging entries. The actual
file, staging directory and parent descriptors are classified again at their
persistence boundaries. A successful kernel flush on tmpfs is not a durable
archive or restoration acknowledgement. Classification rejects known volatile
types; it cannot prove hardware reliability beneath another filesystem type.
On macOS this also rejects filesystem aliases such as `/tmp` and `/var`; supply
their canonical parent locations (for example `/private/tmp`) or an ordinary
relative path inside the current directory.

Cold restore requires a new destination. An existing file, symlink, empty
directory, or repository is protected. The complete artifact is verified before
any staging repository is initialized. Verified objects are imported into an
owned mode-0700 sibling directory using the same open input descriptor and exact
expected identities. Concurrent path replacement cannot substitute another input
file; a payload changed in place must still pass its expected native hash during
import.

The shared Rust engine owns all history mutation. It initializes an empty native
store through the pinned staging directory descriptor, verifies and seeds the
archived operation under an absent-HEAD guard,
materializes the selected captured working tree into staging, and publishes a new
recovery operation whose parent is the archived root. That operation retains
the archived source/history identities while replacing the active workspace
registration with one selected workspace located at the new final path. Archived
operations themselves remain byte-for-byte unchanged and reachable in operation
history. Other historical workspace paths are never used as output locations.
Multiple-workspace archives require an explicit selection.

After the engine completes, the staged directory is persisted and atomically
renamed using the platform's no-replacement primitive. The parent directory is
then persisted. Unsupported paths or case collisions, corrupt input, resource
exhaustion, cancellation, and pre-publication failures expose no active partial
repository at the destination. On failure, a nonempty private stage is retained
for inspection and recovery; `staging_retained` reports its exact path, original
failure, and recovery operation when available. Unknown files added by another
writer are preserved. Only an empty stage whose name still identifies the
original opened directory is removed. A changed stage identity prevents cleanup
and reports `staging_cleanup`; replacement entries are preserved. If a parent or
stage locator was replaced, the reported path is the original diagnostic locator
and must not be treated as authority to delete its current entry. A failure after
visibility again returns `VisibleButUncertain` with the exact recovery receipt.

Initialization, immutable imports, workspace locks, and source materialization
write through the original staging descriptors. The caller's staging path is a
locator, not a second write authority. Publication and disposal check the
original device/inode identity while its descriptor remains live. The parent
locator and final destination identity are also checked before a durable receipt.
A detected visible identity mismatch returns `publication_uncertain` instead of
a receipt describing unrelated bytes as verified source.

These checks cover cooperative local use and detected namespace replacements.
They do not claim isolation from an adversarial process with the same user
permissions that changes an entry between individual filesystem system calls.
The platform's portable hard-link operation is relative to pinned directories
but still names its source entry; identity checks occur before and after linking.
There is no portable inode-conditional unlink or source-link primitive in this
contract. Nonempty failed restore stages are never recursively deleted.

Destination path names come from the caller and validated native `RepoPath`
records, never archive member names. Historical absolute workspace roots are
metadata, not authority to write there. Source materialization and platform case
support belong to the shared engine. Source edits made after the acknowledged
root are never claimed to be present. An artifact stored only beside the original
repository shares that storage's failure domain; transporting the artifact to an
independent location is an explicit user action.

## Rust API

`izu_bundle::create(source, root, output, options, cancel)` accepts either the
shared `Repository`, an immutable `Store`, or an `ObjectSource` implementation.
It returns a `BundleReceipt` with an explicit publication state and a full
verification report. `inspect(path, options, cancel)` returns `Inspection`;
`verify(path, options, cancel)` returns `VerificationReport`.

`restore(input, destination, RestoreOptions, cancel)` returns
`RestorationReceipt { destination, publication, archive, recovery }`.
`RestoreOptions` contains bundle budgets, shared-engine repository options, and
an optional selected workspace ID. Its recovery receipt identifies the actual
engine publication. No CLI, API, or bundle adapter implements a second history
engine.

Development-only fault hooks expose file/directory persistence and publication
boundaries for cancellation, ENOSPC and visibility-uncertainty tests. The feature
cannot be enabled in a release build. Fault tests exercise error contracts; they
are not evidence of a physical power-loss test or storage-device guarantees.
