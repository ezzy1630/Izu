# Exact storage reclamation

The first reclamation policy deliberately retains all unique work by default.
Lack of HEAD reachability, process age, wall-clock age, and an abandoned writer
are not consent to discard content. `Store::reclaim_selected` accepts exact full
native object identities selected for deliberate disposal. It never discovers a
set of garbage to delete automatically.

## Caller and lifetime contract

`Repository::retention_lease` returns an owned shared retention lease. A caller
holds it before reading the first object in a
multi-object traversal or preparing the first object in a multi-step write. The
lease remains held through publication or explicit abandonment. Preparation may
stream outside the HEAD publication transaction, but not outside this retention
lifetime. Dropping a preparation lease abandons the active process reservation;
it does not discard the resulting immutable objects. A caller that retains a
prepared identity for later publication keeps the lease for that entire interval.
Process exit releases the kernel lease without authorizing removal of its work.

Reclamation holds `temporary.lock` exclusively through recovery, validation,
quarantine, removal and persistence. It does not acquire `head.lock`; publication
holds the shared retention lease before replacing HEAD. This preserves the
HEAD-to-retention lock order used by transactions. Store raw object reads used
inside reclamation must not recursively acquire a shared retention lease while
it is exclusive. Public engine operations establish the outer read/preparation lifetime;
`WorkspaceGuard` and `WorkspaceWriterLease` retain it until dropped. Transactions
retain it from the locked HEAD read through publication. Adapters that use prepared
IDs across engine calls hold their own caller lease through the final operation;
a single-object read does not protect a later traversal of references.

Example request:

```text
plan = store.plan_reclamation([exact_blob_id, exact_orphan_tree_id], cancellation)
outcome = store.reclaim_selected([exact_blob_id, exact_orphan_tree_id], cancellation)
```

The plan is read-only and advisory. Reclamation recomputes it with writers and
retained readers excluded; the earlier plan cannot authorize a stale deletion.
Empty selection removes nothing. Duplicate and absent identities are errors.
There is no abbreviated identity, wildcard, elapsed-age or automatic expiry API.

Every finalized object outside the selection remains a root. Every object and
its canonical metadata is validated, including objects unreachable from HEAD.
Every native reference must exist with its expected kind. Any incoming edge from
an unselected object to a selected object rejects the entire selection before
canonical removals. Thus an unpublished tree protects its file blobs, conflict
alternatives remain retained, and historical candidates and evidence protect
all referenced inputs. Every Operation is always retained, including unpublished
operations used by recovery receipts. Selecting an operation is rejected.

Engine reclamation additionally rejects unresolved external writer intents.
`Repository::plan_reclamation` and `Repository::reclaim_selected` acquire the
store's typed `ExclusiveReclamationGuard` before inspecting `workspace-locks`,
then hold it through the store plan or mutation. Every unresolved `live-*.intent`
blocks disposal, including after process exit. Unknown names, nonregular entries,
malformed identities and corrupt intent records fail closed. Explicit stopped
acknowledgement clears only the exact intent token. Store-only graph validation
cannot infer the meaning of arbitrary engine intent files. This policy is conservative: referenced history and operation chains do
not shrink, and old unique preparation objects remain unless explicitly selected.
Long-lived stores can reclaim deliberately discarded orphan groups without
silently expiring native history. Automatic retention expiry requires a separate
versioned durable ownership and history-pruning policy.

## Quarantine protocol and interruption

The anchored store directory `reclamation` is a recovery namespace, not a source
of persisted deletion consent. Recognized entries have an exact lowercase
64-character ObjectId name and a regular file containing the verified native
frame for that identity. Object frames keep their existing independent native
format version. Unknown names and entry kinds are retained and counted; recognized
corrupt frames fail closed. Scans obey configured object and byte budgets, and
metadata reads obey the in-memory budget.

For each selected object, reclamation creates a no-replacement hardlink in that
namespace and persists the file. It persists the quarantine directory before
removing any canonical entry. It verifies pinned file and directory bindings
against the anchored store layout throughout the operation. It removes canonical
entries and persists each affected shard while the recovery hardlinks survive.
Only after all removals are persisted does it purge the selected quarantine
entries and persist the quarantine directory. It acknowledges completion only
after those barriers. Filesystem durability limitations still apply to the
underlying platform; volatile test mode cannot return a durable receipt.

Cancellation before the first canonical removal returns an ordinary error and
leaves all canonical objects available, possibly with recoverable extra links.
After the first visible canonical removal, the operation attempts its persistence
barriers without honoring cancellation. Later failure returns
`ReclaimOutcome::VisibleButUncertain` with the exact selected identities and
payload-byte accounting. It must not be described as rollback or completion.
The recovery namespace is `.izu/reclamation` relative to the engine repository's
metadata directory (or `reclamation` relative to a standalone Store root).

Recovery verifies all recognized quarantine files before changing entries. It
links each surviving file back to its canonical object name, persists the file
and shard, then removes and persists its recovery name. If the canonical name
already exists, its identity and complete content must validate. Unknown entries
remain untouched. A new reclamation request performs this restoration first;
it never resumes a stale discard request. A crash during final purge may leave
only part of an explicitly discarded set recoverable. No receipt promised that
set survived deliberate disposal, and no retained graph may reference it.

Recovery errors can leave restored entries visible and quarantine copies retained.
They do not mean restoration rolled back. Reopening the store and rerunning
recovery is safe. A corrupt recovery object is kept for inspection and prevents
a new discard from proceeding.

## Verification boundaries

Store tests cover exact deletion, retention of unselected unique work, incoming
edges from orphan trees, operation retention, corrupt roots, missing identities,
unknown quarantine entries, cancellation, and injected failures before and after
canonical entry removal. The store suite also exercises actual child-process exits at four reclamation
boundaries. Engine tests exercise active output readers, caller preparation gaps,
workspace guards and writer leases, crashed intent retention, explicit stopped
acknowledgement, native history retention, unknown/corrupt entries and recovery
after interrupted reclamation. These tests verify the engine/store paths; adapter
and CLI checks remain distinct integration evidence.
