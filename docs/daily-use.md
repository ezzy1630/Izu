# Daily use with local agents

Start with a development pilot on native persistent storage and an independent
copy of your work. Izu's useful difference is managed private workspaces,
checkpoints around execution, and checks bound to the exact integration candidate.
It is not generally faster than Git in the recorded comparisons. Current limits
and remaining release gates are in [status](status.md).

## Start and continue a change

Use the executables in one verified package's `bin` directory. The helpers need
Python 3.11 or newer; agent execution also needs an authenticated Codex CLI.
The selected workspace's project instructions load automatically. Personal
instructions are explicit. The prompt below is an example, not a permission
grant to change an existing project.

```sh
izu-agent start --repo /path/to/project --change parser \
  --instructions-file /path/to/personal/AGENTS.md \
  --prompt "Implement the agreed parser change and preserve unrelated edits"
izu-agent continue --repo /path/to/project --change parser \
  --instructions-file /path/to/personal/AGENTS.md \
  --prompt "Address the review findings"
```

The launcher prints a private run directory. Use `izu-agent status --evidence DIR`
or `izu-agent cancel --evidence DIR` for that run. Cancellation preserves source;
inspect the terminal native receipt before continuing. Continuation starts a new
model conversation in the same native workspace. Use `izu-agent-verify DIR` to
check successful execution, workspace binding and final source readback. Task
correctness still needs review and project tests. See [agent host](agent-host.md).

## Review, check and land

Review the selected change, then commit and land with explicit author settings:

```sh
izu --repo /path/to/project --change parser diff
izu --repo /path/to/project --change parser commit -m "Parser change"
izu --repo /path/to/project --change parser land --current --check cargo-tests -- \
  /path/to/package/bin/izu-cargo-check --state-root /private/check-builds -- test --locked
```

Set `IZU_AUTHOR_NAME` and `IZU_AUTHOR_EMAIL` or supply native author flags. The
Cargo helper gives each check a private external target and temporary directory;
Cargo incremental hard links must not enter captured source. In this development
checkout, add `--cargo-wrapper /absolute/main/checkout/scripts/izu-env` before
`--` to select its isolated toolchain. Build outputs and failed recovery paths
are retained for inspection. Other build tools need the same separation for
ignored caches that cannot be captured safely. Do not weaken capture guards.

Landing advances the target reference after its exact checks pass. It does not
replace the human checkout. `izu update` performs the separate guarded update;
inspect unfinished human work first. The engine rejects stale candidates and
unknown ownership rather than overwriting work.

## Back up unfinished work and prove recovery

Stop managed jobs through their live controller and avoid edits during backup.
Then choose a new output directory outside captured source:

```sh
izu-backup create --repo /path/to/project --output /private/backups/project-001
izu-backup restore --backup /private/backups/project-001 --destination /new/recovered-project
```

Creation checkpoints all registered workspaces and restores the resulting bundle
before reporting success. Restoration creates `main` for the selected workspace
and `workspace-ID` directories for every other archived workspace. Each is an
independent native repository containing the full archived history and its own
restored source. The JSON receipt maps original workspace IDs to those paths.
The command never overwrites an existing destination.

Keep `backup.json` and `history.izu-bundle` together. Copy them to an independently
stored private location and repeat restoration there before treating it as an
off-device backup. A local drill cannot protect against losing that device.
Bundles can contain historical paths, private recovery objects and ignored data;
do not publish them as source archives. Live databases, uncheckpointed external
files and runtime caches are not independently backed up by this command.

## GitHub remains the source-sharing remote

Use Izu's explicit Git exchange to fetch and publish supported branch and tag
history. Expected-old reference checks protect publication from overwriting a
new remote head. GitHub does not store Izu's complete operation log, private
workspace state or all unfinished checkpoints; retain native backups too.

See [Git interoperability](git-interop.md) for exact commands and supported
formats. Submodules, LFS content transfer and Git working-tree conversions remain
outside the first-release scope. Use the existing public `ezzy1630/Izu` repository
for source CI: standard GitHub-hosted runners are free for public repositories,
and public branch protection is available on GitHub Free. The disposable private
repository's billing and plan restrictions do not block this route. Fresh public
CI and deliberately configured required checks are still needed before treating
that route as a protected release gate. See [historical diagnosis](historical-diagnosis.md).
