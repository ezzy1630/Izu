# Executable evidence

`izu-lab` invokes the exact executable supplied with `--executable`; it does not call engine internals or substitute a benchmark implementation. A JSON report records the executable SHA256, source manifest SHA256 (including dirty and untracked source, excluding build outputs), Git HEAD when present, dirty status, Rust version, OS, architecture, scratch filesystem, fixture seed and limits, argument vectors, working directory, stdout, stderr, exit status, timeout and allocation before/after each call. Unborn checkouts are supported by the manifest digest. This is local synthetic-fixture evidence, not deployment or real-remote evidence.

Build and check the harness from the repository root:

```sh
cargo test --locked -p izu-lab
cargo build --locked --release -p izu-cli -p izu-lab
```

Run the command-line acceptance scope against that exact executable. Choose a
new report filename for each run; reports are never overwritten.

```sh
mkdir -p .artifacts/validation .artifacts/tmp
executable="$PWD/target/release/izu"
digest="$(shasum -a 256 "$executable" | cut -d ' ' -f 1)"
TMPDIR="$PWD/.artifacts/tmp" target/release/izu-lab \
 --executable "$executable" --expected-sha256 "$digest" \
 --source-root "$PWD" --output "$PWD/.artifacts/validation/cli-workflows.json" \
 --scratch-dir "$PWD/.artifacts/tmp" acceptance --scope cli-workflows
```

The harness creates a uniquely named owned temporary directory beneath the scratch parent and removes only that directory. It neither fills a disk nor mounts an image. After readbacks, cleanup restores owner traversal/write permission only on lab-owned fixture directories so immutable cache directories can be removed. It uses `symlink_metadata` and never traverses links or changes file permissions. Every benchmark fixture records its cleanup outcome and locator; the outer report records the same observation. A cleanup failure fails the requested scope and preserves the completed JSON evidence plus the owned locator for inspection. The report output uses `create_new` and never overwrites existing evidence. Process execution has a default 30-second deadline, a 2 MiB retained limit per output stream, and a 64 KiB stdin limit. Large output is drained rather than deadlocking the child. The shared `izu-process` boundary launches a retained worker hosting an owned process group and nonblocking bounded pipes. It requests shutdown while the worker still owns the group, then requires observed group quiescence; uncertain cleanup fails the invocation. There are no real post-reap signals or blocking pipe-reader joins. Every owned worker is joined before response validation. The worker executable and SHA256 are recorded. MCP stdin remains open until all expected response IDs arrive, then closes for orderly EOF. Children receive a cleared environment with explicit TMPDIR, a controlled system PATH, optional isolated Cargo/Rustup homes, and disabled global/system Git configuration. No unrelated credentials are forwarded.

The expected product SHA256 is mandatory and verified before any fixture or product command, then checked again after execution. Source manifest hashes before and after execution must agree. The declared exclusions include `.artifacts`, `target`, `target-*`, `lab-results`, and VCS/build metadata at any depth. The source digest describes the supplied tree; it does not itself prove that the tree compiled the artifact. Keep the build command and frozen build source alongside the report. Set TMPDIR, scratch and evidence explicitly when filesystem placement matters. If `CARGO_TARGET_DIR` is set, adjust the executable paths to match it.

Exit 0 means every case required by the explicitly selected scope passed. Exit 2 means at least one case failed or remains blocked. Exit 1 means the harness could not produce its report. Blocked cases are neither successful skips nor proof of unsupported behavior.

The default suite runs init, status, checkpoint, selective commit, history and verify plus cold native bundle restore, corruption/unknown-format rejection, original Git→izu edit→local bare→ordinary clone, interactive MCP, two and thirty independent managed `start` controllers, and the embedded restore/undo, private workspace, guarded ref race, 30-writer integration, conflict/resolution, exact-candidate check, ignore/symlink and JSON scenarios. It independently compares actual file hashes and byte lengths with expectations. Authorship is explicitly synthetic within owned test repositories. The required coverage ledger also names undo/restore, private workspaces, 30 writers integrating into one target, a guarded ref race, conflicts/resolution, exact-candidate check freshness, crash/restart, disk-full/retry, corruption/format rejection, tracked ignore/symlink safety, bundle restore, Git interoperability and JSON/MCP. Cases without an implemented real path remain blocked until exercised through concrete recipes or equivalent independently recorded external evidence; existing recipes become passed or failed from observed executions. A default basic result does not establish those stronger properties.

## Unresolved observations

The local validation campaign repeatedly observed prepared-environment import
failures in a Linux container using a macOS filesystem bind mount. The original
creator metadata agreed with the published READY marker, while the consumer's
opened manifest reported a different native identity despite matching contents.
The guard refused import acknowledgement with `PublicationIdentityUncertain`
and retained the visible artifact.

A diagnostic retained the exact opened `File` after that failure. It observed
unpredictable bytes written through a host descriptor pinned to the staged
manifest before sealing, then the original bytes after restoration. Same-file
positive and distinct-file, identical-content negative controls passed. Under
ordinary descriptor semantics, this establishes live data correlation between
the host pin and the failed guest `File`. It does not prove uninterrupted
creator-to-reopen object identity: the original creating guest `File` closed
normally and was not nonce-correlated. A specific kernel, provider or application
cause remains unproven.

The observed bind mount remains unqualified for this version's persisted
native-identity protocol. The guard remains enforced; matching contents alone
cannot justify acceptance. Separate macOS and guest-local ext4 checks passed
their recorded scopes. They do not certify prepared-cache availability on the
failing mount.

Earlier first-discovery MCP timeouts and two Linux storage acquisitions with
30 ms fixture deadlines also remain unattributed. Later passing checks and the
separately reproduced MCP output-shutdown correction do not explain those
events. Retain these limits with build evidence; a passing later run does not
mean every observed reliability failure has been resolved.

## Scenario format

Each `--scenario` file is a schema-version-1 JSON object with a name and up to 1000 sequential steps. The commands always use the exact supplied izu executable. Relative working directories and file paths are contained within the owned scenario directory. Existing or dangling links escaping that directory are rejected before harness writes.

Supported steps are:

- `write`: write text to an owned relative path.
- `assert_file`: independently compare actual bytes with expected text.
- `symlink`: create a Unix link between owned fixture paths, for testing product path safety with an owned external sentinel.
- `run`: invoke argument vector, optional bounded stdin, expected exit (default 0), exact JSON-pointer assertions, finite alternative assertions, and string captures by JSON pointer. An assertion pointer containing `/*/` projects that field from each array element in order, for exact status-path membership checks.
- `parallel`: run 1..30 processes with shared per-batch launch barriers and optional `max_parallel` (default 30); require an exact success count and an explicitly named typed error code for every rejected process. Timeouts, malformed JSON and truncated output never count as expected race failures. All owned workers are joined before cleanup.

`${root}` is the owned scenario directory. `${name}` substitutes a captured string. Unresolved substitutions fail. A mutation example follows; its captured JSON pointers must come from the actual supported API contract:

```json
{
  "schema_version": 1,
  "name": "example_flow",
  "steps": [
    {"kind":"run","args":["--json","init","."],
     "assertions":{"/outcome/kind":"ok"}},
    {"kind":"write","path":"source.txt","text":"retained bytes\n"},
    {"kind":"run","args":["--json","checkpoint"],
     "assertions":{"/outcome/kind":"ok"}},
    {"kind":"assert_file","path":"source.txt","text":"retained bytes\n"}
  ]
}
```

Disk-full evidence must use an integrator-owned disposable filesystem image passed as `--scratch-dir`, with enough free capacity to preserve the report outside that image. Crash scenarios need an explicitly supplied test-only fault binary and exact last-acknowledged state assertions on restart. Normal CLI builds must not enable test faultpoints. Neither scenario is replaced with a mocked failure or a shared-disk fill.

The harness's own unit tests protect fixture budgets/determinism, bounded output, child reaping, percentile arithmetic and path containment. They do not establish that izu product workflows work.

Embedded candidate checks invoke the POSIX shell's `test` builtin with the original operands passed as separate arguments. This preserves exact candidate/check assertions on macOS and Linux without assuming `/usr/bin/test` exists. The owned native-state case separately requires unknown `FORMAT` versions to return `unsupported_feature` (exit 3), a damaged HEAD checksum to return `corrupt_data` (exit 7), and damaged referenced object-frame bytes to return `corrupt_data` (exit 7). It restores each original byte sequence, verifies the repository again, and independently confirms the source sentinel was preserved.

Managed launch cases use the exact freshly built lab executable as a fixture child, invoked by the supplied izu binary. Every child writes one unique private file and records its actual cwd plus wall-clock start/end timestamps. The report preserves these intervals and observed peak overlap. The suite requires real overlap of at least two jobs, at most four active jobs, thirty distinct private roots, successful completed job receipts, and unchanged primary source. Global-clock reversal or missing intervals fail instead of fabricating overlap. Receipt and child output evidence is retained before fixture cleanup.

Git HEAD/status metadata is recorded only when `git rev-parse --show-toplevel` exactly matches the supplied source root. A frozen source snapshot nested inside a checkout records `ancestor_ignored` instead of attributing its parent checkout status to the snapshot. The source manifest remains the checkout-independent identity.

Default `acceptance --scope full` retains all required coverage and reports incomplete while storage crash/restart or disk-full cases lack concrete evidence. CI may explicitly select `acceptance --scope cli-workflows`: it still executes and requires every actual CLI case, including native archive, corruption, Git exchange, JSON/MCP and independent managed launches. Only the two named storage-fault cases are excluded from that scope; their blocked records remain in the report. `required_cases`, `excluded_scope_cases`, `scope_complete` and `full_coverage_complete` prevent scope success from being confused with complete coverage. The root aggregate must attach its separate fault suite and kernel ENOSPC evidence with exact source/driver/artifact identities. Missing CLI capabilities, malformed output, changed source or product/worker artifact, timeouts and required blocked cases still fail the CLI scope.

Source manifests on macOS and Linux sort the full relative filesystem path by
encoded bytes, so `a.rs` sorts before `a/nested.rs`. For each entry, hash the
8-byte little-endian path byte length, raw relative path bytes, then either
`file\0` followed by the 64 lowercase ASCII hexadecimal SHA256 characters of
the file contents, or `symlink\0` followed by raw link-target bytes. File
length, permissions and timestamps are not added to this existing framing.
The recorder uses the same encoded-byte ordering, including surrogateescaped
non-UTF-8 names on hosts that preserve those bytes. Source collection
exclusions and the 100000-entry limit remain unchanged. A provenance test covers a component-prefix pair, a
Unicode filename and a relative symlink using an independently framed digest.
Directory symlinks and broken symlinks are entries whose targets are hashed
without following them. Ordinary generated directories retain their existing
exclusions; a symlink with such a directory name remains an entry. `.git` is
always excluded, including a directory symlink.
