# Owned cooperative process execution

`izu-process` provides a shared safe Rust subprocess boundary for Git transport,
validation and check execution. Dependencies are general-purpose `rustix` 1.1
with `fs`/`process` features and `tempfile` 3. It contains no unsafe code, shell
interpolation, process engine copying, output-reader threads or blocking joins.
Verified operating systems in this lane are macOS and Linux arm64. Windows
process control is not implemented.

## Caller contract

`CommandSpec` carries the exact `program: OsString`, `args: Vec<OsString>`,
`directory: Option<PathBuf>`, and `environment: Vec<(OsString, Option<OsString>)>`.
The last field sets or removes environment variables. Builders `new`, `arg`,
`args`, `current_dir`, `env` and `env_remove` preserve raw OS bytes. Direct fields
are validated before launch: NUL, empty programs, malformed environment keys and
duplicate explicit environment keys are rejected. Debug output contains counts,
not command arguments or environment values.

This explicit shape matters: `std::Command` getters hide `env_clear` and replace
invalid NUL arguments with a placeholder. Transcoding those getters can silently
change a spawn rejection into another command. The shared runner never does so.
Only the declared fields cross the worker boundary; callers cannot supply hidden
`pre_exec`, UID, process-group or stdio configuration to the generic runner.

`WorkerLauncher { executable: PathBuf, prefix_args: Vec<OsString> }` selects the
specific facade/helper. `run(&CommandSpec, input, &RunOptions, &WorkerLauncher,
&cancelled)` returns `Result<Outcome, RunError>`. `run_interactive` adds a trusted
bounded `FnMut(&[u8], &[u8]) -> bool` predicate: after all input is written, stdin
remains open until the predicate matches retained stdout/stderr. Normal `run`
closes stdin after writing. MCP response parsing remains in the caller.

`RunOptions` controls stdin/stdout/stderr byte caps, timeout, cleanup timeout,
`OutputPolicy::{Terminate, TruncateDrain}`, and explicit
`BaseEnvironment::{Clear, Inherit}`. Clear starts the target with only declared
variables; Inherit snapshots the caller environment and applies explicit
changes. The worker itself starts with a clear environment.

`Outcome` carries the target exit status, retained captures, exact dropped-byte
counts, observed EOF flags, written input count, termination reason, cleanup
report and executable path. `command_elapsed` is worker-observed target
spawn-start through target exit (including polling latency); `elapsed` includes
worker launch, target, cleanup and final drain; `cleanup_elapsed` measures only
cleanup/drain. Prelaunch failures use typed `RunError`; failures after launching
use an outcome with cleanup evidence. Errors never include ticket/argv/env data.
Target stderr remains caller-owned output and may contain its own diagnostics.

A caller may accept command output only after checking termination, exit status,
capture truncation/EOF and cleanup according to that operation's policy.
`Completed` describes target completion, not escaped-descendant containment or
complete output. Git must reject truncation and missing EOF. Validation may
retain explicitly truncated output but cannot report it as complete.

## Retained worker and versioned boundary

The normal CLI uses its exact current executable with prefix `__process-worker`;
the facade delegates that hidden mode to `worker_main()`. The optional standalone
`izu-process-worker` binary supports library callers and disposable tests. There
is no service or daemon to install. A crate-owned test-child binary is a fixture,
not a product interface.

The launcher canonicalizes the supplied worker path and requires a regular file.
A private `ep*` directory under the caller-selected system temp root (`TMPDIR`
on Unix) is explicitly created with permissions `0700`;
a Unix socket inside carries a bounded command ticket and status messages. The
runner honors the selected temp root and never falls back to an internal-disk
path if socket binding fails. The caller must select a short path within the
platform Unix socket path-length limit. The local task helper selects
`.artifacts/local/tmp-root` under the project root for these tests.
Command argv never appears in worker argv; the worker receives only the channel
path. Worker handshake checks magic/version `IZUP`/1 and the exact owned child
PID. The worker validates it is its own process-group leader before launching a
target. The selected target inherits the worker's group and raw stdin/out/err.
The worker reports target status and remains alive until parent cleanup, holding
the group identity throughout. If the parent disconnects, the worker terminates
its own cooperative group. A post-launch worker failure has the same cleanup
lease. Group identity checks precede that lease.

Ticket framing is four-byte little-endian length, then five-byte magic/version,
length-prefixed raw program bytes, argument count and fields, optional directory,
and environment count plus key/value fields. Limits are 1 MiB total encoded
request, 4,096 arguments and 4,096 environment entries. Decode rejects trailing
bytes, invalid counts, NUL and malformed keys. Status is bounded at 256 bytes:
handshake magic/version plus PID; `S` target spawned; `E` raw Unix exit status and
elapsed nanoseconds; `F` target spawn errno. Both implementations belong to the
same versioned crate; callers do not manipulate this private protocol.

Canonical executable path and owned-PID/version handshake identify the requested
worker boundary. They are not cryptographic proof of an executed image against
adversarial same-user filesystem replacement. Integration evidence must bind the
actual facade/helper build and source separately. Private directory permissions
protect against other users, not an attacker running as the same user.

## Lifecycle and bounded I/O

`OwnedProcess::spawn(&mut Command)` is the lower-level owner used for existing
runtime workers. It sets `process_group(0)` using safe `CommandExt`, stores a
private `Child` and has no constructor from arbitrary PIDs or external children.
`id()` is read-only bookkeeping; `take_stdin/stdout/stderr` transfers pipe
ownership. `observe_exit()` uses `waitid(WNOWAIT | WNOHANG | WEXITED)` and never
reaps the leader. `stop(Duration)` checks ownership, sends group SIGKILL before
reaping, then bounds leader-reap polling. After reaping it performs only a harmless group
existence probe (signal zero), never another real signal. `ESRCH` establishes
`GroupQuiescence::AbsentObserved`; group presence, permission error or any other
uncertainty remains unknown when the shared cleanup deadline expires. A reused
numeric group can cause only a conservative failure to prove absence. No process
inventory or sleep-only assumption is used. Repeated stop returns the cached report
and sends no further signals. No public signalling API accepts a PID.

macOS can return `EPERM` for `killpg` after the direct leader exited, even with no
live descendants. Cleanup retains this signal diagnostic; only an independent
post-reap absence observation can resolve whether the group stopped. The generic
retained-worker path keeps a live leader
until group signalling, avoiding that normal-exit ambiguity. The real direct
zombie test reproduces `EPERM`; normal retained-worker tests report signal sent
and leader reaped. A worker that already killed its own group can produce the
same `EPERM` diagnostic and still have independently established group absence.

Capture uses nonblocking descriptors and a single fair loop. Each iteration
writes/reads at most 16 KiB per stream and checks cancellation/deadline. It sleeps
1 ms only if all channels make no progress. Stream caps either terminate the
group or discard/count excess while draining. Storage is bounded by configured
retained caps (allocator capacity rounding is outside the retained-byte count).
Large stdin, stdout and stderr make progress concurrently. No thread join waits
for a descendant-held pipe. After signalling and reaping, final draining shares
one cleanup deadline; an escaped live writer yields `eof=false`, and the caller's
read descriptors close on deadline.

Cleanup timeout is limited to 30 seconds. Defaults: input/output 16 MiB, stderr
64 KiB, run deadline 30 seconds, cleanup deadline 2 seconds. Encoded-ticket and
capture reservations are fallible; checked arithmetic rejects resource-size
overflow. Caller-owned command/input memory and caller callback work are outside
the runner's allocation/time control. Operating-system spawn, filesystem,
allocator and scheduling latency are not hard real-time bounded; deadlines bound
the cooperative polling/drain path once those calls return.

`CleanupReport::cooperative_stop_succeeded()` means the owned leader was reaped
and subsequent harmless probing observed that the process group was absent.
Explicit `OwnershipLost` always prevents success. A failed parent signal remains
visible in the report but does not invalidate independently proved absence.
Signal delivery and leader reaping alone never establish this result. Runtime must keep its durable writer
marker when group absence is unproved: a same-group process with pending SIGKILL
or delayed kernel termination can still be a writer. The absence observation
proves no normal same-group member existed at that observation, not that every
descendant was reaped. It does not prove all
possible descendants are gone: processes may change session, group or UID;
SIGKILL can exclude processes the caller cannot signal; kernel states can delay
termination (which prevents group-absence proof while membership persists).
Normal nested helpers that deliberately form another group are also outside the
original group profile and require their own lifecycle proof. Process groups are cooperative control, not containment. Other
code must not reap this owner's child through process-wide wait calls. Such a
reaper invalidates PID ownership; detected ownership loss prevents signalling.
Exclusive reaping is necessary because POSIX lacks a cross-platform immutable
process-group capability. Dropping an unfinished owner does only nonblocking
best-effort cleanup, returning no evidence of success. It performs no diagnostic
write, because a full inherited stderr pipe could block a destructor forever.
Callers requiring cleanup evidence must explicitly call `stop`; dropping an owner
cannot authorize clearing a durable writer marker.

POSIX background references: [waitid / WNOWAIT](https://pubs.opengroup.org/onlinepubs/9799919799/functions/waitid.html)
and [process-group signals](https://pubs.opengroup.org/onlinepubs/9799919799/functions/kill.html).

## Local evidence

Use the isolated Rust 1.99 toolchain and private target directory:

```sh
scripts/izu-env \
  cargo test --locked --offline -p izu-process
```

Three unit tests cover actual private directory permissions, malformed bounded
protocol data and the rule that signal/reap evidence alone is insufficient. Twenty-one real lifecycle tests cover normal/nonzero exits, concurrent
2 MiB stdin/stdout/stderr, retained descendant pipes, TERM ignoring, escaped
`setsid` writers, timeout/cancel, size caps, spawn failure, raw non-UTF8 argv,
literal shell syntax, environment, interactive stdin closure, hidden facade mode,
wrong worker version, direct zombie uncertainty, no signalling after reap and
nonblocking drop with an undrained full stderr pipe, raw invalid input, working directory/environment removal and
parent-disconnect self-cleanup, post-reap group absence and same-group background
writer quiescence and a retained worker killing its own group. The Drop regression
first reproduced a blocked destructor with a full undrained stderr pipe: the
fixture sent its ready handshake but could not send its dropped handshake. After
removing the synchronous diagnostic, it completes while stderr remains full.
The self-kill test
first reproduced the old macOS failure: parent signal `EPERM`, leader reaped,
and observed group absence incorrectly returned an unsuccessful stop. It passes
with the independent-absence criterion. The test-only background-child fixture deliberately does not
wait: exiting while a descendant retains output pipes is the regression being
reproduced. That fixture alone has a documented Clippy zombie-process exception.

Historical pre-rename `ezy-process` debug and release each passed all 24 tests
on macOS and Linux arm64 on 2026-10-03. That library's SHA256 was
`6da23892502727430b3bbf954a8546cd3180541bbd166d0a4398f572d094d32c`;
it does not identify the renamed `izu-process` source or `IZUP/1` protocol.
The original logs and recorded `ezy` paths under `.artifacts/linux-process`
remain unchanged; its `pre-drop-fix` subdirectory contains earlier evidence.

Fresh renamed source uses library SHA256
`4d52faec85ad9d64c11d7503370b0bfa63b76ba158901eb583a5f62f45156335`.
Evidence for it is separate under `.artifacts/izu-process-final`: raw test and
Clippy logs, toolchain metadata, exact source and helper binary SHA256 files,
and Linux image inspection and invocation scripts. Hidden facade mode and
wrong-version rejection tests exercise the renamed `IZUP/1` handshake.
Fresh Mac and Linux debug and release runs each passed all 24 process tests and
15 merge tests. Mac strict Clippy passed for all targets in debug and release.
These checks used the renamed source and freshly built `izu` helper binaries.

Linux verification uses Rust 1.99.0 (`b940084d7`) on arm64 Linux
7.0.12-linuxkit and Docker image
`rust@sha256:59037199c44290f2befcdd58dcc540164763fc296950255aaefeef096a1866b0`.
The container uses a read-only root/source and a Neural-only writable bind for
Cargo cache, targets, temporary sockets and evidence. It uses `--init` to provide
a normal child-reaping environment; no claim is made about an init-less
container that indefinitely retains orphan zombies.

Focused release kernels are in `examples/process-kernels.rs`, using the explicitly built
standalone worker and fixture paths. These measurements are historical, from
the pre-rename `ezy` source on this macOS machine on 2026-10-03:
unconditional 1 ms polling sleep measured 176,325 us/op full execution and
166,804 us/op target duration for simultaneous 2 MiB stdin/out/err (five runs).
Sleeping only on idle channels measured 22,408 and 4,327 us/op respectively.
64 KiB echo full execution varied from 30,123 to 42,752 us/op (20 runs), so no
improvement is claimed for that kernel. These are local process-kernel timings,
not VCS superiority or repository-scale benchmark claims.
