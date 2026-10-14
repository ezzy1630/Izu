//! Owned disposable process fixture; no shell input is accepted.
// Exiting before descendants is the condition this fixture must reproduce.
#![allow(clippy::zombie_processes)]
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
    thread,
    time::Duration,
};
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    match args.get(1).and_then(|s| s.to_str()).unwrap_or("") {
        "__process-worker" => {
            izu_process::worker_main().unwrap();
        }
        "partial-auth-worker" => {
            let endpoint = args
                .last()
                .unwrap()
                .to_str()
                .unwrap()
                .strip_prefix("tcp:")
                .unwrap();
            let port: u16 = endpoint.parse().unwrap();
            let mut stream =
                std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
            stream.write_all(b"?").unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        "worker-with-untrusted-connections" => {
            let endpoint = args
                .last()
                .unwrap()
                .to_str()
                .unwrap()
                .strip_prefix("tcp:")
                .unwrap();
            let port: u16 = endpoint.parse().unwrap();
            let mut peers = Vec::new();
            for index in 0..12 {
                let mut stream =
                    std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
                if index % 2 == 0 {
                    stream.write_all(b"wrong authentication").unwrap();
                }
                peers.push(stream);
            }
            izu_process::worker_main().unwrap();
            drop(peers);
        }
        "wrong-worker" => {
            let endpoint = &args[args.len() - 1];
            if let Some(port) = endpoint
                .to_str()
                .and_then(|value| value.strip_prefix("tcp:"))
            {
                let mut nonce = [0; 32];
                std::fs::File::from(rustix::io::fcntl_dupfd_cloexec(io::stdin(), 3).unwrap())
                    .read_exact(&mut nonce)
                    .unwrap();
                let mut channel = std::net::TcpStream::connect((
                    std::net::Ipv4Addr::LOCALHOST,
                    port.parse::<u16>().unwrap(),
                ))
                .unwrap();
                channel.write_all(&nonce).unwrap();
                channel.write_all(b"IZUP\x01").unwrap();
                channel
                    .write_all(&std::process::id().to_le_bytes())
                    .unwrap();
                channel.write_all(b"Z").unwrap(); // Authenticated but invalid worker status.
                thread::sleep(Duration::from_secs(30));
            } else {
                let mut channel = std::os::unix::net::UnixStream::connect(endpoint).unwrap();
                channel.write_all(b"IZUP\x02").unwrap();
                channel
                    .write_all(&std::process::id().to_le_bytes())
                    .unwrap();
                thread::sleep(Duration::from_secs(30));
            }
        }
        "echo" => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes).unwrap();
            io::stdout().write_all(&bytes).unwrap();
            io::stderr().write_all(b"diagnostic").unwrap();
        }
        "state" => {
            io::stdout()
                .write_all(
                    std::env::current_dir()
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes(),
                )
                .unwrap();
            io::stderr()
                .write_all(
                    std::env::var_os("PATH")
                        .unwrap_or_default()
                        .as_encoded_bytes(),
                )
                .unwrap();
        }
        "heartbeat" | "heartbeat-hold" => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&args[2])
                .unwrap();
            let duration = if args[1] == "heartbeat-hold" { 30 } else { 2 };
            let deadline = std::time::Instant::now() + Duration::from_secs(duration);
            while std::time::Instant::now() < deadline {
                file.write_all(b"tick\n").unwrap();
                file.flush().unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        }
        "orphan-runner" => {
            let mut spec = izu_process::CommandSpec::new(std::env::current_exe().unwrap());
            spec.arg("heartbeat").arg(&args[3]);
            let launcher = izu_process::WorkerLauncher {
                executable: args[2].clone().into(),
                prefix_args: vec![],
            };
            let _ = izu_process::run(
                &spec,
                b"",
                &izu_process::RunOptions::default(),
                &launcher,
                &|| false,
            )
            .unwrap();
        }
        "flood" => {
            let out = thread::spawn(|| {
                io::stdout()
                    .write_all(&vec![b'O'; 2 * 1024 * 1024])
                    .unwrap();
            });
            let err = thread::spawn(|| {
                io::stderr()
                    .write_all(&vec![b'E'; 2 * 1024 * 1024])
                    .unwrap();
            });
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input).unwrap();
            assert_eq!(input.len(), 2 * 1024 * 1024);
            out.join().unwrap();
            err.join().unwrap();
        }
        "hold" => {
            thread::sleep(Duration::from_secs(30));
        }
        "drop-full-stderr" => {
            let owner = (0..64)
                .find_map(|_| {
                    let mut command = Command::new(std::env::current_exe().unwrap());
                    command
                        .arg("hold")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                    let mut owner = izu_process::OwnedProcess::spawn_until(
                        &mut command,
                        std::time::Instant::now() + Duration::from_secs(5),
                        &|| false,
                    )
                    .unwrap();
                    (!owner.stop(Duration::ZERO).cooperative_stop_succeeded()).then_some(owner)
                })
                .expect("fixture must obtain an unconfirmed zero-budget cleanup");
            let stderr = io::stderr();
            let flags = rustix::fs::fcntl_getfl(&stderr).unwrap();
            izu_process::set_nonblocking(&stderr).unwrap();
            let bytes = [b'E'; 4096];
            loop {
                match rustix::io::write(&stderr, &bytes) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::AGAIN) => break,
                    Err(error) => panic!("fill stderr: {error}"),
                }
            }
            rustix::fs::fcntl_setfl(&stderr, flags).unwrap();
            io::stdout().write_all(b"ready\n").unwrap();
            drop(owner);
            io::stdout().write_all(b"dropped\n").unwrap();
        }
        "self-kill-worker" => {
            assert_eq!(
                rustix::process::getpgid(None).unwrap(),
                rustix::process::getpid()
            );
            rustix::process::kill_current_process_group(rustix::process::Signal::KILL).unwrap();
        }
        "background" => {
            let child = Command::new(std::env::current_exe().unwrap())
                .arg("hold")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            println!("retained {}", child.id());
        }
        "background-writer" => {
            let child = Command::new(std::env::current_exe().unwrap())
                .arg("heartbeat")
                .arg(&args[2])
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !std::path::Path::new(&args[2]).exists() {
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            println!("writer {}", child.id());
        }
        "ignore-term" => {
            let child = Command::new("/bin/sh")
                .arg("-c")
                .arg("trap '' TERM; exec /bin/sleep 30")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            println!("retained {}", child.id());
        }
        "escape-child" => {
            rustix::process::setsid().unwrap();
            println!("escaped");
            io::stdout().flush().unwrap();
            thread::sleep(Duration::from_secs(1));
        }
        "escape" => {
            let _child = Command::new(std::env::current_exe().unwrap())
                .arg("escape-child")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            thread::sleep(Duration::from_millis(30));
        }
        "interactive" => {
            let mut byte = [0u8; 1];
            io::stdin().read_exact(&mut byte).unwrap();
            println!("response");
            io::stdout().flush().unwrap();
            let mut tail = Vec::new();
            io::stdin().read_to_end(&mut tail).unwrap();
            println!("closed");
        }
        "argv" => {
            for arg in &args[2..] {
                use std::os::unix::ffi::OsStrExt;
                io::stdout().write_all(arg.as_bytes()).unwrap();
                io::stdout().write_all(b"\0").unwrap();
            }
            io::stderr()
                .write_all(
                    std::env::var_os("EXACT_ENV")
                        .unwrap_or_default()
                        .as_encoded_bytes(),
                )
                .unwrap();
        }
        "exit7" => {
            println!("normal");
            std::process::exit(7);
        }
        _ => std::process::exit(2),
    }
}
