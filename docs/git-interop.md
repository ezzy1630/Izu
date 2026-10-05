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
- `inventory_with_options(source, options, cancel)` checks an explicit branch
  selection and reports every advertised ref outside that scope.
- `import_into(repository, source, options, cancel)` and `fetch_into` prepare
  immutable native objects, then call the engine's atomic `import_revisions`.
- `clone_into(destination, source, options, cancel)` requires a new or empty
  destination, inventories first, imports through the engine, and materializes
  the advertised default branch through the engine's guarded restore operation.
- `export_to(repository, target, options, cancel)` requires a new or empty local
  output and builds a bare Git repository.
- `push_ref(repository, request, cancel)` publishes one explicit ordinary branch
  into an existing bare local target or an explicit HTTPS target.

Sources are `GitSource::local(path)` or validated `GitSource::https(url)`. SSH,
embedded URL credentials, queries, fragments and normalized dot segments are
unsupported. HTTPS URLs must be canonical. The adapter does not create remote
repositories, modify credentials or permissions, execute
source hooks, or load user credential helpers. Library callers can supply
`authentication: Some(HttpsAuthentication::basic(url, username, password))` for
one exact HTTPS repository URL. The credential exists in memory, has redacted
diagnostics, is not persisted, and redirects are refused. The facade accepts
explicit HTTPS sources and scoped authentication; the CLI reads a token only
from an explicitly named environment variable. No authenticated GitHub result
has been proven. Additional DER trust anchors are explicit configuration,
limited to eight certificates of 64 KiB each; there is no TLS verification bypass.

Each imported native ref has an explicit expected previous native revision.
An empty `ImportOptions.refs` creates identically named new refs from ordinary
source branches; it cannot overwrite an existing native ref. This full scope
rejects tags and unsupported ref namespaces. An explicit `ImportOptions.refs`
checks only the selected branch graphs and returns every other advertised ref
in `GitInventory.omitted_refs`. Unrelated tags and pull refs therefore do not
block an explicit branch selection, and are not claimed to have been adopted.
Default ref naming requires portable native ref names; callers can explicitly
map another supported Git branch name to a portable native name. The clone path
leases only the exact internal initialization ref that it just created. Native
history changes always use the engine, including clone restoration.

`PushRequest.expected_old` is required by the type. `None` explicitly means that
the remote ref must be absent. No tracking ref or remembered lease is inferred.
The default non-fast-forward policy rejects replacing divergent history.
`ExplicitAllow` still requires the exact supplied lease. Existing local and
HTTPS targets are published through Git receive-pack after the ancestry policy
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
bytes and original Git IDs.

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

The source mapping supports complete selected ordinary branch graphs, UTF-8
source paths and textual metadata, SHA-1 Git repositories, regular/executable
files and symlinks.
Within the selected graphs, before native publication, it rejects
signed commits, merge tags, unknown commit headers, non-UTF-8 textual fields,
Git SHA-256 repositories, shallow history, grafts, replace refs, linked
worktrees, object alternates, unknown repository extensions, `.gitattributes`,
filters, Git LFS pointers and submodules. Rejected metadata is reported in the
inventory or a typed error; it is not stripped and reported as a successful
adoption. Tags and other source ref namespaces are rejected in full scope or
explicitly reported as omitted in branch scope. Namespace aliases are checked
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
Protocol v2, SSH and dumb HTTP are explicit missing capabilities.

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
rereads the target. The receiver owns its branch protection, permission and
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
ordered merge parents, modes and symlinks, native and remote stale leases,
divergence refusal, source capability rejection, corrupt objects, unsafe paths,
and bounded ingestion failures. Owned Rustls loopback fixtures run installed
Git's actual HTTP CGI service. They prove scoped Basic authentication, native
change and source/history round trips through HTTPS, server hook refusal,
server ref races, selected-scope omissions, malformed-receipt uncertainty,
redirect refusal, admission limits and in-flight cancellation. The fixture
worker executable is a test artifact; the product artifact is the shared
facade executable.

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
