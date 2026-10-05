//! FIFO admission for cooperating HEAD users. Kernel HEAD locking remains the
//! publication boundary; permanent claimant leases order its contenders.
use crate::durability::{Identity, check_directory_entry, check_file_entry};
use crate::{Result, Store, StoreError, StoreOptions, io_error, kernel_file};
use izu_model::CancellationToken;
use izu_platform::Directory;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::thread;
use std::time::{Duration, Instant};

const QUEUE_DIRECTORY: &str = "head-admission-v1";
const GATE: &str = "gate.lock";
const MAX_CLAIMS: usize = 1_024;
const TICKET_MAGIC: &[u8; 8] = b"IZUHQUE1";
const TICKET_LEN: usize = 56;

#[derive(Debug, Clone, Copy)]
pub(crate) enum HeadMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub(crate) struct HeadLease {
    // Drop order is significant: a claimant must cover actual HEAD ownership.
    _head: File,
    _claim: Claim,
    _directory: Directory,
}

struct Budget {
    started: Instant,
    timeout: Duration,
}
impl Budget {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }
    fn check(&self, cancel: &CancellationToken) -> Result<()> {
        cancel.check()?;
        if self.started.elapsed() >= self.timeout {
            return Err(StoreError::LockTimeout("head.lock"));
        }
        Ok(())
    }
    fn pause(&self, cancel: &CancellationToken) -> Result<()> {
        self.check(cancel)?;
        let remaining = self
            .timeout
            .checked_sub(self.started.elapsed())
            .ok_or(StoreError::LockTimeout("head.lock"))?;
        thread::sleep(remaining.min(Duration::from_millis(5)));
        self.check(cancel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot(u16);
impl Slot {
    fn at(index: usize) -> Result<Self> {
        if index >= MAX_CLAIMS {
            return Err(StoreError::LimitExceeded("head admission waiters"));
        }
        u16::try_from(index)
            .map(Self)
            .map_err(|_| StoreError::LimitExceeded("head admission waiters"))
    }
    fn name(self) -> String {
        format!("slot-{:04}.lock", self.0)
    }
}

#[derive(Debug)]
struct Claim {
    file: File,
    slot: Slot,
    sequence: NonZeroU64,
}
#[derive(Debug, Clone, Copy)]
struct Predecessor {
    slot: Slot,
    sequence: NonZeroU64,
}
struct Queue {
    directory: Directory,
}
struct Registration {
    claim: Claim,
    predecessors: Vec<Predecessor>,
}
struct Reservation {
    file: File,
    slot: Slot,
    length: u64,
}

// Prepare the first claimant before FORMAT makes a new store discoverable.
// Kernel submissions are covered by init's existing strongest device barriers;
// coordination itself is reconstructable and needs no additional full flush.
pub(crate) fn initialize_new(root: &Directory, options: &StoreOptions) -> Result<()> {
    let queue = Queue {
        directory: root
            .create_dir(OsStr::new(QUEUE_DIRECTORY))
            .map_err(|error| io_error("create HEAD admission directory", error))?,
    };
    queue.check_root(root)?;
    let gate = queue
        .directory
        .create_new_file(OsStr::new(GATE))
        .map_err(|error| io_error("create HEAD admission gate", error))?;
    let name = Slot::at(0)?.name();
    let mut slot = queue
        .directory
        .create_new_file(OsStr::new(&name))
        .map_err(|error| io_error("create initial HEAD admission claimant", error))?;
    slot.write_all(&[0; TICKET_LEN])
        .map_err(|error| io_error("prepare initial HEAD admission claimant", error))?;
    queue.check_gate(&gate)?;
    queue.check_file(&name, &slot)?;
    queue.check_root(root)?;
    kernel_file(&gate, options)?;
    kernel_file(&slot, options)?;
    kernel_file(queue.directory.file(), options)?;
    queue.check_gate(&gate)?;
    queue.check_file(&name, &slot)?;
    queue.check_root(root)
}

fn private(metadata: &Metadata) -> Result<()> {
    Identity::metadata(metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(StoreError::InvalidPath(
                "HEAD admission entries must be private",
            ));
        }
    }
    Ok(())
}

fn try_exclusive(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(io_error("lock HEAD admission entry", error))
        }
    }
}

impl Queue {
    fn open(store: &Store, budget: &Budget, cancel: &CancellationToken) -> Result<Self> {
        budget.check(cancel)?;
        let directory = store
            .root
            .ensure_dir(OsStr::new(QUEUE_DIRECTORY))
            .map_err(|error| io_error("open HEAD admission directory", error))?;
        let queue = Self { directory };
        queue.check(store)?;
        budget.check(cancel)?;
        Ok(queue)
    }
    fn check(&self, store: &Store) -> Result<()> {
        self.check_root(&store.root)
    }
    fn check_root(&self, root: &Directory) -> Result<()> {
        check_directory_entry(root, OsStr::new(QUEUE_DIRECTORY), &self.directory)?;
        let metadata = self
            .directory
            .file()
            .metadata()
            .map_err(|error| io_error("read HEAD admission directory", error))?;
        private(&metadata)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let root = root
                .file()
                .metadata()
                .map_err(|error| io_error("read HEAD admission root", error))?;
            if root.dev() != metadata.dev() {
                return Err(StoreError::InvalidPath(
                    "HEAD admission directory differs from store device",
                ));
            }
        }
        Ok(())
    }
    fn check_file(&self, name: &str, file: &File) -> Result<Metadata> {
        check_file_entry(&self.directory, OsStr::new(name), file)?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error("read HEAD admission entry", error))?;
        private(&metadata)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(StoreError::Corrupt {
                    kind: "HEAD admission entry",
                    reason: "permanent file must have exactly one link",
                });
            }
        }
        Ok(metadata)
    }
    fn check_gate(&self, gate: &File) -> Result<()> {
        self.check_file(GATE, gate)?;
        if gate
            .metadata()
            .map_err(|error| io_error("read HEAD admission gate", error))?
            .len()
            != 0
        {
            return Err(StoreError::Corrupt {
                kind: "HEAD admission gate",
                reason: "permanent gate is not blank",
            });
        }
        Ok(())
    }
    fn gate(&self, budget: &Budget, cancel: &CancellationToken) -> Result<File> {
        budget.check(cancel)?;
        // Empty directories and a missing gate are resumable after creator death.
        // open_lock never truncates/replaces an existing permanent inode.
        let gate = self
            .directory
            .open_lock(OsStr::new(GATE))
            .map_err(|error| io_error("open HEAD admission gate", error))?;
        self.check_gate(&gate)?;
        loop {
            budget.check(cancel)?;
            if try_exclusive(&gate)? {
                self.check_gate(&gate)?;
                budget.check(cancel)?;
                return Ok(gate);
            }
            budget.pause(cancel)?;
        }
    }

    fn slots(&self, budget: &Budget, cancel: &CancellationToken) -> Result<[bool; MAX_CLAIMS]> {
        let mut slots = [false; MAX_CLAIMS];
        let entries = self
            .directory
            .entries()
            .map_err(|error| io_error("scan HEAD admission entries", error))?;
        for (count, entry) in entries.enumerate() {
            budget.check(cancel)?;
            if count > MAX_CLAIMS {
                return Err(StoreError::LimitExceeded("HEAD admission entries"));
            }
            let entry = entry.map_err(|error| io_error("read HEAD admission entry", error))?;
            if entry.name == OsStr::new(GATE) {
                continue;
            }
            let slot = entry
                .name
                .to_str()
                .and_then(|name| name.strip_prefix("slot-"))
                .and_then(|name| name.strip_suffix(".lock"))
                .and_then(|number| number.parse::<usize>().ok())
                .and_then(|index| Slot::at(index).ok())
                .filter(|slot| entry.name == OsStr::new(&slot.name()))
                .ok_or(StoreError::Corrupt {
                    kind: "HEAD admission directory",
                    reason: "unknown entry is retained",
                })?;
            slots[usize::from(slot.0)] = true;
        }
        Ok(slots)
    }

    // None requires retrying only after BOTH the gate and any unfinalized
    // reservation have dropped. Otherwise incomplete live records can form a
    // registration cycle. No probe lease escapes into predecessor bookkeeping.
    fn register(
        &self,
        store: &Store,
        budget: &Budget,
        cancel: &CancellationToken,
    ) -> Result<Option<Registration>> {
        let _gate = self.gate(budget, cancel)?;
        self.check(store)?;
        let slots = self.slots(budget, cancel)?;
        let mut reservation = None;
        let mut predecessors: Vec<Predecessor> = Vec::new();
        let mut maximum = 0_u64;
        for (index, present) in slots.iter().enumerate() {
            if !present {
                continue;
            }
            budget.check(cancel)?;
            let slot = Slot::at(index)?;
            let name = slot.name();
            let mut file = self
                .directory
                .open_existing_lock(OsStr::new(&name))
                .map_err(|error| io_error("open HEAD admission claimant", error))?;
            let metadata = self.check_file(&name, &file)?;
            if try_exclusive(&file)? {
                if reservation.is_none() {
                    // This is now our reservation, not an observer probe. Its
                    // old bytes cannot participate in max(live sequence).
                    reservation = Some(Reservation {
                        file,
                        slot,
                        length: metadata.len(),
                    });
                } else {
                    drop(file);
                }
                continue;
            }
            let Some(sequence) = read_ticket(&mut file, slot)? else {
                return Ok(None);
            };
            self.check_file(&name, &file)?;
            // An immediate observer probe can briefly lease an old record whose
            // cancelled higher number was reused. Snapshot it conservatively
            // even when another observed slot has that number.
            predecessors
                .try_reserve(1)
                .map_err(|_| StoreError::Allocation)?;
            predecessors.push(Predecessor { slot, sequence });
            maximum = maximum.max(sequence.get());
        }
        let mut reservation = match reservation {
            Some(reservation) => reservation,
            None => {
                let index = slots
                    .iter()
                    .position(|present| !present)
                    .ok_or(StoreError::LimitExceeded("head admission waiters"))?;
                let slot = Slot::at(index)?;
                let name = slot.name();
                let file = self
                    .directory
                    .open_lock(OsStr::new(&name))
                    .map_err(|error| io_error("create HEAD admission claimant", error))?;
                let metadata = self.check_file(&name, &file)?;
                if !try_exclusive(&file)? {
                    return Ok(None);
                }
                Reservation {
                    file,
                    slot,
                    length: metadata.len(),
                }
            }
        };
        let sequence = maximum
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(StoreError::LimitExceeded("HEAD admission sequence"))?;
        budget.check(cancel)?;
        let record = encode_ticket(reservation.slot, sequence);
        reservation
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_error("seek reserved HEAD ticket", error))?;
        reservation
            .file
            .write_all(&record)
            .map_err(|error| io_error("write reserved HEAD ticket", error))?;
        // Reusing an exact-size claimant needs only the complete record write.
        // Its length came from this pinned descriptor, and the readback below
        // still requires exact EOF even if uncooperative code changes the file.
        if reservation.length != TICKET_LEN as u64 {
            reservation
                .file
                .set_len(TICKET_LEN as u64)
                .map_err(|error| io_error("bound reserved HEAD ticket", error))?;
        }
        if read_ticket(&mut reservation.file, reservation.slot)? != Some(sequence) {
            return Err(StoreError::Corrupt {
                kind: "HEAD admission ticket",
                reason: "registered record differs from the reserved ticket",
            });
        }
        self.check_file(&reservation.slot.name(), &reservation.file)?;
        budget.check(cancel)?;
        Ok(Some(Registration {
            claim: Claim {
                file: reservation.file,
                slot: reservation.slot,
                sequence,
            },
            predecessors,
        }))
    }

    fn predecessor_done(&self, predecessor: Predecessor, own: NonZeroU64) -> Result<bool> {
        let name = predecessor.slot.name();
        // Always reopen: cloning a claimant's file description can share its
        // kernel lease with another request in the same process.
        let mut file = self
            .directory
            .open_existing_lock(OsStr::new(&name))
            .map_err(|error| io_error("probe older HEAD claimant", error))?;
        self.check_file(&name, &file)?;
        if try_exclusive(&file)? {
            drop(file); // No checks, hooks, waits or flushes while probing free.
            return Ok(true);
        }
        let sequence = read_ticket(&mut file, predecessor.slot)?;
        self.check_file(&name, &file)?;
        match sequence {
            None => Ok(false),
            Some(sequence) if sequence == predecessor.sequence => Ok(false),
            // Our still-live ticket prevents reset or reuse at/below our number.
            Some(sequence) if sequence > own => Ok(true),
            Some(_) => Err(StoreError::Corrupt {
                kind: "HEAD admission ticket",
                reason: "reused predecessor does not follow the waiting claimant",
            }),
        }
    }

    fn check_claim(&self, claim: &mut Claim) -> Result<()> {
        self.check_file(&claim.slot.name(), &claim.file)?;
        if read_ticket(&mut claim.file, claim.slot)? != Some(claim.sequence) {
            return Err(StoreError::Corrupt {
                kind: "HEAD admission ticket",
                reason: "leased claimant record changed",
            });
        }
        Ok(())
    }

    fn wait(
        &self,
        registration: &Registration,
        budget: &Budget,
        cancel: &CancellationToken,
    ) -> Result<()> {
        for predecessor in &registration.predecessors {
            loop {
                budget.check(cancel)?;
                if self.predecessor_done(*predecessor, registration.claim.sequence)? {
                    break;
                }
                budget.pause(cancel)?;
            }
        }
        budget.check(cancel)
    }
}

impl Store {
    pub(crate) fn acquire_head(
        &self,
        mode: HeadMode,
        cancel: &CancellationToken,
    ) -> Result<HeadLease> {
        let budget = Budget::new(self.options.lock_timeout);
        let queue = Queue::open(self, &budget, cancel)?;
        let mut registration = loop {
            budget.check(cancel)?;
            if let Some(registration) = queue.register(self, &budget, cancel)? {
                break registration;
            }
            budget.pause(cancel)?;
        };
        // The gate is released before invoking any development hook or waiting.
        #[cfg(feature = "fault-injection")]
        self.boundary(crate::DurableBoundary::HeadAdmissionRegistered)?;
        budget.check(cancel)?;
        queue.wait(&registration, &budget, cancel)?;
        queue.check_claim(&mut registration.claim)?;
        // Declared after the claimant so error paths close native HEAD first.
        let head = self
            .root
            .open_existing_lock(OsStr::new("head.lock"))
            .map_err(|error| io_error("open HEAD lock", error))?;
        check_file_entry(&self.root, OsStr::new("head.lock"), &head)?;
        loop {
            budget.check(cancel)?;
            let result = match mode {
                HeadMode::Shared => head.try_lock_shared(),
                HeadMode::Exclusive => head.try_lock(),
            };
            match result {
                Ok(()) => {
                    queue.check(self)?;
                    queue.check_claim(&mut registration.claim)?;
                    check_file_entry(&self.root, OsStr::new("head.lock"), &head)?;
                    budget.check(cancel)?;
                    return Ok(HeadLease {
                        _head: head,
                        _claim: registration.claim,
                        _directory: queue.directory,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) => budget.pause(cancel)?,
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(io_error("acquire HEAD lock", error));
                }
            }
        }
    }
}

fn encode_ticket(slot: Slot, sequence: NonZeroU64) -> [u8; TICKET_LEN] {
    let mut record = [0; TICKET_LEN];
    record[..8].copy_from_slice(TICKET_MAGIC);
    record[8..10].copy_from_slice(&slot.0.to_le_bytes());
    record[16..24].copy_from_slice(&sequence.get().to_le_bytes());
    let checksum = Sha256::digest(&record[..24]);
    record[24..].copy_from_slice(&checksum);
    record
}

// A held incomplete record cannot authorize numbering or predecessor completion.
// Its owner may be exiting after an interrupted registration write.
fn read_ticket(file: &mut File, expected: Slot) -> Result<Option<NonZeroU64>> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("seek HEAD admission ticket", error))?;
    let mut record = [0; TICKET_LEN];
    if let Err(error) = file.read_exact(&mut record) {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(io_error("read HEAD admission ticket", error))
        };
    }
    let mut extra = [0];
    if file
        .read(&mut extra)
        .map_err(|error| io_error("read HEAD admission ticket end", error))?
        != 0
    {
        return Ok(None);
    }
    if Sha256::digest(&record[..24])[..] != record[24..] {
        return Ok(None);
    }
    if &record[..8] != TICKET_MAGIC {
        return Err(StoreError::UnsupportedVersion {
            kind: "HEAD admission ticket",
        });
    }
    if record[8..10] != expected.0.to_le_bytes() || record[10..16] != [0; 6] {
        return Err(StoreError::Corrupt {
            kind: "HEAD admission ticket",
            reason: "slot or reserved bytes differ",
        });
    }
    let mut sequence = [0; 8];
    sequence.copy_from_slice(&record[16..24]);
    NonZeroU64::new(u64::from_le_bytes(sequence))
        .map(Some)
        .ok_or(StoreError::Corrupt {
            kind: "HEAD admission ticket",
            reason: "zero sequence",
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn setup() -> std::result::Result<(tempfile::TempDir, Store, Queue), Box<dyn std::error::Error>>
    {
        let fixture = tempfile::tempdir()?;
        let path = fixture.path().canonicalize()?.join("store");
        let store = Store::init(&path, &crate::StoreOptions::default())?;
        let queue = Queue::open(
            &store,
            &Budget::new(store.options.lock_timeout),
            &CancellationToken::new(),
        )?;
        Ok((fixture, store, queue))
    }

    fn register(queue: &Queue, store: &Store) -> Result<Registration> {
        queue
            .register(
                store,
                &Budget::new(store.options.lock_timeout),
                &CancellationToken::new(),
            )?
            .ok_or(StoreError::Corrupt {
                kind: "test registration",
                reason: "unexpected incomplete live claimant",
            })
    }

    fn replace_ticket(file: &mut File, slot: Slot, sequence: NonZeroU64) -> Result<()> {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| io_error("seek test ticket", error))?;
        file.write_all(&encode_ticket(slot, sequence))
            .map_err(|error| io_error("write test ticket", error))?;
        file.set_len(TICKET_LEN as u64)
            .map_err(|error| io_error("bound test ticket", error))
    }

    #[test]
    fn new_store_prepares_blank_coordination_before_the_first_acquisition() -> TestResult {
        let fixture = tempfile::tempdir()?;
        let path = fixture.path().canonicalize()?.join("store");
        let store = Store::init(&path, &StoreOptions::default())?;
        let queue = Queue {
            directory: store.root.open_dir(OsStr::new(QUEUE_DIRECTORY))?,
        };
        queue.check(&store)?;
        let gate = queue.directory.open_existing_lock(OsStr::new(GATE))?;
        queue.check_gate(&gate)?;
        let slot_name = Slot::at(0)?.name();
        let slot = queue.directory.open_existing_lock(OsStr::new(&slot_name))?;
        assert_eq!(
            queue.check_file(&slot_name, &slot)?.len(),
            TICKET_LEN as u64
        );
        assert_eq!(
            std::fs::read(path.join(QUEUE_DIRECTORY).join(&slot_name))?,
            [0; TICKET_LEN]
        );
        let slot_identity = Identity::file(&slot)?;
        let gate_identity = Identity::file(&gate)?;
        drop(slot);
        drop(gate);
        drop(store.begin(crate::HeadExpectation::Absent, &CancellationToken::new())?);
        let mut slot = queue.directory.open_existing_lock(OsStr::new(&slot_name))?;
        let gate = queue.directory.open_existing_lock(OsStr::new(GATE))?;
        assert_eq!(Identity::file(&slot)?, slot_identity);
        assert_eq!(Identity::file(&gate)?, gate_identity);
        assert_eq!(
            read_ticket(&mut slot, Slot::at(0)?)?.map(NonZeroU64::get),
            Some(1)
        );
        queue.check_gate(&gate)?;
        Ok(())
    }

    #[test]
    fn independent_same_process_descriptors_observe_live_and_released_claims() -> TestResult {
        let (_fixture, store, queue) = setup()?;
        let older = register(&queue, &store)?;
        let later = register(&queue, &store)?;
        assert_eq!(later.predecessors.len(), 1);
        assert!(!queue.predecessor_done(later.predecessors[0], later.claim.sequence)?);
        let probe = queue
            .directory
            .open_existing_lock(OsStr::new(&older.claim.slot.name()))?;
        assert!(matches!(
            probe.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(probe);
        drop(older);
        assert!(queue.predecessor_done(later.predecessors[0], later.claim.sequence)?);
        queue.wait(
            &later,
            &Budget::new(store.options.lock_timeout),
            &CancellationToken::new(),
        )?;
        Ok(())
    }

    #[test]
    fn cancelled_higher_ticket_reuse_with_a_transient_probe_has_no_cycle() -> TestResult {
        let (_fixture, store, queue) = setup()?;
        let first = register(&queue, &store)?;
        let second = register(&queue, &store)?;
        let third = register(&queue, &store)?;
        let waiting = register(&queue, &store)?;
        assert_eq!(waiting.claim.sequence.get(), 4);
        let reused_slot = third.claim.slot;
        drop(third);
        let cancelled = register(&queue, &store)?;
        assert_eq!(cancelled.claim.slot, reused_slot);
        assert_eq!(cancelled.claim.sequence.get(), 5);
        drop(cancelled);
        drop(first);
        let newer = register(&queue, &store)?;
        assert_eq!(newer.claim.sequence.get(), 5);
        assert_ne!(newer.claim.slot, reused_slot);
        drop(second);
        assert!(queue.predecessor_done(waiting.predecessors[0], waiting.claim.sequence)?);
        assert!(queue.predecessor_done(waiting.predecessors[1], waiting.claim.sequence)?);
        // Model the tiny interval between a successful observer probe and close.
        // A registration scan may conservatively see its obsolete valid record.
        let probe = queue
            .directory
            .open_existing_lock(OsStr::new(&reused_slot.name()))?;
        assert!(try_exclusive(&probe)?);
        let after_probe = register(&queue, &store)?;
        assert_eq!(after_probe.claim.sequence.get(), 6);
        assert_eq!(
            after_probe
                .predecessors
                .iter()
                .filter(|old| old.sequence.get() == 5)
                .count(),
            2
        );
        drop(probe);
        assert!(queue.predecessor_done(waiting.predecessors[2], waiting.claim.sequence)?);
        drop(waiting);
        queue.wait(
            &newer,
            &Budget::new(store.options.lock_timeout),
            &CancellationToken::new(),
        )?;
        drop(newer);
        let reused_again = register(&queue, &store)?;
        assert!(reused_again.claim.sequence > after_probe.claim.sequence);
        queue.wait(
            &after_probe,
            &Budget::new(store.options.lock_timeout),
            &CancellationToken::new(),
        )?;
        drop(after_probe);
        queue.wait(
            &reused_again,
            &Budget::new(store.options.lock_timeout),
            &CancellationToken::new(),
        )?;
        assert_eq!(
            queue
                .slots(
                    &Budget::new(store.options.lock_timeout),
                    &CancellationToken::new()
                )?
                .iter()
                .filter(|present| **present)
                .count(),
            4
        );
        Ok(())
    }

    #[test]
    fn incomplete_live_record_releases_registration_gate_and_reservation() -> TestResult {
        let (_fixture, store, queue) = setup()?;
        let first = register(&queue, &store)?;
        let incomplete = register(&queue, &store)?;
        let free_slot = first.claim.slot;
        drop(first);
        incomplete.claim.file.set_len(7)?;
        assert!(
            queue
                .register(
                    &store,
                    &Budget::new(store.options.lock_timeout),
                    &CancellationToken::new()
                )?
                .is_none()
        );
        for name in [GATE.to_string(), free_slot.name()] {
            let probe = queue.directory.open_existing_lock(OsStr::new(&name))?;
            assert!(
                try_exclusive(&probe)?,
                "stranded registration lease: {name}"
            );
            drop(probe);
        }
        drop(incomplete);
        let reclaimed = register(&queue, &store)?;
        assert_eq!(reclaimed.claim.sequence.get(), 1);
        assert_eq!(
            read_ticket(
                &mut queue
                    .directory
                    .open_existing_lock(OsStr::new(&reclaimed.claim.slot.name()))?,
                reclaimed.claim.slot
            )?,
            Some(reclaimed.claim.sequence)
        );
        Ok(())
    }

    #[test]
    fn reused_predecessor_must_follow_our_still_live_ticket() -> TestResult {
        let (_fixture, store, queue) = setup()?;
        let old = register(&queue, &store)?;
        let waiting = register(&queue, &store)?;
        let predecessor = waiting.predecessors[0];
        drop(old);
        let mut impostor = queue
            .directory
            .open_existing_lock(OsStr::new(&predecessor.slot.name()))?;
        assert!(try_exclusive(&impostor)?);
        replace_ticket(&mut impostor, predecessor.slot, waiting.claim.sequence)?;
        let error = queue
            .predecessor_done(predecessor, waiting.claim.sequence)
            .expect_err("equal reused ticket must be refused");
        assert!(
            matches!(
                error,
                StoreError::Corrupt {
                    kind: "HEAD admission ticket",
                    ..
                }
            ),
            "{error:?}"
        );
        let newer = NonZeroU64::new(waiting.claim.sequence.get() + 1).expect("small sequence");
        replace_ticket(&mut impostor, predecessor.slot, newer)?;
        assert!(queue.predecessor_done(predecessor, waiting.claim.sequence)?);
        Ok(())
    }

    #[test]
    fn sequence_overflow_never_numbers_below_a_live_claim() -> TestResult {
        let (_fixture, store, queue) = setup()?;
        let mut live = register(&queue, &store)?;
        let maximum = NonZeroU64::new(u64::MAX).expect("nonzero maximum");
        replace_ticket(&mut live.claim.file, live.claim.slot, maximum)?;
        let error = queue
            .register(
                &store,
                &Budget::new(store.options.lock_timeout),
                &CancellationToken::new(),
            )
            .err()
            .expect("live maximum must fail");
        assert!(
            matches!(error, StoreError::LimitExceeded("HEAD admission sequence")),
            "{error:?}"
        );
        drop(live);
        assert_eq!(register(&queue, &store)?.claim.sequence.get(), 1);
        Ok(())
    }

    #[test]
    fn all_live_slots_refuse_an_extra_claim_without_growing_state() -> TestResult {
        let (_fixture, store, queue) = setup()?;
        let _gate = queue.gate(
            &Budget::new(store.options.lock_timeout),
            &CancellationToken::new(),
        )?;
        let mut live = Vec::new();
        live.try_reserve_exact(MAX_CLAIMS)?;
        for index in 0..MAX_CLAIMS {
            let slot = Slot::at(index)?;
            let mut file = queue.directory.open_lock(OsStr::new(&slot.name()))?;
            assert!(try_exclusive(&file)?);
            replace_ticket(
                &mut file,
                slot,
                NonZeroU64::new(u64::try_from(index)? + 1).expect("positive index"),
            )?;
            live.push(file);
        }
        drop(_gate);
        let error = queue
            .register(
                &store,
                &Budget::new(store.options.lock_timeout),
                &CancellationToken::new(),
            )
            .err()
            .expect("full live queue must fail");
        assert!(
            matches!(error, StoreError::LimitExceeded("head admission waiters")),
            "{error:?}"
        );
        assert_eq!(
            queue
                .slots(
                    &Budget::new(store.options.lock_timeout),
                    &CancellationToken::new()
                )?
                .iter()
                .filter(|present| **present)
                .count(),
            MAX_CLAIMS
        );
        assert!(
            !queue
                .directory
                .entries()?
                .any(|entry| entry.is_ok_and(|entry| entry.name == "slot-1024.lock"))
        );
        drop(live);
        Ok(())
    }
}
