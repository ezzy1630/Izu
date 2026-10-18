//! Bounded, owned-fixture profiling. No source-format or locking shortcuts.
use izu_engine::*;
use izu_store::{HeadExpectation, Store, StoreOptions};
use serde_json::json;
use std::{fs, path::PathBuf, time::Instant};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("provide a new owned fixture directory")?);
    let files: usize = args.next().as_deref().unwrap_or("2000").parse()?;
    let history: usize = args.next().as_deref().unwrap_or("50").parse()?;
    if files == 0 || files > 20_000 || history == 0 || history > 1000 {
        return Err("fixture exceeds bounds".into());
    }
    fs::create_dir(&root)?;
    let repository = Repository::init(&root, RepositoryOptions::default())?;
    let c = CancellationToken::new();
    for i in 0..files {
        let path = root.join(format!("group{}/file{i}", i / 100));
        fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
        fs::write(path, format!("original {i}\n"))?;
    }
    let identity = Identity {
        name: "Profile fixture".into(),
        email: "fixture@example.invalid".into(),
    };
    let mut commit = repository.commit(
        repository.workspace_id(),
        repository
            .workspace(repository.workspace_id(), &c)?
            .expected,
        Selection::All,
        "baseline".into(),
        identity.clone(),
        &c,
    )?;
    let base = commit.revision;
    for i in 0..history {
        fs::write(root.join("group0/file0"), format!("revision {i}\n"))?;
        commit = repository.commit(
            repository.workspace_id(),
            repository
                .workspace(repository.workspace_id(), &c)?
                .expected,
            Selection::Paths(vec![RepoPath::new("group0/file0")?]),
            "history".into(),
            identity.clone(),
            &c,
        )?;
    }
    let mut rows = Vec::new();
    for label in [
        "tree_decode",
        "revision_diff",
        "merge_base",
        "working_status",
    ] {
        let mut samples = Vec::new();
        for _ in 0..7 {
            let start = Instant::now();
            match label {
                "tree_decode" => {
                    std::hint::black_box(repository.tree(commit.tree, &c)?);
                }
                "revision_diff" => {
                    std::hint::black_box(repository.diff(base, commit.revision, &c)?);
                }
                "merge_base" => {
                    assert_eq!(
                        repository.merge_base(base, commit.revision, &c)?,
                        Some(base)
                    );
                }
                _ => {
                    assert!(
                        repository
                            .status(repository.workspace_id(), &c)?
                            .entries
                            .is_empty()
                    );
                }
            }
            samples.push(start.elapsed().as_secs_f64());
        }
        rows.push(json!({"operation":label,"seconds":samples}));
    }
    // Time real HEAD transaction admission under concurrent callers. This excludes
    // history publication, so it cannot be presented as commit throughput.
    let head = repository.current_operation(&c)?;
    for workers in [1, 4, 16] {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let stores = (0..workers)
            .map(|_| Store::open(repository.metadata_path(), &StoreOptions::default()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut threads = Vec::new();
        for store in stores {
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(
                move || -> std::result::Result<Vec<f64>, String> {
                    let c = CancellationToken::new();
                    barrier.wait();
                    let mut samples = Vec::new();
                    for _ in 0..20 {
                        let start = Instant::now();
                        let transaction = store
                            .begin(HeadExpectation::At(head), &c)
                            .map_err(|e| e.to_string())?;
                        samples.push(start.elapsed().as_secs_f64());
                        drop(transaction);
                    }
                    Ok(samples)
                },
            ));
        }
        let mut samples = Vec::new();
        for thread in threads {
            samples.extend(
                thread
                    .join()
                    .map_err(|_| "profile worker panicked")?
                    .map_err(std::io::Error::other)?,
            );
        }
        rows.push(
            json!({"operation":"head_transaction_admission","workers":workers,"seconds":samples}),
        );
    }
    assert_eq!(repository.current_operation(&c)?, head);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"files":files,"history":history,"rows":rows,
        "head_unchanged":true,"limits":"Warm observations; caches and host load uncontrolled. Admission timing excludes publication."}))?
    );
    Ok(())
}
