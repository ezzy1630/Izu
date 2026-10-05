fn main() {
    if let Err(error) = izu_process::worker_main() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
