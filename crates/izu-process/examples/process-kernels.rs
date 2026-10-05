use izu_process::{CommandSpec, RunOptions, Termination, WorkerLauncher, run};
use std::{path::PathBuf, time::Instant};
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let worker = WorkerLauncher {
        executable: PathBuf::from(&args[1]),
        prefix_args: vec![],
    };
    let opts = RunOptions {
        stderr_limit: 3 * 1024 * 1024,
        ..RunOptions::default()
    };
    for (mode, count, size) in [("echo", 20, 64 * 1024), ("flood", 5, 2 * 1024 * 1024)] {
        let input = vec![b'I'; size];
        let start = Instant::now();
        let mut command_ns = 0u128;
        for _ in 0..count {
            let mut command = CommandSpec::new(&args[2]);
            command.arg(mode);
            let result = run(&command, &input, &opts, &worker, &|| false).unwrap();
            assert_eq!(result.termination, Termination::Completed);
            assert!(result.cleanup.cooperative_stop_succeeded());
            command_ns += result.command_elapsed.unwrap().as_nanos();
        }
        println!(
            "{mode}: {} us/op full; {} us/op target (iterations {count}, stdin {size})",
            start.elapsed().as_micros() / count,
            command_ns / 1000 / count
        );
    }
}
