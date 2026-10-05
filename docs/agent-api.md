# Typed JSON and MCP

Every interface dispatches the same `izu_api::Operation` through `izu_api::Api`.
The Rust engine owns source/history mutations. No command or protocol adapter has
an independent revision-selection or history implementation.

`izu json` accepts one UTF-8 JSON request on stdin, bounded to 1 MiB. The request
envelope contains `schema_version: 1` and a tagged operation. Unknown fields,
unknown operations, invalid IDs, and unsupported schema versions are errors.
Schemas come from the same Rust request enum through pinned Schemars 1.2.2;
`izu schema --json` returns the request and response schema.

```json
{"schema_version":1,"operation":{"kind":"status","context":{"repository":"/absolute/project","workspace":null}}}
```

Read context requires a repository locator. Omitted/null workspace chooses only
the workspace containing that path. An explicit workspace is its full validated
128-bit lowercase hexadecimal ID. Revisions, trees, operations, and candidates use
their full validated native IDs; an abbreviated/divergent selector is not guessed.

Mutations of workspace source require an exact workspace ID and both revision and
working-tree expectations obtained from an earlier `inspect` response.

```json
{"schema_version":1,"operation":{"kind":"commit","context":{"repository":"/absolute/project","workspace":"WORKSPACE_ID","expected":{"head":"REVISION_ID","working_tree":"TREE_ID"}},"selection":{"kind":"paths","paths":["src/main.rs"]},"message":"Implement change","author":{"name":"Explicit author","email":"author@example.com"},"target":null}}
```

The example ID placeholders must be replaced with real native IDs. A stale
expectation fails without selecting another revision. Named-reference updates and
candidate preparation require a caller-selected exact old value; null expressly
means the reference is absent. Intentional commits have a required explicit Author. `commit.target` optionally
contains `{name, expected}` for atomic workspace/reference publication; null
commits only the selected workspace. Human primary commits select guarded `main`.

Responses always carry schema version and executable identity. An ordinary success
is `outcome.kind: "ok"` with `result.kind` and `result.data`. Native payloads use the
engine's serialized records. The generated outer response schema intentionally
does not claim a stricter shape for `data` than those native records.

Failures have `outcome.kind: "error"` and a typed `error.code`, message, next action,
and visible operation ID when known. Restoration errors also expose the separate
`recovery_operation_id`; retained staging/original paths are structured in
`retained_paths`. A rejected operation can also contain its actual
`result`: a durably recorded conflict candidate or a stopped failed/cancelled job
is still reviewable. Known visible-but-uncertain artifact publication instead has
`outcome.kind: "uncertain"`, the exact receipt, and `publication_uncertain` error.
There is no boolean that converts publication uncertainty into confirmed success.

Error codes distinguish `invalid_request`, `unsupported_schema`,
`unsupported_feature`, `not_found`, `missing_check`, `stale_expectation`,
`publication_uncertain`, `cancelled`, `corrupt_data`, `conflict`, and `failure`.
Native operations remain independently inspectable after an interrupted response.
Do not retry an uncertain mutation without reading the exact operation and target.

Run `izu agent serve` to expose newline-delimited JSON-RPC over stdin/stdout.
Stdout contains only MCP messages. Each frame is bounded before allocation;
oversized frames are drained through their newline and return an error. Invalid
JSON and invalid request structures do not stop valid subsequent requests. Batch
messages are unsupported. There are at most eight queued requests plus one active
operation. Responses are bounded to 16 MiB; native result encoding is bounded
before constructing nested MCP text content.

On macOS and Linux, this command takes an exclusive byte-stream lease over its
inherited stdin/stdout and piped stderr. Supply pipes/FIFOs or regular files;
packet-mode Linux output writers and other descriptor types are unsupported. On
Linux, observable `O_DIRECT` on stdout/stderr pipe writers is rejected. Ordinary
byte-stream stdin is a **host precondition**: a Linux packet pipe's reader flags
do not reveal the writer's packet mode, so the server cannot attest or reliably
reject such input by descriptor inspection. The host must not supply packet-mode
input. This follows from the [Linux pipe creation code](https://github.com/torvalds/linux/blob/v6.12/fs/pipe.c#L874-L886).
Descriptor inspection establishes their type, **not** exclusive ownership: the host must
provide dedicated endpoints without other readers, writers or status-flag owners.
The server deliberately sets temporary `O_NONBLOCK` on pipe/FIFO endpoints. This
changes every alias of the same open file description, including aliases retained
by the host; duplicating a descriptor does not isolate the change. Normal/error
return restores the original NONBLOCK bit while preserving unrelated current
flags. SIGKILL or abnormal termination cannot restore flags on surviving aliases.
Use dedicated endpoints rather than sharing their aliases with other work.

Regular-file redirection remains available. Bounded ordinary-file reads/writes,
descriptor setup and engine/filesystem cleanup can enter uninterruptible kernel
IO; the transport does not promise a hard process-exit deadline for those calls.

The implementation supports two exact published protocols:

- [MCP 2026-07-28 stdio](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio),
  [versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning),
  and [discovery](https://modelcontextprotocol.io/specification/2026-07-28/server/discover):
  every request supplies protocol version and client capabilities in `params._meta`.
  `server/discover` exposes the supported versions and tool capability. Modern
  results contain `resultType: "complete"` and server identity in result metadata.
- [MCP 2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
  and [tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools):
  `initialize` negotiates the supported legacy revision, followed by
  `notifications/initialized`, then `tools/list` and `tools/call`.

```json
{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"example-client","version":"1"}}}}
```

`tools/list` returns deterministically ordered named operations such as
`izu_status`, `izu_commit`, and `izu_land`. Each tool's input schema is derived from
the same request enum and restricts its operation tag. `tools/call.arguments` is a
complete typed request envelope. The tool name and request operation must match.
Unknown tools or malformed RPC structure return protocol errors. Engine, boundary,
and business-rule failures return a tool result with `isError: true`, typed
`structuredContent`, and a serialized JSON text block. Successful tools have
`isError: false`. Unsupported modern protocol versions return code `-32022` and
the accepted modern version. Missing required modern metadata returns `-32602`.

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"izu_status","arguments":{"schema_version":1,"operation":{"kind":"status","context":{"repository":"/absolute/project","workspace":null}}}}}
```

An MCP client keeps stdin open while waiting for responses. `notifications/cancelled`
signals the referenced cancellation token and suppresses a response that has not
started delivery. A partly delivered JSON frame is completed, or the whole
transport ends; cancellation never drops its remainder and appends another frame.
Request IDs remain pending until complete delivery or discard. Cancellation does
not erase durable native state.

EOF or terminal input failure cancels active/queued operations and joins the
worker after cleanup. With a healthy draining peer, already returned durable or
uncertain receipts are delivered. If output remains pending after EOF, a
two-second absolute drain budget starts when that output becomes pending. Partial
progress does not extend it; an empty output state ends that episode. Time spent
settling a cancelled engine operation with no pending output is excluded. Drain
expiry discards undelivered frames and ends the session with a nonzero exit code.
Closing the stdout consumer is observed even while stdin remains open. Parse,
admission and queue-full errors use bounded queues; exhausting them terminates
the session instead of blocking input processing. The host should drain stdout
throughout the session and close stdin for ordinary shutdown.

Terminal transport errors retain bounded request IDs and exact native operation,
recovery and revision IDs from returned receipts for a best-effort stderr
diagnostic. They do **not** prove rollback or safe retry: active work can have
completed, and a receipt can be undelivered. Inspect native status, operation
history and exact publication targets before retrying. Diagnostic output is
bounded and nonblocking on piped stderr; a full pipe can lose it. Unsupported
blocking device stderr is skipped, never used as a blocking fallback. The exit
code still reports failure.

The Rust `mcp::serve<R: BufRead, W: Write>` compatibility entrypoint shares protocol
handling but retains its caller's blocking IO behavior. Arbitrary implementations
cannot guarantee cancellable reads/writes. The executable uses the leased stdio
coordinator described above; it does not detach a blocked reader or terminate
from a worker thread.

MCP discovery is advisory. It cannot automatically bind an arbitrary host's threads
or replace another process's working directory. `agent_run`, `managed_start`, and `managed_run` are deliberately absent from MCP
tools. The ordinary human wrapper resolves a named private change and binds the
chosen argv before its first write:

```sh
izu --repo /absolute/project start feature -- program argument
izu --repo /absolute/project --change feature run -- program argument
```

The advanced wrapper accepts caller-supplied exact bindings:

```sh
izu --repo /absolute/project --workspace WORKSPACE_ID agent run --cwd /absolute/private-workspace --expected-head REVISION_ID --expected-tree TREE_ID -- program argument
```

The same `agent_run` typed JSON operation is available to a caller that explicitly
selects the executable/argv, cwd, workspace, expected head/tree, environment overlay,
and timeout. The facade checks canonical cwd against the registered root and the
runtime rechecks the revision/source binding at launch. No implicit shell interpretation, recipe execution, remote mutation, or
generic error-swallowing tool is provided. Explicit Git tools can affect external
repositories; check tools execute declared code and can access external systems.
Their conservative `openWorldHint` is true. Tool hints are advisory metadata,
not authorization or isolation. Environment overlay values are not automatically copied into durable machine
job summaries; a chosen program can print them into captured output.

Process groups, admission accounting, and cancellation control the runtime's owned
cooperating children. They do not prove containment of arbitrary external writers,
hard resource quotas, or a security boundary. A local checkpoint is distinct from
a remote backup. Native verification validates native object integrity; candidate
checks and external publication have their own exact-input/readback gates.


Shared convenience operations retain exact mutation boundaries. `managed_start`
contains either a `new` target with full immutable `from` revision, or a `resume`
target with an exact MutationContext. `managed_run` also requires that context.
Both carry explicit argv/environment/timeout; no operation chooses a divergent
logical change automatically. Their receipts contain `workspace`, `before`,
`after`, and `job`. Failure/cancellation can retain the same actual receipt.
If a new start creates its workspace but fails before the first checkpoint,
the partial receipt instead contains `workspace` and the exact `fork_operation`,
with a retained source path. Its error identifies a newer engine-reported
uncertain operation when present, otherwise the fork. After checkpoint, failures
identify that checkpoint or a newer engine-reported operation. Neither a failed
start nor cancellation rolls back a visible operation. Managed source preparation
and close use the runtime's cancellable source admission before history mutation;
waiting for capacity consumes neither a command slot nor its execution timeout.
Resume refuses a known active or unreconciled writer before its preparation
checkpoint. Runtime admission checks ownership again before the command starts.
`land_current` accepts an exact source, target-reference expectation, explicit
check definitions, author and timeout. It runs the same candidate/check/land
operations; conflicts or failed checks retain the reviewable candidate and jobs.

`check_start` durably publishes a `pending` attempt token before execution, with
null finish time. Terminal evidence is accepted only for the latest attempt and
its exact candidate/revision/tree/environment/argv/start time. Starting a new
attempt invalidates any older pass immediately. The managed `check` operation
owns this lifecycle; a pending or failed latest attempt cannot land.

Environment import/materialize/status are opt-in typed operations with bounded
recipe JSON and explicit recipe trust. Import additionally requires an explicit
quiescence acknowledgement. Source identity comes from a locked captured private
workspace; supplied environment keys are never accepted as check attestations.
`environment_bind` fully verifies the default repository cache, writes a durable
native binding blob, and returns its `object_id` separately from `key`, manifest
digest, and source identity. The receipt explicitly reports unreferenced storage:
there is no synthetic history operation or archive-root claim. A later candidate
check's optional `environment` field references that native object and roots it
for bundle closure. Binding never runs the recipe's preparation argv.

For a declared environment, the runtime materializes the exact verified artifact
into its private check workspace and verifies actual starting files under the
pinned native writer lease before release. The job's `starting_environment`
receipt reports `StartingFileContents` scope and observed OS/architecture;
toolchain identity and ABI are caller declarations. The cache proof's shared
lease stays alive through owned process cleanup. Writable outputs can change
during execution, and the proof reports `security_boundary: false`. A new failed
or pending attempt blocks landing even if an earlier attempt passed. The cache
payload is not implicitly archived by storing its binding descriptor.
The capability response's `environment_checks` reports the same starting-file
scope, default repository cache, and caller-declared toolchain and ABI.
Native complete bundles can retain historical ignored or recovery secrets. They
are local complete archives, with no implicit upload or redaction guarantee.


`jobs_list` returns bounded newest-first summaries, filtered by an explicit
workspace if supplied; it omits argv, output, and overlay values. `job_status`
returns one exact job receipt. `job_cancel` acts only through recorded runtime
ownership and has conservative external-world metadata. After controller loss,
`writer_status` exposes an exact intent token. `writer_acknowledge_stopped` requires
that token and an explicit bounded operator note, persisted in native history;
live or mismatched leases refuse it. `job_acknowledge_stopped` records the separate
runtime assertion. Starting jobs whose token was never saved require native token
reconciliation first. No command guesses process identity from a saved PID.

Git sources preserve legacy local-path strings. HTTPS is explicitly represented
as `{ "url": "https://host/owned.git" }`; URL credentials and unsupported schemes
are rejected. Optional transport authentication is ephemeral `{ "kind": "basic",
"username": "selected-user", "password": "caller-supplied-secret" }`. MCP requests
cannot name arbitrary host secret environment variables. DER trust anchors are
explicit byte arrays, limited to eight certificates of 64 KiB each. Neither
credentials nor trust choices silently modify global Git configuration. Remote
publication still requires the exact target lease and explicit divergence policy.
