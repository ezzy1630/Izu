# Running Codex inside an IZU change

`scripts/agent-host.py` is an explicit local Codex CLI adapter. It calls the
shared IZU managed `start` or `run` operation. IZU creates or resolves the private
workspace, checkpoints the source, binds the job to its exact head/tree and
directory identity, launches the adapter there, and checkpoints its result.
The adapter reads its canonical cwd before launching Codex and passes that
directory explicitly to `codex exec --cd`. It does not implement history writes.

This uses the installed Codex CLI and existing account authentication. It does
not install software, edit account credentials, load Codex user config/rules,
or message an existing chat. Model and reasoning settings apply to this launch:
`gpt-6.1-sol`, medium reasoning. The CLI's `--ephemeral` mode avoids saved Codex
session files; JSON events still identify the newly executed session.

The [official non-interactive documentation](https://learn.chatgpt.com/docs/non-interactive-mode)
describes `codex exec` and its JSON event stream. Check the installed CLI's
`exec --help` before changing its flags; this adapter uses its supported
`--ignore-user-config`, `--ignore-rules`, `--ephemeral`, `--json`, `--cd`,
`--skip-git-repo-check` and `--sandbox workspace-write` options.

## Launch

Create a prompt file and a private local state directory under this main checkout's
canonical `.artifacts/local`. The adapter resolves a development worktree's main
checkout through its Git directory metadata; a similarly named path elsewhere is
rejected. The state directory must be outside the native repository's
source root, so that credentials cannot enter a source checkpoint. The current
adapter is suitable for explicitly selected native repositories or disposable
fixtures stored beneath `.artifacts/local`; it refuses a state home inside the
repository being launched.

```sh
python3 scripts/agent-host.py start \
  --izu /absolute/path/to/fresh/izu \
  --repo /absolute/path/to/native-project \
  --change feature \
  --codex /absolute/path/to/installed/codex \
  --state-root /absolute/izu-checkout/.artifacts/local/host-state \
  --auth-file /absolute/path/to/existing/auth.json \
  --prompt-file /absolute/path/to/task.txt \
  --evidence /absolute/izu-checkout/.artifacts/local/host-run-unique
```

Use `run` with the same change to continue existing work. `start` also resumes a
known named change through IZU's normal ambiguity and writer-ownership guards.
The evidence directory must be new. A launch without `--auth-file` can use
credentials already present in its environment; it does not start login flows.
No authenticated execution is claimed until actual model events and resulting
files have been checked.

The adapter creates a private temporary `CODEX_HOME`, copies a supplied auth file
with mode 0600, and removes that temporary directory in `finally`. Token refresh
can update the temporary copy. The supplied auth file is only read. `TMPDIR`
and `XDG_CACHE_HOME` also use the private state directory. Abruptly killing the
adapter with SIGKILL bypasses Python cleanup; inspect and remove its owned
temporary state before retaining or sharing the surrounding artifacts. Do not
include credentials in prompts, source files, or reports.

## Verify actual behavior

`launch.json` binds the IZU binary SHA-256, adapter SHA-256, installed Codex version
and explicit argv. `binding.json` records the canonical cwd and source hashes
before model execution. Codex's events and stderr are separate bounded files
(eight MiB each); exceeding a bound fails the host. `host-result.json` records
its actual exit and resulting source hashes. Source evidence is bounded at
10,000 entries and 128 MiB, and refuses special filesystem entries. The native
`izu-response.json` retains actual workspace/checkpoint/job receipts, including
failure or uncertainty. Cancelled jobs can lack a host-result file because the
host was terminated; inspect the native receipt and retained source.

```sh
python3 scripts/agent-host-verify.py /absolute/path/to/host-run-unique \
  --file result.txt 'expected text'
```

The verifier compares the host cwd, native workspace ID, preparation head/tree,
actual terminal exit and caller-supplied file contents. Without `--file`, it
proves the binding and successful execution only. Agent final messages and exit
zero do not prove that a task was implemented correctly. Run the native exact
candidate checks before landing; this host adapter never lands or publishes.

## Cancellation, concurrency and recovery

IZU's runtime owns the cooperating process group and its job reservations.
Separate change names select independent private source directories. Runtime
admission and cancellation remain engine behavior; the adapter provides no
additional concurrency cap or security isolation guarantee. A private workspace
is not a sandbox against arbitrary external writers.

Cancel the owning foreground host adapter or IZU controller with SIGINT for cooperative shutdown,
then inspect its native receipt and source before restarting the same named
change. Do not signal saved worker PIDs after reopening a runtime. The observed
local host pilot found cross-process `jobs cancel JOB_ID` rejecting an actively
owned managed job with a typed ownership conflict; until that engine behavior is
changed and verified, use the owning controller's cancellation path.

Restart creates a new Codex session in the same native change. It does not resume
the ephemeral model conversation. IZU retains checkpoints and source edits,
including edits written before cancellation. Use `recover`, `jobs status` and
`writer` inspection when ownership or persistence is uncertain; do not assume a
failed response rolled history back.

## Repeat the actual host campaign

The campaign creates a new owned native fixture, establishes a human sentinel,
launches five real Codex sessions, observes the default four-job cap with another
queued, and verifies each agent's bytes and independent registered workspace.
This consumes model usage. Its state and evidence remain inside the supplied
new `.artifacts/local` directory; temporary authentication copies are removed.

```sh
python3 scripts/agent-host-campaign.py \
  --izu /absolute/path/to/fresh/izu \
  --codex /absolute/path/to/installed/codex \
  --auth-file /absolute/path/to/existing/auth.json \
  --root /absolute/izu-checkout/.artifacts/local/host-campaign-unique
python3 scripts/agent-host-integrate.py \
  --izu /absolute/path/to/same/izu \
  --campaign /absolute/izu-checkout/.artifacts/local/host-campaign-unique
```

Wait for the whole campaign to return before integration. A job's `Finished`
state can precede the managed wrapper's final checkpoint. The integration
campaign commits two actual model results in the disposable fixture, runs exact
candidate checks, lands the first, proves stale-target rejection of the second,
retains and resolves their `shared.txt` conflict, proves a new resolved check is
required, then lands and updates the human workspace. It checks that all five
private source trees and the human sentinel remain intact.

The campaign registers each launched adapter in its own owned process group and
closes/reaps every process and log handle when observation fails, times out or is
interrupted. It first requests cooperative cancellation through the live owned
handle; after thirty seconds it can forcibly stop only that owned group. Native
runtime receipts remain authoritative about cancellation and retained source.
Final reaping waits at most another five seconds. An OS stall can prevent even a
killed process from becoming reapable; that timeout fails cleanup explicitly and
must not be read as proof that every descendant stopped.
Forced termination can leave persistence or runtime ownership uncertain, so
inspect those receipts and recovery state before reuse.


The adapter explicitly selects `--cooperative-interrupt-ms 5000`. Cancellation
uses the retained controller-to-worker pipe; the worker interrupts its live
adapter child, which forwards SIGINT to the live Codex handle and waits for its
tool cleanup. The worker records the observed command exit; the controller then
performs its mandatory hard process-group cleanup before checkpointing. This
bounded grace is separate from the execution deadline. Missing acknowledgment,
expired grace or uncertain cleanup retains writer intent and reports
`OwnershipUnknown`; source is not checkpointed or replaced automatically.
Default managed commands retain immediate hard cleanup unless they explicitly
select this cooperating-adapter contract. This does not contain arbitrary
writers which evade the selected host's cleanup. Loss of the controller still
requires inspection and explicit stopped-writer recovery.
