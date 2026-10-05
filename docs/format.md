# izu native format, version 1

This is an original format. It does not serialize Rust memory layout, enum
discriminant layout, pointers, platform endianness, or another VCS engine's
objects. SHA-256 is a standard utility primitive, not a history engine.

## Immutable object frame

An object file contains exactly one 17-byte header followed by exactly the stated
number of payload bytes. Trailing bytes and premature EOF are corruption.

| Offset | Size | Meaning |
| --- | --- | --- |
| 0 | 8 | ASCII `IZUOBJ1` followed by one zero byte (`49 5a 55 4f 42 4a 31 00`) |
| 8 | 1 | Kind tag: blob=1, tree=2, revision=3, operation=4, candidate=5, evidence=6 |
| 9 | 8 | Unsigned 64-bit payload byte count, big-endian |
| 17 | count | Exact payload bytes |

The `ObjectId` is the 32-byte SHA-256 digest of **header followed by payload**.
Its text spelling is exactly 64 lowercase hexadecimal ASCII characters. Kind and
length are part of identity. `RevisionId`, `TreeId`, `OperationId`, `CandidateId`
and `EvidenceId` are distinct typed IDs; merely wrapping a hash does not prove
object existence or kind. Loading verifies both hash and expected kind.

The magic selects frame version 1 and all five metadata schemas below. Unknown
magic/version/kind is rejected before any payload allocation or publication.
Total length `17 + count` must fit in `u64`. Blob length is streamed; it is never
converted to `usize` to allocate one buffer. Metadata length is bounded first,
then checked for a platform `usize` conversion. A frame header alone never
authorizes the stated allocation.

IZU version 1 deliberately uses a new identity domain. It rejects the prior
unreleased EZY prototype's `EZYOBJ1` object magic, `EZY-STORE 1` store marker,
`EZYHEAD1` head marker and EZY bundle framing. The shared version number does not
imply compatibility. There is no automatic migration, old-name alias, header
retagging or hash rewriting. Existing prototype directories, acknowledged objects,
old binaries and historical fixture evidence must remain untouched; renaming the
product does not authorize overwriting them. An explicit future migration would
need its own reviewed protocol and fresh acknowledgment.

## Canonical metadata JSON

Blobs contain arbitrary bytes, including empty data and non-UTF-8. All other
kinds contain exactly one canonical UTF-8 JSON object matching their version-1
record schema. Canonical bytes are defined independently of a JSON library:

1. There is no BOM, insignificant whitespace, trailing newline or trailing data.
2. Objects have unique string keys sorted lexicographically by their UTF-8 byte
   sequences. Arrays retain their specified order. Set fields have unique members
   ordered by their underlying typed order (ID bytes); maps have unique keys.
3. Strings contain Unicode scalar values encoded in UTF-8, with no Unicode or
   filesystem normalization. Quote and backslash are escaped as `\"` and `\\`.
   U+0008, U+0009, U+000A, U+000C and U+000D use `\b`, `\t`, `\n`, `\f`, `\r`.
   Other U+0000..U+001F use lowercase `\u00xx`. All other scalar values are literal
   UTF-8, including slash, DEL, U+2028 and U+2029. Unpaired surrogates are invalid.
4. Numbers are integers in `-9223372036854775808..=18446744073709551615`.
   They use ordinary decimal digits, optional minus for negatives, no plus sign,
   leading zero, fraction or exponent. Zero is `0`, never `-0`. Field types may
   impose a narrower range. Floating-point numbers are unsupported.
5. Booleans and null use exact lowercase `true`, `false` and `null`.
6. Every schema field is present, including explicit `null` for absent optional
   values and empty collections where valid. Unknown fields, duplicate keys,
   duplicate set members, reordered sets, alternate escapes, alternative enum
   spellings and invalid record invariants are rejected. Data is not accepted
   and then silently dropped or normalized.

The decoder checks canonical bytes before and after typed decoding. The second
comparison detects semantic transformations such as duplicate set members.
Object-key order is independent of Rust struct declaration order or JSON map
features. Timestamps are signed Unix milliseconds; names/descriptions remain
UTF-8. Times are data, not a substitute for ancestry or concurrent publication
order.

## Version-1 schemas

Exact fields and invariants are in [engine-contract.md](engine-contract.md) and
the authoritative `izu-model` declarations. Enum spellings are snake_case.
`TreeEntry`, `ResolvedTreeEntry`, `FileMode`, and `RevisionOrigin` use a `kind`
discriminator. `CheckOutcome` uses `status`. `ObjectKind` in manifests uses
`blob`, `tree`, `revision`, `operation`, `candidate`, or `evidence`; the binary
frame still uses its explicit numeric tag.

- A tree is a flat sorted map from exact relative paths to file, symlink,
  directory or nonrecursive conflict entries. File contents reference blob IDs;
  raw symlink-target bytes are inline lowercase hex, not a resolved target.
  Empty directories can be represented explicitly. Every slash ancestor must
  exist explicitly as a directory with its recorded mode or an unresolved
  conflict; implicit missing parents are invalid. Files or links cannot have
  descendants; directory paths may. Conflict alternatives remain immutable and
  retained until an explicit resolving revision is published.
- A revision contains an independent stable `ChangeId`, exact tree ID, ordered
  parents, UTF-8 description/author, timestamp and optional origin. The empty
  initialization baseline has explicit `bootstrap` origin. An imported Git
  origin references its exact original raw commit blob plus a validated external
  lowercase 40- or 64-character ID; it does not define native history identity.
- An operation contains its previous operation ID and the entire new repository
  view. The view holds logical change heads (including explicit divergence), named
  refs, workspace/source state, candidates and check-evidence IDs.
- A candidate contains exact expected target/source revisions, exact result
  revision/tree and requested check commands/environment identities.
- Evidence contains the candidate ID, independent 128-bit attempt token, exact
  checked revision/tree/environment, command, outcome and start/optional-end time.
  `pending` has an explicit null finish timestamp; terminal `passed`, `failed` and
  `cancelled` require a finish timestamp at or after the original start. Missing
  environment/evidence or pending execution is unknown. A token is 32 lowercase
  hex characters, distinct from candidate/content identities.

Starting a rerun first durably publishes pending evidence as the current record
for that candidate/check. Earlier pass records stay in operation history but do
not authorize the current attempt. Terminal publication compares its exact token,
bindings and original start against the locked active pending record. Pure model
comparison does not replace the engine's atomic active-pointer/source guards or
runtime verification of actual execution inputs.

All referenced objects are visited with their expected kinds using the public
`ReferencedObjects` trait. Native archive/collection must traverse the complete
closure, including retained operations, divergence, conflict alternatives,
candidate/evidence/environment blobs and imported origin blobs. Blob-as-metadata
decoding and kind mismatches fail. A metadata record's syntactic validity does
not prove its referenced objects exist or agree; the engine/store verifies those
cross-object assertions before publishing it.

## Paths and filesystem fidelity

Version 1 supports UTF-8 POSIX relative paths. `/` is the sole separator. A
literal backslash, space, newline and either Unicode normalization form remain
exact path bytes. Empty/absolute paths, empty slash components, `.` or `..`
components and NUL are rejected; they are not normalized. Any component equal
to `.izu`, `.izu-recovery`, `.ezy`, `.ezy-recovery` or `.git` ignoring ASCII case is
reserved and rejected, so imported source data cannot overwrite native, recovery,
legacy prototype or Git metadata. Legacy names are protections, not compatibility
aliases. `.gitignore`, `.izu-notes` and `.ezy-notes` are ordinary allowed spellings.
Invalid-UTF-8 filesystem filenames produce an explicit unsupported-input error.
Windows path materialization is not supported by version 1.

Native identity preserves exact case and Unicode spelling. A tree cannot contain
two namespace keys equal under NFD normalization (Unicode 17.0.0), full default
case folding (Unicode 16.0.0), then NFD again. The comparison key never replaces
stored spelling or on-disk object bytes. This catches Unicode canonical aliases
and full-fold aliases that lowercase misses, including σ/ς, Straße/STRASSE and
ſ/s. Explicit directory ancestors are subject to the same rule. The pinned
utilities are [`caseless` 0.2.2](https://docs.rs/caseless/0.2.2/caseless/) and
`unicode-normalization` 0.1.25; dependency-data upgrades require explicit review
and policy compatibility, not a silent acceptance-rule change.

The materialization boundary additionally detects aliases/collisions on its actual filesystem and
rejects unsafe results before writes. It cannot assume the filesystem provides
the native map's distinctions. Source capture/materialization must use explicit
no-follow checks; a symlink is represented as a link, never read as its target.
Symlink targets retain arbitrary nonzero POSIX bytes, including invalid UTF-8,
absolute targets and dot components, with no traversal during decoding.

`FileMode::Regular` is exactly 0644; `Executable` exactly 0755; `unix` carries
another exact POSIX permission value in 0000..7777. The two common values cannot
use the `unix` representation. Preservation does not authorize applying special
bits: materialization refuses setuid/setgid/sticky by default. Git transport has
a narrower regular/executable-bit mapping and cannot roundtrip arbitrary native
permissions without an explicit conversion policy. Ownership, ACLs, xattrs,
hardlink relationships and device/socket/FIFO objects are unsupported and must
not be silently collapsed into ordinary file entries.

Git import explicitly represents each subtree as a directory 0755, reflecting
Git's lack of separately stored directory permissions. Native source capture
records the actual directory mode instead. Neither native restoration nor a
bundle decoder adds invented ancestors to an already identified tree.

Initialization refuses any existing legacy `.ezy` or `.ezy-recovery` entry before
creating `.izu`, capturing source or writing recovery state. An entry's mere
presence, including a symlink, is sufficient for refusal; initialization cannot
reinterpret it as a current repository or silently skip it and claim a clean
adoption. The engine supplies the filesystem and no-follow evidence for this
boundary; the model supplies reserved relative-path validation.

Historical workspace absolute locators are metadata, not write/execute
authority. Restoring at a new location retains historical objects exactly and
publishes a new locally remapped operation; it does not visit old paths.

## Limits, allocation and cancellation

Default limits are 64 GiB per streamed blob; 16 MiB per metadata payload;
250,000 tree entries/changes; 1,024 exact heads per change; 64 revision parents;
10,000 workspaces; 100,000 refs/candidates; 250,000 evidence records; 1 MiB per
text/aggregate argv; and 4,096 bytes per path/symlink target. Names are at most
255 bytes, identities 1,024 bytes per field, commands/checks at most 1,024 items.
The hard metadata ceiling is 64 MiB. A configurable policy can be stricter; an
invalid or overflowing limit does not disable validation.

An allocation-free scan before JSON parsing enforces at most 64 container levels,
1,000,000 JSON nodes (keys count), and 21 bytes per number token. All input bytes
are already bounded; tiny-node expansion is independently bounded. Staging
writers check accumulated size with checked arithmetic and use fallible reserve;
hashing allocates no payload buffer. Symlink hex decoding checks its length
before fallible allocation. Container and cross-field limits are validated at
the typed boundary. Malformed/unknown/canonicality failures have typed errors.

Serde, Unicode normalization and standard-library container internals do not offer fully fallible
allocation. Their allocations are bounded by admitted bytes/nodes, but allocator
exhaustion can still abort the process instead of returning `ModelError`.
This residual is not described as recoverable allocation handling. Durable HEAD
and immutable objects must recover after process termination; the model tests
do not establish storage crash durability. A flat whole-tree and full-view
operation format also has these explicit size limits; giant-repository support
requires measured evidence or a new version, not a performance claim.

`CancellationToken` is shared and monotonic. Streaming/transaction/runtime
boundaries check it between bounded work chunks and before publication. Metadata
codec work itself is bounded rather than asynchronously interrupted. A caller
checks before/after its decode/encode phase. Cancellation or allocation failure
must not publish a partial mutable view or falsely claim an acknowledged write
did not occur.

## Durable state and schema evolution

HEAD selects one immutable operation; objects and required directory entries
must be durable before HEAD receives a durable acknowledgment. Publication checks
the new operation's parent against the locked current HEAD. Visibility after a
failed durability step is a distinct uncertainty result. Those rules are storage
semantics requiring store/engine crash tests, not guarantees established by this
format's hashing tests. The version-1 filesystem control layout is:

- `FORMAT` contains exactly `IZU-STORE 1\n` in ASCII.
- `objects/<first two hex digits>/<remaining 62 hex digits>` contains one exact
  object frame. Publication never overwrites an existing content path.
- `tmp/izu-<32 lowercase hex digits>.tmp` holds unpublished temporary data.
- `head.lock` and `temporary.lock` are permanent lock files, never replaced while
  participants may hold them. Writer/collector use belongs to the store.
- `HEAD` is exactly 72 bytes: ASCII `IZUHEAD1` (8), raw operation object ID (32),
  then SHA-256 of those first 40 bytes (32). There is no text hex or newline.
  Unknown versions, wrong length/checksum, missing objects and non-operation
  targets are rejected. The referenced operation still receives frame/hash/schema
  and closure verification.

`head-admission-v1/` is coordination outside the immutable object schema. New stores prepare its directory, permanent blank `gate.lock` and one zero-filled 56-byte `slot-0000.lock` before creating FORMAT; existing stores may still initialize missing coordination lazily. A zero-filled free slot is preparatory space, not a live ticket. The gate protects registration, and at most 1,024 permanent `slot-0000.lock` through `slot-1023.lock` files carry reusable 56-byte records: ASCII `IZUHQUE1` (8), little-endian slot number (2), zero reserved bytes (6), nonzero little-endian sequence (8), then SHA-256 of the preceding 24 bytes (32). Readers require exact length, slot, version, reserved bytes and checksum. Kernel leases, not PID or record age, determine claimant liveness; only a successful independent exclusive lease authorizes overwriting an incomplete free record. Numbering may reset when no live claim remains; a waiting request's lease prevents reset while it uses its predecessor snapshot. These files are never removed or replaced by the store. Coordination is reconstructable after process/power loss and does not change FORMAT 1 or durable history. Older binaries retain native HEAD serialization but do not participate in ordered admission.

The normative publication protocol flushes prepared immutable object data before
no-overwrite installation, flushes shard and parent directory entries, verifies
and flushes the new operation's full typed object closure, then flushes a prepared
HEAD file before its atomic replacement and flushes the repository/temporary
directories. The store applies its platform-specific durability primitive,
including Apple full filesystem sync where supported. A failure after visible
HEAD replacement returns `VisibleButUncertain`, preserving the visible receipt.
An archive bootstrap with a historical parent is a distinct operation allowed
only while the locked local HEAD is absent and the complete archived closure is
validated. These control bytes do not change framed object identities.

Version 1 rejects unknown schemas/fields/variants rather than accepting a partial
future state. Any incompatible schema change requires a new explicit format
version and migration. Migration validates old framed objects, writes new
immutable objects and an explicitly translated view, and publishes the new HEAD
durably. It never rewrites bytes under existing object IDs. Readers that do not
support a version refuse it; no implicit migration, downgrade or guessed
compatibility is permitted.

This implementation finalizes an unreleased version-1 draft with required attempt
tokens and pending evidence. Old development evidence without `attempt` is
explicitly rejected. The decoder does not fabricate a token, replay a historical
pass as current, or silently upgrade its immutable bytes. No development-to-final
migration is implemented; evidence compatibility must not be inferred from an
unchanged frame hash or magic. Incompatible changes after a published version
require the explicit version/migration protocol above.

The unreleased draft also finalizes explicit directory ancestors and conservative
canonical/full-casefold namespace alias rejection. Development trees that lack
required ancestors or contain forbidden aliases fail validation. The decoder
does not add 0755 directories, normalize names, drop one alias or rewrite an
existing tree ID. Future published compatibility changes follow the explicit
version/migration rule above.

## Fixed vectors

These are byte-for-byte test vectors, also checked by Cargo tests. Payload lengths
are UTF-8 bytes. JSON shown below contains the actual escaped JSON spelling;
`é` and `雪` are literal UTF-8, not Unicode escapes.

| Kind | Payload | Byte length | SHA-256 frame ID |
| --- | --- | --- | --- |
| Blob (1) | empty | 0 | `9856ab41499a1bffe4177de835d2c7d3ce66a8a57f0ad4921c6329cc83291587` |
| Blob (1) | ASCII `abc` | 3 | `739ba683e351a7f2d57d22b2f2f4e55fefc54fb903f32fee4ef0831b64812359` |
| Tree (2) | `{"entries":{}}` | 14 | `24d2a1d3109cdc927435777079f89db1ec1bcc2b900ad23eeedd83a1ad96cadb` |

The `abc` frame header is hex `495a554f424a3100010000000000000003`.

The following canonical primitive vector is stored as a **Blob** (1), because
its fields intentionally do not claim to be a metadata record schema. It is
91 bytes, with ID
`ede0676c5ced754c53ccef40826bfe6f34553dcf1891fa91fdc18428bb87f8ed`:

```json
{"max":18446744073709551615,"min":-9223372036854775808,"text":"é/雪\u0000\b\t\n\f\r\"\\"}
```

The following valid Revision (3) payload is 271 bytes, with ID
`8bff10ea806eb0f8b3121c09574fac3dd191e8c75e053e3145d5e9b7cec29f19`:

```json
{"author":{"email":"a@b","name":"Ézzy"},"change":"01010101010101010101010101010101","created_at_unix_ms":-9223372036854775808,"description":"é/雪\u0000\b\t\n\f\r\"\\","origin":null,"parents":[],"tree":"0202020202020202020202020202020202020202020202020202020202020202"}
```

The code-block formatting newline is not part of either vector. Hash equality
does not establish existence of the Revision vector's referenced tree.

The following valid Pending Evidence (6) payload is 509 bytes, with ID
`99d5dc1d860a498c9a20f02b998a42b47071f3a5855262a220df7618ab07b492`:

```json
{"argv":["cargo","test"],"attempt":"07070707070707070707070707070707","candidate":"0606060606060606060606060606060606060606060606060606060606060606","check":"tests","finished_at_unix_ms":null,"inputs":{"environment":"0505050505050505050505050505050505050505050505050505050505050505","revision":"0303030303030303030303030303030303030303030303030303030303030303","tree":"0404040404040404040404040404040404040404040404040404040404040404"},"outcome":{"status":"pending"},"started_at_unix_ms":-9223372036854775808}
```

Its references are illustrative IDs; they still require closure verification.
Valid canonical pending bytes do not establish a passed check or permission to
launch a process.
