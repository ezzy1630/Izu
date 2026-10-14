# Git interoperability

The native engine is an original Rust implementation with SHA-256 objects and
native operations. It does not depend on Git implementation code or a Git
library. `izu-git` is an optional adapter that runs an explicitly installed Git
executable through the shared bounded process worker. Native commands work
without Git; adapter construction reports a missing executable or worker
capability explicitly.

## API and authority

`GitAdapter::new(GitToolConfig)` resolves the Git executable before reading a
source. Supply `worker: Some(WorkerLauncher { executable, prefix_args })`; the
production facade uses its own executable with `__process-worker`.

- `inventory(source, cancel)` inventories ordinary refs and reachable source
  objects without publishing native history.
- `inventory_with_options(source, options, cancel)` checks explicit branch/tag
  selections and reports every advertised ref outside that scope.
- `import_into(repository, source, options, cancel)` and `fetch_into` prepare
  immutable native objects, then call the engine's atomic `import_history`.
- `clone_into(destination, source, options, cancel)` requires a new or empty
  destination, inventories first, imports through the engine, and materializes
  the advertised default branch through the engine's guarded restore operation.
- `export_to(repository, target, options, cancel)` requires a new or empty local
  output and builds a bare Git repository.
- `push_ref(repository, request, cancel)` publishes one explicitly selected branch or tag
  into an existing bare local target or an explicit HTTPS or scoped SSH target.

Sources are `GitSource::local(path)`, validated `GitSource::https(url)`, or
`GitSource::ssh(url)`. SSH requires canonical `ssh://user@host[:port]/path` syntax
and an explicit `GitToolConfig.ssh: Some(SshTransport { url, executable,
identity_file, known_hosts_file })`, scoped to that exact URL. These three tool
and credential paths must be absolute nonsymbolic regular files. Identity files
are limited to 64 KiB and known-hosts files to 1 MiB. The installed OpenSSH tool
uses strict host-key checking, the selected identity and known-hosts files,
batch mode, and no ambient SSH configuration, identity agent, forwarding, proxy,
connection sharing, password prompt or host-key updates. SCP syntax, shell
metacharacters, embedded passwords, queries, fragments and normalized dot
segments are refused. HTTPS URLs must be canonical. The adapter does not create remote
repositories, modify credentials or permissions, execute
source hooks, or load user credential helpers. Library callers can supply
`authentication: Some(HttpsAuthentication::basic(url, username, password))` for
one exact HTTPS repository URL. The credential exists in memory, has redacted
diagnostics, is not persisted, and redirects are refused. The facade accepts
explicit HTTPS sources and scoped authentication; the CLI reads a token only
from an explicitly named environment variable. Disposable authenticated GitHub clone, fetch, leased publication and independent
readback have been exercised; protection, scoped denial, SSH and final artifact
acceptance remain release gates. Additional DER trust anchors are explicit configuration,
limited to eight certificates of 64 KiB each; there is no TLS verification bypass.

Each imported native ref has an explicit expected previous native revision.
Both `ImportOptions.refs` and `ImportOptions.tags` empty creates identically named
new branches and tags; existing targets require explicit exact leases. Whole
adoption refuses unsupported ref namespaces. Explicit selections check only the
selected branch/tag graphs and return every other advertised ref
in `GitInventory.omitted_refs`. Unrelated tags and pull refs therefore do not
block an explicit branch selection, and are not claimed to have been adopted.
Default ref naming requires portable native ref names; callers can explicitly
map another supported Git branch name to a portable native name. The clone path
leases only the exact internal initialization ref that it just created. Native
history changes always use the engine, including clone restoration.

`PushRequest.expected_old` is required by the type. `None` explicitly means that
the remote ref must be absent. No tracking ref or remembered lease is inferred.
The default non-fast-forward policy rejects replacing divergent history.
`ExplicitAllow` still requires the exact supplied lease. Existing local, HTTPS and scoped SSH
targets are published through Git receive-pack after the ancestry policy
has passed. Local push uses an exact `--force-with-lease=ref:old`; HTTPS sends
the exact expected old ID in its receive-pack update command. Target repository
hooks and receive policy remain authoritative. System/global Git configuration
is deliberately excluded; this preserves the target repository's policy, not
configuration inherited from the invoking user's environment. Every successful
publication rereads the explicit target ref. A failed or interrupted publication
that may have become visible reports uncertainty, including remote/ref,
expected old ID, attempted new ID and observed readback. A changed readback
does not prove that an interrupted push never became visible. Only a complete
well-formed negative receive-pack receipt or trusted Git porcelain rejection is
treated as a definite rejection. Uncertainty is not retried automatically.

## Mapping

Git SHA-1 IDs and native SHA-256 IDs are distinct types. Import stores each
original commit payload as a native blob and records its Git ID in
`RevisionOrigin::Git`. Imported change IDs use a versioned Git-ID namespace.
Export independently rebuilds blobs and trees and resolves ordered parents. It
reuses original commit bytes only when tree, ordered parents, message, author
and author time still match the native revision. Unchanged supported commits
therefore retain their complete author/committer headers, timezones, message
bytes and original Git IDs. `gpgsig` and `gpgsig-sha256` headers, including their
continuation bytes, are preserved by this same identity check. Their payloads
are opaque: import/export does not verify signatures, trust keys or claim that
a signed author identity has been authenticated. Rewritten commits never copy
an original signature onto changed content; export reports this loss when an
origin-bearing revision has changed. `mergetag` headers use the same raw-byte
preservation path, including every continuation prefix, embedded tag message
and opaque signature byte. Import validates the embedded annotated tag headers,
requires a declared commit target matching one of the merge's parents, and
bounds both commit bytes and merge tag count before native publication. It does
not authenticate embedded signatures. Rewritten commits omit the original
merge tags and report that loss; an old attestation cannot attach to changed
parents or content.

New or rewritten commits require an explicit `GitSignature` for the committer.
The native author identity is retained. Native author time is emitted in UTC;
Git's second precision truncates native milliseconds and is reported as a
warning. Internal `RevisionOrigin::Bootstrap` parents are omitted from Git source
history, and an attempt to export the bootstrap tip is rejected. Intentional
empty commits remain ordinary commits.

Files retain Git regular/executable modes. Symlink targets remain raw bytes and
are never followed while importing or exporting. The engine handles source
materialization with its safe filesystem boundary. Git directory entries map
to the conventional native mode 0755 because Git records directory kind, not
POSIX permissions. Native file modes other than 0644/0755 and directory modes
other than 0755 are rejected rather than silently weakened. Empty directories
can exist as Git tree entries, but normal Git checkout omits them; export reports
that limitation. Unresolved native conflicts cannot be exported.

## Capability limits

The first-release compatibility scope targets native macOS/Linux storage and
ordinary complete SHA-1 branch/tag graphs, including merge ancestry, empty
commits, annotated/nested/signed tags and unchanged signed commit bytes. Source
paths use portable UTF-8 names and textual metadata is UTF-8; regular/executable files
and symlinks retain their Git modes. HTTPS and explicitly scoped SSH require
real-remote acceptance. This scope does not claim complete Git adoption:
submodules, automatic LFS hydration/upload and Git clean/smudge/text/EOL/ident
working-tree conversions are deferred.
`.gitattributes` files are ordinary preserved blobs. Import reads committed
object bytes, not a filtered working tree, and native materialization does not
run Git clean/smudge filters or apply text/EOL/ident attributes. Export writes
the same attribute bytes; a subsequent ordinary Git checkout applies Git
attributes and may produce different working-tree bytes (for example CRLF).
Attribute-referenced filter executables are never installed or executed by IZU.
Configured source filters remain refused, and Git LFS pointers remain rejected
rather than being mistaken for their external large-file content. A library
caller may explicitly select `GitToolConfig.lfs_policy = LfsPolicy::PreservePointers`
to import/export the exact committed pointer blobs. Inventory then warns that
external LFS content is unavailable; export/push warns that no external LFS
objects were uploaded. Native checkouts contain pointer bytes in this mode.
This is lossless pointer interchange, not LFS hydration or upload support.
Within the selected graphs, before native publication, it rejects
unknown commit headers, non-UTF-8 textual fields,
Git SHA-256 repositories, shallow history, grafts, replace refs,
object alternates, unknown repository extensions, configured
filters, Git LFS pointers without an explicit pointer policy, and submodules.
Ordinary working repositories, bare repositories, separate Git directories
with absolute or relative regular `.git` files, and linked worktree sources
with bounded regular `commondir` files are supported. Linked sources read refs
and objects from the common directory and HEAD from the selected worktree;
source working files, configuration and hooks are never executed. Detached
HEAD inventory is supported; clone still requires an advertised default branch. Rejected metadata is reported in the
inventory or a typed error; it is not stripped and reported as a successful
adoption. Whole-repository adoption includes ordinary branches and tags;
other ref namespaces are rejected. Explicit selections report every omitted
branch, tag or other ref. Namespace aliases are checked
with the native model's Unicode normalization and full folding before clone
initialization. Existing unrelated target refs are not changed by a one-ref push.

For local input the adapter reads bounded source metadata directly, rejects
special files and symbolic metadata entries, and places an object alternate in
an owned mode-0700 bare cache. Only the owned cache's configuration is used by
Git object reads. Loose objects are installed through descriptor-anchored
filesystem operations with no overwrite when creating a new owned export.
Before a push, existing target objects that collide with the intended IDs are
read and verified through a target-only cache. After receive-pack succeeds, the
intended target closure is verified again independently of the export cache.
Targets must be bare, without `core.worktree` or linked worktree registrations.
Inventory failures after a visible publication report uncertainty.

Default bounds are 128 MiB compressed pack bytes, delta depth 64, 16 MiB per
object, 128 MiB total inflated pack payload and independently 128 MiB total
reconstructed pack payload, 50,000 loaded objects, 10,000 refs, 100,000 entries
per flattened tree, depth 128, 1 MiB source config/packed refs, 64 KiB stderr,
and a 30-second command deadline
plus a bounded process cleanup grace. Cancellation is explicit. stdout is
bounded separately for each command. These are ingestion limits, not scale
performance claims. The shared runner retains its leader through group cleanup,
uses nonblocking pipes, and reports incomplete cleanup; it is cooperative
process ownership, not an OS sandbox against descendants changing session,
group or UID. macOS/Linux are the initial platform boundary.

## HTTPS exchange and resource boundary

The original adapter implements Smart HTTP protocol v0 over verified TLS,
including bounded pkt-line discovery, upload-pack and receive-pack
`report-status`. It requests complete self-contained packs and does not request
thin packs, partial clone filters or shallow history. The protocol and pack
layouts are public interchange formats, independent of the native format.
The same original v0 framing, pack admission, exact ref leases and receive-pack
receipts serve SSH through ordinary remote `git-upload-pack` and
`git-receive-pack` invocations. The adapter consumes each SSH session's initial
advertisement before parsing its bounded transfer response. It does not
require stateless-RPC command options from SSH servers. Protocol v2 and dumb
HTTP remain explicit missing capabilities.

Responses are read in bounded chunks before any pack is written to disk. Ref
advertisements are limited to `1200 * max_refs + 64 KiB`; pack transfer bodies
are limited to `2 * max_pack_bytes + max_stderr_bytes + 64 KiB`. Framed pack
payload separately obeys `max_pack_bytes`. The original pack v2/v3 validator
checks the checksum, type/count/size integers, actual zlib output, self-contained
offset/reference delta bases, copy ranges, declared and actual result sizes,
aggregate payload and delta depth, with cancellation and a deadline.
Installed Git receives a pack only after admission, through
`index-pack --stdin --strict --threads=1 --max-input-size=<cap>`.
One admitted pack and its index occupy an owned private cache. Fetch pack files
stay in staging; native adoption uses the engine's object APIs.

HTTPS fetch rechecks the selected advertised refs after transfer. Push sends a
full admitted pack to actual receive-pack, verifies the complete receipt, then
rereads the target. Receipt parsing requires exactly one unpack status and the
requested ref's status, followed by a flush. It also recognizes the single
additional outer flush observed in GitHub Smart HTTP responses. A third flush,
an extra status, another delimiter, truncation or arbitrary trailing content
remains an uncertain publication result. The regression fixture contains the
exact bounded GitHub response body captured from an authorized disposable
expected-absent branch publication; it contains no credentials. The receiver owns its branch protection, permission and
hook policy. Local publication verifies preexisting duplicate IDs and the
resulting closure; it does not make simultaneous target metadata reconfiguration
atomic with the ref lease.

The HTTP client does not use ambient proxies, credential helpers, cookies or
redirects. It uses Reqwest 0.12.28 and Rustls 0.23.45 with web PKI roots and the
Ring 0.17.14 crypto provider; Ring includes native cryptography. Flate2 1.1.10
uses its Rust backend. The adapter itself forbids unsafe Rust. These standard
library crypto, network and syscall boundaries require dependency/security
review; no Git implementation code or library is linked.

Payload budgets are not a peak-RAM claim: encoded delta buffers, reconstructed
objects, HTTP framing, command input and metadata can coexist. Git's internal
allocation and file-generation behavior remain trusted installed-tool
boundaries. The adapter bounds admitted data, staging population and process
lifetime; it does not provide an aggregate OS filesystem quota, process-memory
sandbox or authority over escaped descendants. DNS completion is a library/OS
boundary; cancellation drops the request and bounds reactor shutdown without
joining an unresolved resolver indefinitely.

## Evidence and production gates

Local Rust tests use disposable Git repositories and bare targets. They exercise
native commit → export → actual Git clone, existing Git → native clone → native
change → leased local push → Git clone, exact original metadata/ID round trips,
signed payload byte/ID preservation, Git-created embedded merge tag byte/ID
preservation, rewritten-commit merge tag removal with warnings, malformed
embedded tag rejection before native publication, attribute-file preservation and ordinary
Git EOL checkout after a native edit, malformed signature headers, separate Git
directories and linked-worktree source round trips, explicit LFS pointer
preservation with unavailable-content warnings, ordered merge
parents, modes and symlinks, native and remote stale leases,
divergence refusal, source capability rejection, corrupt objects, unsafe paths,
and bounded ingestion failures. Owned Rustls loopback fixtures run installed
Git's actual HTTP CGI service. They prove scoped Basic authentication, native
change and source/history round trips through HTTPS, server hook refusal,
server ref races, selected-scope omissions, malformed-receipt uncertainty,
redirect refusal, admission limits and in-flight cancellation. The fixture
worker executable is a test artifact; the product artifact is the shared
facade executable. A selected SSH fixture executable forwards the exact remote
service command to installed Git upload/receive-pack. It proves clone, native
edit, leased push, stale-lease refusal and source scoping over that process and
protocol boundary. It does not prove OpenSSH encryption, a real daemon's
host-key authentication, private-key authentication or a GitHub SSH endpoint.
SSH authentication/cancellation and hosted-server acceptance remain required.

GitHub production acceptance still requires an explicitly authorized disposable
repository and authentic identity/authentication, GitHub-specific lease races,
branch protections and permission failures, authenticated source/history
readbacks, resource containment for a hostile installed tool, repeated
transport cancellation,
Git SHA-1 collision policy verification, supported-platform runs, recovery and
disk-full evidence, and a measured large-repository baseline. A passing local
fixture is not a GitHub deployment or push result.

The original protocol implementation follows the official
[Smart HTTP protocol](https://git-scm.com/docs/http-protocol),
[pack format](https://git-scm.com/docs/gitformat-pack),
[protocol capabilities](https://git-scm.com/docs/gitprotocol-capabilities)
and [index-pack admission options](https://git-scm.com/docs/git-index-pack).
Crypto/provider choices follow the
[Rustls provider interface](https://docs.rs/rustls/0.23.45/rustls/)
and [Reqwest feature definitions](https://docs.rs/reqwest/0.12.28/reqwest/).

## Native tags and Git round trips

Whole-repository import and export include all branches and tags. Native v2
metadata stores tags independently from branch refs, with typed revision, tree
or blob targets. Import publishes branches and tags in one engine transaction;
`TagImport.expected` leases the complete existing native tag value. Existing v1
repositories require explicit migration before tag adoption.

`ImportOptions.tags` selects `TagImport { git_ref, native_tag, expected }` and
`ExportOptions.tags` selects `TagExport { native_tag, git_ref }`. Both selection
lists empty means the whole repository. Explicit branch selections omit tags;
source omissions remain in `GitInventory.omitted_refs`, and export reports an
omission warning when native tags exist.

Git origin retains the ultimate target SHA1 and every annotated tag object,
ordered outermost first, as opaque native blobs. Export validates each original
SHA1, parsed target SHA1 and target kind before writing the exact bytes. Nested
annotations, binary messages and embedded signatures survive unchanged.
Preservation does not verify signatures or authenticate tagger identities.
A changed target cannot reuse the original annotation graph; export rejects a
mismatch. Branch edits leave existing tag targets unchanged.

`PushRequest.git_ref` selects the branch or tag namespace. Tag pushes use the
same explicit remote SHA1 lease and publication readback as branches. Replacing
an existing tag with a different object requires `ExplicitAllow`; tags have no
branch fast-forward relationship. Fetch supports explicit tag selection through
the same native import transaction. Git tree/blob tag closures are imported as
real trees/blobs, never synthetic revisions.

Inventory bounds tag depth and object count/bytes. Declared target kind must
match the addressed Git object. Malformed or unsupported headers fail before
native history publication. Ordinary Git `fsck --strict` checks exported and
pushed closure in interoperability fixtures.

Human and agent interfaces use the same engine tag records. CLI whole `git clone`,
`git import` and `git export` include tags by default. `--tags-json PATH` supplies
a bounded array of `{ "git_ref", "native_tag", "expected" }` mappings to clone,
import or fetch; `expected` is either null for an absent tag or its exact listed
native tag record. JSON/MCP requests use the optional `tags` array. Explicit
branch-only mappings remain partial adoption and inventory their omissions.
Push chooses the native tag namespace when `--git-ref` is under `refs/tags/`;
it publishes only that selected closure and requires the explicit old Git ID or
absent lease. The corresponding native name is supplied through `--native-ref`.
