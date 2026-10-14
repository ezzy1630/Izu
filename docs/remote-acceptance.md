# Disposable GitHub acceptance

`scripts/remote-acceptance.py` requires an explicitly authorized disposable
repository selected with `--target OWNER/izu-acceptance-NAME`. There is no default
target. It rejects other repository names before authentication, requires the
authenticated owner to match exactly, and refuses creation unless the target is
confirmed absent. Existing repositories must match the selected private target.
The helper never deletes repositories. Candidate source and CI publication use
only the explicitly selected `candidate-ci` phase and integrator's frozen archive
after explicit publication approval. Selecting a target does not itself grant
permission to create it, publish source or configure branch protection.

Run from the intended checkout with an exact built binary and a private evidence
directory under the main checkout's `.artifacts/local`:

```sh
python3 scripts/remote-acceptance.py --target YOUR_LOGIN/izu-acceptance-check \
  --binary /absolute/path/to/izu \
  --evidence /absolute/main/.artifacts/local/remote-acceptance --phase setup
python3 scripts/remote-acceptance.py --target YOUR_LOGIN/izu-acceptance-check \
  --binary /absolute/path/to/izu \
  --evidence /absolute/main/.artifacts/local/remote-acceptance --phase exercise
python3 scripts/remote-acceptance.py --target YOUR_LOGIN/izu-acceptance-check \
  --binary /absolute/path/to/izu \
  --evidence /absolute/main/.artifacts/local/remote-acceptance --phase acceptance
```

Setup creates the private repository, seeds synthetic branches and a tag, and
independently clones and fscks with ordinary Git. Exercise imports the chosen
branch through the shared Izu engine and materializes it. Acceptance commits
native changes, pushes with exact leases, reads content with ordinary Git,
fetches into Izu, races independent Izu processes against the same remote lease,
exercises invalid and missing authentication, requests actual GitHub branch
protection, and cancels an upload before independent ref readback. The diagnostic
`capture` phase exports native synthetic data and publishes an absent
`receipt-capture` branch by HTTPS receive-pack, retaining only a bounded response
body and status/identity metadata.

Credentials come from the selected `gh` account into process memory. Izu receives
an explicitly selected environment variable for the exact HTTPS URL. Ordinary
Git uses an ephemeral askpass program containing no secret, with system/global
Git configuration disabled and credential helpers empty. The program is removed
on exit. Existing credential scopes are unchanged; exact-URL use does not prove a
repository-limited provider credential. Outputs are redacted before storage.
Receipts bind argv, cwd, binary SHA-256, status, timing and readback. Fixture data
and raw receipts are private artifacts, not public source deliverables.

The `final` phase uses a fresh native clone and absent `final-main`, `final-race`
and `final-cancel` branches, a fixed export committer timestamp, and explicit
checks for successful pushes, stale/auth/cancellation exits, one race winner,
and independent content/fsck readback. Run it with the final integrated binary
after preserving initial defect receipts. Supply a fresh `--run-label`, such as
`package-r1`, for later packaged-binary repeats; it names distinct branches,
fixtures and receipts.

Harness exit zero means commands were recorded. Deliberately tolerated negative
or uncertain command exits are not passed gates. Inspect receipts and readbacks.
Cancellation with an absent ref proves that observation; it does not prove
cancellation during server publication. A protection API 403 leaves protection
and permission-denial acceptance blocked. No local hook substitutes for GitHub
policy. Keep initial defect evidence distinct from the final integrated binary
rerun. Literal Izu clone requires the shared facade's clone entrypoint;
init/import/update alone cannot be reported as that interface.

Retain the remote until the integrator inspects readbacks and authorizes deletion
under the user's exact cleanup approval. Keep private evidence after cleanup and
independently confirm the target returns 404.

The `candidate-ci` phase requires `--source-root`, `--source-archive` and
`--source-digest` (exact archive SHA-256). It rejects archive links, devices,
escaping paths, Git metadata and private artifacts; bounds entries and expanded
bytes; and creates an owned disposable Git fixture commit. It pushes only the
absent `candidate-ci` branch, records independent ref readback, and queries runs
for that exact Git commit. The frozen workflow must trigger on that branch's
push. The helper does not edit workflows or infer CI success from dispatch.

Official hosted-runner labels distinguish `macos-14` ARM64 and
`macos-15-intel` x86-64. See the [GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Windows platform-only tests are partial evidence, not a supported full Windows
runtime or full-suite claim.
