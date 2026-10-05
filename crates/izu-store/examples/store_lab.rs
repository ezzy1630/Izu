//! Development-only disposable-fixture driver. It cannot build in release mode.
use izu_model::{
    CancellationToken, ObjectKind, Operation, OperationId, RepositoryView, decode_metadata,
    encode_metadata,
};
use izu_store::{FaultHook, HeadExpectation, PublishOutcome, Store, StoreError, StoreOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args();
    let _program = args.next();
    let first = args
        .next()
        .ok_or("usage: store_lab init|append|blob|verify|recover PATH, or benchmark COUNT")?;
    let allow_volatile_for_tests = first == "--allow-volatile-for-tests";
    let action = if allow_volatile_for_tests {
        args.next().ok_or("missing action")?
    } else {
        first
    };
    let target = args.next().ok_or("missing path or benchmark count")?;
    if action == "benchmark" {
        if allow_volatile_for_tests {
            return Err("volatile fixture mode is not a persistence benchmark".into());
        }
        return benchmark(target.parse()?);
    }
    let options = if action == "append" {
        let label = args.next().ok_or("append needs description")?;
        let flag = args.next();
        let boundary = args.next();
        let mut options = fault_options(flag.as_deref(), boundary)?;
        options.allow_volatile_for_tests = allow_volatile_for_tests;
        let store = Store::open(Path::new(&target), &options)?;
        let cancel = CancellationToken::new();
        let tx = store.begin(HeadExpectation::Any, &cancel)?;
        let view = match tx.current_head() {
            Some(head) => {
                decode_metadata::<Operation>(
                    &store.get(head.object_id(), ObjectKind::Operation, &cancel)?,
                    &options.limits,
                )?
                .view
            }
            None => RepositoryView::default(),
        };
        let operation = put_operation(&store, tx.current_head(), view, &label)?;
        match tx.publish(operation, &cancel)? {
            PublishOutcome::Durable(receipt) => println!("DURABLE {}", receipt.current),
            PublishOutcome::VisibleButUncertain {
                receipt,
                error: StoreError::VolatileTestMode,
            } => {
                println!(
                    "VOLATILE_TEST_VISIBLE {} (persistent storage not acknowledged)",
                    receipt.current
                );
            }
            PublishOutcome::VisibleButUncertain { receipt, error } => {
                println!("VISIBLE_UNCERTAIN {} {error}", receipt.current);
                std::process::exit(2);
            }
        }
        return Ok(());
    } else {
        StoreOptions {
            allow_volatile_for_tests,
            ..StoreOptions::default()
        }
    };
    match action.as_str() {
        "init" => {
            Store::init(Path::new(&target), &options)?;
            println!(
                "{} {target}",
                if allow_volatile_for_tests {
                    "INITIALIZED_VOLATILE_TEST"
                } else {
                    "INITIALIZED"
                }
            );
        }
        "blob" => {
            let bytes: u64 = args.next().ok_or("blob needs byte count")?.parse()?;
            let store = Store::open(Path::new(&target), &options)?;
            let id = store.put_blob(
                &mut Synthetic {
                    remaining: bytes,
                    cursor: 0,
                },
                bytes,
                &CancellationToken::new(),
            )?;
            println!(
                "{} {id} {bytes}",
                if allow_volatile_for_tests {
                    "VOLATILE_TEST_BLOB"
                } else {
                    "DURABLE_BLOB"
                }
            );
        }
        "verify" => {
            println!(
                "{:?}",
                Store::open(Path::new(&target), &options)?.verify(&CancellationToken::new())?
            );
        }
        "recover" => {
            println!(
                "{:?}",
                Store::open(Path::new(&target), &options)?.recover(&CancellationToken::new())?
            );
        }
        _ => return Err("unknown action".into()),
    }
    Ok(())
}

fn fault_options(
    flag: Option<&str>,
    boundary: Option<String>,
) -> Result<StoreOptions, Box<dyn std::error::Error>> {
    let Some(flag) = flag else {
        return Ok(StoreOptions::default());
    };
    if !matches!(flag, "--hold-at" | "--enospc-at") {
        return Err("unknown fault flag".into());
    }
    let boundary = boundary.ok_or("missing durable boundary")?;
    let hold = flag == "--hold-at";
    Ok(StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |event| {
            if format!("{event:?}") == boundary {
                println!("BOUNDARY {event:?}");
                io::stdout().flush()?;
                if hold {
                    loop {
                        std::thread::park();
                    }
                }
                return Err(io::Error::from_raw_os_error(28));
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    })
}

fn put_operation(
    store: &Store,
    parent: Option<OperationId>,
    view: RepositoryView,
    label: &str,
) -> Result<OperationId, Box<dyn std::error::Error>> {
    let operation = Operation {
        parent,
        view,
        description: label.into(),
        created_at_unix_ms: 1,
    };
    let bytes = encode_metadata(&operation, &store.options().limits)?;
    Ok(OperationId::from_object(store.put(
        ObjectKind::Operation,
        &bytes,
        &CancellationToken::new(),
    )?))
}

fn benchmark(count: u64) -> Result<(), Box<dyn std::error::Error>> {
    if !(1..=10_000).contains(&count) {
        return Err("benchmark count must be 1..10000".into());
    }
    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("store");
    let store = Store::init(&path, &StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let build_start = Instant::now();
    let mut current = None;
    for index in 0..count {
        current = Some(put_operation(
            &store,
            current,
            RepositoryView::default(),
            &format!("prepared-{index}"),
        )?);
    }
    let prepared_ms = build_start.elapsed().as_secs_f64() * 1000.0;
    let current = current.ok_or("benchmark history missing")?;
    let bootstrap_start = Instant::now();
    match store.bootstrap_archive(current, &cancel)? {
        PublishOutcome::Durable(_) => {}
        PublishOutcome::VisibleButUncertain { error, .. } => return Err(error.into()),
    }
    let bootstrap_ms = bootstrap_start.elapsed().as_secs_f64() * 1000.0;
    let mut samples = [0.0; 3];
    let mut lock_acquire_ms = [0.0; 3];
    let mut stage_operation_ms = [0.0; 3];
    let mut publication_ms = [0.0; 3];
    let mut verify_after_publication_ms = [0.0; 3];
    for index in 0..3 {
        let begin = Instant::now();
        let tx = store.begin(HeadExpectation::Any, &cancel)?;
        lock_acquire_ms[index] = begin.elapsed().as_secs_f64() * 1000.0;
        let stage_start = Instant::now();
        let next = put_operation(
            &store,
            tx.current_head(),
            RepositoryView::default(),
            &format!("measured-{index}"),
        )?;
        stage_operation_ms[index] = stage_start.elapsed().as_secs_f64() * 1000.0;
        let publication_start = Instant::now();
        match tx.publish(next, &cancel)? {
            PublishOutcome::Durable(_) => {}
            PublishOutcome::VisibleButUncertain { error, .. } => return Err(error.into()),
        }
        publication_ms[index] = publication_start.elapsed().as_secs_f64() * 1000.0;
        samples[index] = begin.elapsed().as_secs_f64() * 1000.0;
        let verification_start = Instant::now();
        store.verify_reachable(next, &cancel)?;
        verify_after_publication_ms[index] = verification_start.elapsed().as_secs_f64() * 1000.0;
    }
    let report = store.verify_reachable(
        store
            .current_head(&cancel)?
            .ok_or("benchmark HEAD missing")?,
        &cancel,
    )?;
    println!(
        "metadata_only_store_baseline history={count} prepared_ms={prepared_ms:.3} bootstrap_ms={bootstrap_ms:.3} append_ms={samples:?} lock_acquire_ms={lock_acquire_ms:?} stage_operation_ms={stage_operation_ms:?} publication_ms={publication_ms:?} verify_after_publication_ms={verify_after_publication_ms:?} reachable_objects={} reachable_payload_bytes={}",
        report.object_count, report.payload_bytes
    );
    Ok(())
}

struct Synthetic {
    remaining: u64,
    cursor: u64,
}
impl Read for Synthetic {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = usize::try_from(self.remaining.min(output.len() as u64))
            .map_err(|_| io::Error::other("chunk length overflow"))?;
        for byte in &mut output[..count] {
            self.cursor = self.cursor.wrapping_add(1);
            *byte = self
                .cursor
                .wrapping_mul(6_364_136_223_846_793_005)
                .rotate_left(17) as u8;
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}
