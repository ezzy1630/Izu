# Windows boundary candidate

This candidate is compiled through `tests/windows_native.rs`; it is not selected
by the production library exports. It preserves Windows durable-publication
rejection. Native CI is an experiment on a disposable hosted Windows filesystem,
not a hardware power-loss test or a product-support claim.

## Anchoring and ownership

`windows/fs.rs` contains safe policy and `windows/nt.rs` contains the limited FFI.
Root lookup supports disk and UNC roots, then each ordinary component is opened
using `NtCreateFile` with `OBJECT_ATTRIBUTES.RootDirectory`. Every child lookup
uses a single bounded UTF-16 component and `FILE_OPEN_REPARSE_POINT`; handles
with `FILE_ATTRIBUTE_REPARSE_POINT` cannot anchor directory traversal or serve
as regular source/lock files. No operation rebuilds a child pathname from the
directory's former location. Parent traversal, device namespaces, alternate
streams, separators, wildcards, control characters, trailing spaces/dots and
reserved device aliases are rejected.

The handles are synchronous and non-inheritable. Each successful open transfers
exactly one handle into an owning Rust `File`; failed opens never manufacture a
handle. Cloning borrows an existing object identity. Opens permit read/write/delete
sharing so concurrent writers can acquire the same permanent lock object and so
held handles do not depend on the entry remaining at its original pathname.
Creation inherits the parent Windows ACL; this candidate does not yet establish
private-ACL or current-user ownership equivalence to Unix mode 0600/0700. That
must be reviewed before any private repository/runtime admission is enabled.

File creation uses atomic `FILE_CREATE`, never a separate existence check.
Permanent locks are opened without truncation or replacement; after a creation
race only `AlreadyExists` permits reopening. Actual mutual exclusion belongs to
the caller's shared file-lock boundary. Rename and link targets use held
destination directory handles in `NtSetInformationFile`. No-replacement is a
kernel operation. Deletion opens the named object without following a reparse
point, then requests disposition on that exact object; it never traverses or
recursively removes descendants. Reparse target decoding and anchored symlink
creation remain unsupported.

Enumeration uses a zero-length counted NT name relative to the held directory
to reopen that object independently for each cursor, and
requests one native record at a time into an aligned fixed 4096-byte buffer.
The response byte count, fixed header, even UTF-16 length and maximum component
length are checked before parsing. Reparse records are reported as `Other`,
because a reparse point is not necessarily a symlink. The name allocation is
bounded and fallible at the native decoding boundary. An enumeration error
terminates that cursor. Concurrent namespace edits can affect which entries a
scan sees; it is not a snapshot.

## FFI review

The general-purpose `windows-sys` 0.61.2 binding features are Foundation,
Storage/FileSystem, System/IO, Security (OBJECT_ATTRIBUTES pointer types),
Wdk/Foundation and Wdk/Storage/FileSystem. Calls are read/open/query/set/flush
operations on owned or borrowed filesystem handles. There are no hand-declared
imports, global pointers, asynchronous callbacks, assembly or C implementation.

Every unsafe call documents pointer ownership, alignment, storage size and
synchronous lifetime. Rename/link buffers have a fixed `repr(C)` layout and
their root, filename-length and filename offsets are asserted against the
generated authoritative structures at compile time. NTSTATUS failures are
mapped with `RtlNtStatusToDosError`. Native enumeration bytes are parsed with
checked safe byte operations rather than typed pointer casts. These checks do
not substitute for native execution and an independent source review.

## Native failure diagnosis

The initial hosted Windows 2025 run reported `InvalidFilename` (123) for the
writable directory probe and for the combined atomic-mutation/enumeration test.
Both paths used a literal dot component with `NtCreateFile`; the original log
cannot locate the combined test failure more precisely. This revision removes
that Win32 dot convention from self-reopening and submits a zero-length counted
name relative to the held directory instead. It preserves handle anchoring,
reparse inspection and an independent scan cursor. Native acceptance remains
required: the tests print mutation-stage checkpoints and failed native calls
print NTSTATUS, access/class and bounded component information. The anchoring
test also checks enumeration and reopened identity after namespace replacement.
Cross-compilation alone does not establish the proposed self-reopen behavior.

## Persistence experiment and acceptance boundary

Run on a native Windows x86-64 host with the pinned Rust toolchain:

```sh
cargo test --locked -p izu-platform --test windows_native -- --nocapture
cargo clippy --locked -p izu-platform --all-targets -- -D warnings
```

The tests use fresh owned temporary directories. They cover file and directory
handle anchoring after namespace replacement, reserved/escaping components,
reparse rejection, atomic no-replacement collisions, hard-link identity,
nonempty-directory deletion refusal, independent enumeration cursors, and
concurrent permanent-lock creation without truncation. Reparse testing requires
native symbolic-link creation permission; failure is a failing check, never a
silently skipped success.

The barrier experiment writes an actual file, requires successful NT and Win32
file flushes, and prints both directory-flush results for read-only and writable
directory handles. It then asserts every public durable barrier still returns
`Unsupported`. A successful directory call establishes only that this host
accepted that request. Enabling durable acknowledgements requires a documented
filesystem/storage guarantee that the barrier covers directory-entry create,
rename and removal, together with crash/fault-point and concurrent-publication
tests. Device cache behavior and power loss are separate evidence requirements.
Volume-wide flush is documented as requiring administrative privileges; this
candidate neither obtains privileges nor substitutes such a flush silently.

## Primary API sources

- [NtCreateFile](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntcreatefile)
  documents relative RootDirectory names, synchronous options, sharing and reparse
  opening. The user-mode form is NtCreateFile, supplied by ntdll bindings.
- [NtSetInformationFile](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntsetinformationfile)
  documents information-class structures and DELETE access for rename/disposition.
- [FILE_RENAME_INFORMATION](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information)
  and [FILE_LINK_INFORMATION](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_link_information)
  specify relative target handles and kernel no-replacement behavior.
- [NtQueryDirectoryFile](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntquerydirectoryfile)
  documents synchronous directory queries and bounded output buffers.
- [FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)
  describes required write access, storage flushes and privileged volume flushing.
- [ZwFlushBuffersFileEx](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-zwflushbuffersfileex)
  distinguishes data-only/no-sync modes from ordinary flush and gives the
  user-mode Nt API naming. The candidate uses ordinary NtFlushBuffersFile and
  does not use modes that omit metadata or device cache synchronization.

Windows process ownership still requires an independently implemented Job Object
runner, suspended-worker assignment before target startup, bounded pipe I/O,
parent-loss cleanup and observed writer quiescence. Repository workflows remain
blocked until persistence, private ownership and process contracts all pass.
