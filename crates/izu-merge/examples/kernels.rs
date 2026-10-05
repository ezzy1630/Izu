//! Focused kernel measurements; run with `cargo run -p izu-merge --release --example kernels`.
use izu_merge::{Options, diff, merge};
use std::{hint::black_box, time::Instant};
fn main() {
    let base = (0..20_000)
        .map(|i| format!("record {i:06} same text\n"))
        .collect::<String>()
        .into_bytes();
    let mut ours = base.clone();
    ours[40_000] = b'X';
    let mut theirs = base.clone();
    theirs[200_000] = b'Y';
    for (name, count) in [("identical", 100), ("one-edit", 100), ("two-sided", 100)] {
        let start = Instant::now();
        for _ in 0..count {
            match name {
                "identical" => {
                    black_box(
                        diff(black_box(&base), black_box(&base), &Options::default()).unwrap(),
                    );
                }
                "one-edit" => {
                    black_box(
                        diff(black_box(&base), black_box(&ours), &Options::default()).unwrap(),
                    );
                }
                _ => {
                    black_box(
                        merge(
                            Some(black_box(&base)),
                            Some(black_box(&ours)),
                            Some(black_box(&theirs)),
                            &Options::default(),
                        )
                        .unwrap(),
                    );
                }
            }
        }
        println!(
            "{name}: {} us/op ({} iterations, {} bytes/input)",
            start.elapsed().as_micros() / count,
            count,
            base.len()
        );
    }
    for name in ["add-20k-lines", "delete-20k-lines"] {
        let (a, b): (&[u8], &[u8]) = if name.starts_with("add") {
            (b"", &base)
        } else {
            (&base, b"")
        };
        let start = Instant::now();
        let count = 20;
        let mut successes = 0;
        let mut error = None;
        for _ in 0..count {
            match black_box(diff(black_box(a), black_box(b), &Options::default())) {
                Ok(patch) => {
                    black_box(patch);
                    successes += 1;
                }
                Err(reason) => error = Some(reason),
            }
        }
        println!(
            "{name}: {} us/op ({count} iterations, {successes} successes, error {error:?})",
            start.elapsed().as_micros() / count
        );
    }
    let a = b"a\n".repeat(10_000);
    let b = b"b\n".repeat(10_000);
    let mut opts = Options::default();
    opts.limits.max_edit_distance = 256;
    let start = Instant::now();
    let result = diff(&a, &b, &opts);
    println!(
        "repeated divergent: {:?} in {} us",
        result.unwrap_err(),
        start.elapsed().as_micros()
    );
}
