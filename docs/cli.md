# izu command line

`izu` is native Rust version control. It does not require Git, an account, or a
background daemon for local history and private work. The CLI, typed JSON, and
MCP all use the same Rust operation facade and engine. `izu capabilities --json`
reports the scope of the executable you are actually running.
This pre-release uses IZU native framing and metadata. It does not migrate older
EZY framing; preserve historical data and its matching reader. Initialization
refuses legacy metadata instead of creating a second repository over it.

Create an empty project, select your author once, and make an intentional commit:

```sh
izu init my-project
cd my-project
export IZU_AUTHOR_NAME="Your name"
export IZU_AUTHOR_EMAIL="you@example.com"
printf 'first version\n' > hello.txt
izu commit -m "Start project"
izu status
izu log
```

The exported identity is an explicit choice in your shell, not a global izu or Git
configuration change. `--author-name` and `--author-email` override it. izu never
infers a human identity. Initialization records an identified engine service actor.
Normal primary commits advance the workspace and `main` atomically, with guards
against a changed workspace or reference. Private commits stay in their change.

Start a private change and run the program you explicitly choose there:

```sh
izu start feature -- python3 -c 'from pathlib import Path; Path("hello.txt").write_text("feature version\n")'
izu --change feature status
izu --change feature diff
izu --change feature commit -m "Implement feature"
izu --change feature land --current --check tests -- python3 -c 'from pathlib import Path; assert Path("hello.txt").read_text() == "feature version\n"'
izu status
izu log main
izu update
```

`start NAME` allocates a private workspace inside native metadata from the exact
current `main` revision. `--from REFERENCE_OR_REVISION` chooses another baseline.
The same name resumes its existing workspace; an incompatible explicit baseline
is rejected. `izu --change NAME run -- PROGRAM ARGS` launches again, and `run`
inside that private directory selects it automatically. Human launches reject the
primary workspace. Primary files remain stable while the chosen program works.
There is no shell interpretation unless you explicitly choose a shell executable.

Managed launches checkpoint admitted source before running and after observed
process shutdown. The response identifies the private directory, actual job state,
bounded output, and checkpoint receipts. Program failure and cancellation retain
those receipts and return an error. `--timeout-seconds` sets a deadline;
`--env NAME=VALUE` supplies an ephemeral environment overlay. Overlay values are
not automatically copied into durable job summaries; a chosen program can
print values into its captured output. SIGINT/SIGTERM cancel the CLI's owned job on Unix;
cancellation does not roll back edits or a publication that already happened.

`land --current` requires a committed current change and an explicitly supplied
check program. `--source REFERENCE_OR_REVISION` chooses a different exact source;
`--target NAME` selects a reference other than the default `main`. The facade
resolves these once, prepares a native integration candidate, checks its exact
result in a private workspace, and publishes only if the declared checks pass and
the target still matches. Missing, failed, cancelled, stale, or source-changing
checks cannot authorize landing. Starting a new check durably invalidates an older
pass; late evidence cannot revive it.

Landing moves the target reference and retains primary files and all change
sources. `status` shows the actual checkout revision and `main` separately.
`log` follows the selected checkout; `log main` follows the published target.
`update [REFERENCE_OR_REVISION]` deliberately moves a clean selected workspace,
with recovery preservation and an exact expectation. Dirty source is rejected.
A primary checkout behind `main` cannot silently commit over the newer target.
Preserve its edits in a checkpoint or private change, reconcile them, then update.

Use `change list`, `change open NAME`, and `change close NAME` to inspect or close
named changes. Close stops owned writers, checkpoints and verifies source, and
removes registration while retaining the source directory and native history. It
refuses unknown writer ownership. `--repo PATH` chooses a project or registered
workspace from another directory. `--workspace ID` and `workspace open/close ID`
are advanced exact bindings; ordinary named work does not require copying IDs.

`checkpoint` creates a durable local working snapshot without an intentional
commit. It is not a remote backup. `commit -m MESSAGE --path RELATIVE_PATH` selects
one admitted file or directory; repeat `--path` for more. Unselected edits remain
on disk and visible in status. `change revise -m MESSAGE` records a new immutable
revision of the selected logical change and retains the old revision in history.
Concurrent mutation reports a stale expectation instead of overwriting another
writer. Native verification and recovery do not run project tests.
Captures observe each file during traversal; they do not claim an atomic snapshot
of a filesystem being changed by another writer. Immutable checkpoints remain
available under unknown writer intent; source materialization and new launches
require explicit stopped-token recovery first.

Advanced integration can be reviewed and executed in separate steps:

```sh
izu candidate SOURCE_REVISION --target main --expect MAIN_REVISION --check tests -- cargo test
izu candidate-show CANDIDATE_ID
izu check CANDIDATE_ID --check tests
izu land CANDIDATE_ID
```

Candidates and unresolved conflicts are durable native records. A conflict error
includes its exact candidate and paths. `resolve CANDIDATE_ID --path PATH --take
base|ours|theirs|delete` selects a side. `--content-file FILE [--executable]` supplies
bounded UTF-8 content. Each resolution produces a new candidate requiring its own
checks. `check ... --start-only` publishes a pending attempt without execution.

`restore REVISION --path PATH` and `undo OPERATION --path PATH` preserve recovery
source and affect only the chosen workspace/path scope. Whole-workspace restore
also adopts that revision; selected-path restore retains the current head. Undo
never rewinds unrelated workspaces or references. `operations --limit N` reads
operation lineage. `verify` checks referenced native object integrity; `recover`
inspects and cleans owned interrupted store state. On partial restoration errors,
preserve source and retained originals and inspect the reported recovery operation
before retrying. References can be explicitly guarded with `refs set/delete` and
`--expect REVISION` or `--expect-absent`.

Managed controller recovery is explicit. `jobs list` returns bounded summaries;
`jobs status JOB_ID` reads the actual recorded state. A saved PID never grants
ownership. `jobs cancel JOB_ID` refuses an unknown or unowned process instead of
signalling an arbitrary PID. After a crashed controller, first verify that its
process group and any external writers stopped. `--change NAME writer status`
shows the exact native intent token. Record the assertion with `writer
acknowledge-stopped TOKEN --note ASSERTION`, then reconcile the runtime job with
`jobs acknowledge-stopped JOB_ID --note ASSERTION`. Notes are durable. A different
token or a live native writer lease cannot clear intent. If a crash occurred before
a token was saved in the runtime job, reconcile the native token explicitly first;
izu never adopts a different workspace's token or infers process death from age.

`revert REVISION -m MESSAGE --path PATH` creates an intentional inverse change for
a single-parent revision. Selected dirty source is refused; unselected edits stay
on disk. A primary revert publishes its new head and `main` atomically. Private
reverts remain private. A conflicting inverse records immutable conflict data
while retaining files/head/reference. `resolve-revision REVISION --path PATH
--take SIDE` (or `--content-file`) resolves that native record; source changes only
when explicitly restored or integrated afterward.

Optional prepared environments require an explicit trusted recipe, source and
lockfile identities, and caller-prepared dependency/output directories. Discovery
executes nothing. `environment source-identity` on a private change supplies its
actual captured source identity. See [environments.md](environments.md) for recipe
fields and cache invariants.

```sh
izu --change feature environment source-identity
izu environment import ../prepared --recipe recipe.json --trust-recipe --quiescent
izu environment status --recipe recipe.json --trust-recipe
izu --change feature environment materialize --recipe recipe.json --trust-recipe
izu environment bind recipe.json --trust-recipe
```

The default cache is local native metadata `environments/v1`; `--cache PATH`
chooses another explicit cache and `--copy` disables clone preference. Quiescence
is the caller's explicit assertion, not inferred writer ownership. Materialization
holds a checked private-workspace guard and rejects mismatched source/lockfiles.
Receipts report measured copy/clone behavior. `bind` fully verifies the default
repository cache and stores a native binding blob. Its `object_id` is distinct
from the cache `key`. The blob is durable locally but remains unreferenced until a
candidate's check definition includes it; only then is it part of rooted bundle
closure. Warm cache payloads themselves remain outside bundles.

Use that native object ID with `candidate ... --environment OBJECT_ID`, or
`land --current --check NAME --environment OBJECT_ID -- PROGRAM ARGS`. The runtime
materializes the exact bound artifact in the private check workspace, verifies
the actual starting file contents under its pinned writer lease, and retains its
cache lease until process cleanup. The job receipt includes the measured starting
environment proof; `capabilities` states its verification scope. Missing, changed,
or mismatched artifacts cannot reuse an old
passing check. This scope covers starting file contents: warm outputs can change
during the chosen program. OS and architecture are observed; toolchain identity
and ABI remain explicit caller declarations. This is not a security boundary or
proof of an immutable execution environment. Preparation argv is never run
implicitly by binding, discovery, or check launch.

A bundle is a complete local archive of already captured native object closure:

```sh
izu checkpoint
izu bundle create ../project.izu-bundle
izu bundle verify ../project.izu-bundle
izu bundle restore ../project.izu-bundle ../restored-project
```

Uncaptured edits and runtime/environment caches are omitted. Historical captures
and recovery sources may contain previously captured ignored files or secrets;
bundles are private complete archives, not redacted exchanges. Nothing uploads
automatically. Keep another independently retained copy for device-loss recovery.
Bundle parents must be real directory paths, without symlink aliases; on macOS
use explicit canonical parents rather than `/var` or `/tmp`. Restore requires a
new nonexistent destination; `--selected-workspace ID` disambiguates a multi-
workspace archive. Receipts distinguish durable from visible-but-uncertain
publication and report retained recovery paths on failure.

Git interoperability is optional and invokes an explicitly installed Git tool.
This CLI accepts explicit local repository paths and credential-free HTTPS URLs:

```sh
izu git import ../source-git-repository
izu git fetch ../source-git-repository --native-ref imported-main --git-ref refs/heads/main --expect NATIVE_REVISION
izu git export ../new-bare-git-repository --committer-name "Your name" --committer-email "you@example.com"
izu git push ../bare-target --native-ref main --git-ref refs/heads/main --expect GIT_OBJECT_ID
```

Import/fetch do not implicitly checkout or publish source. Push requires an exact
Git reference expectation or `--expect-absent`; divergent publication additionally
requires `--allow-non-fast-forward`. Target receive policy remains authoritative.
New exported native commits require an explicit authentic committer. The default
timestamp is observed current time at UTC; explicit timestamp/offset flags are
available. No command alters your global Git configuration or credentials. HTTPS rejects
embedded URL credentials and unsupported transports. `--https-user USER
--https-token-env VARIABLE` reads one explicitly selected secret from your process
environment for that exact URL; it is not copied into configuration or receipts.
For a supplied private trust anchor, `--tls-root-cert FILE` reads a regular DER
file of at most 64 KiB, with no-follow/nonblocking file opening. At most eight
anchors are accepted; certificate validation is never disabled. The same HTTPS
operations in typed JSON accept an explicit URL object and ephemeral basic
credentials. Public-host/account access is distinct from owned TLS fixture proof;
this executable does not claim your account or provider policy is already verified.

`--json` emits one schema-versioned response. `json` reads one typed request of at
most 1 MiB; `schema --json` derives its schemas from Rust types. Results and text
diffs are bounded; oversized/binary content is identified explicitly. History
limits are 1..=1000 records. The build identity reports package/native-format
versions and an optional supplied build ID; an absent ID is unknown.

Exit codes are 0 confirmed success, 1 failure/not found, 2 invalid request/schema,
3 unsupported feature, 4 stale expectation/conflict, 5 missing or failed check,
6 uncertain publication, 7 corrupt data, and 130 cancellation. An uncertain
publication can already be visible. Inspect its exact operation, target, recovery
operation and retained paths before retrying. Process groups and cooperative
admission are not a security sandbox or hard CPU/memory/disk containment. MCP
discovery does not automatically bind another host's threads; see
[agent-api.md](agent-api.md) for supported explicit wrappers and protocols.
