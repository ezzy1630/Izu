#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args_os().skip(1);
    let path = match arguments.next() {
        Some(path) if arguments.next().is_none() => std::path::PathBuf::from(path),
        _ => {
            eprintln!("usage: izu-runtime-worker ABSOLUTE_TICKET_PATH");
            return std::process::ExitCode::from(2);
        }
    };
    match izu_runtime::worker_main(&path) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(1)
        }
    }
}
