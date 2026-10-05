#![forbid(unsafe_code)]

use izu_api::{Api, ApiError, CancellationToken, Request, Response};
use izu_cli::{AgentCommand, Command, bounded_json_input, parse_args, write_human, write_json};
use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    let machine = args.iter().any(|arg| arg == "--json" || arg == "json");
    let cli = match parse_args(args) {
        Ok(cli) => cli,
        Err(error) => {
            if error.use_stderr() && machine {
                let response = Response::error(ApiError::invalid(error.to_string()));
                let _ = write_json(&mut io::stdout().lock(), &response);
                return ExitCode::from(response.exit_code());
            }
            let code = if error.use_stderr() { 2 } else { 0 };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    if matches!(
        &cli.command,
        Command::Agent {
            command: AgentCommand::Serve
        }
    ) {
        // Initialization and diagnostics must remain within the leased stdio
        // boundary. finish()/eprintln could block on the same failed transport.
        return match izu_cli::mcp::serve_stdio(Api::for_current_executable) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => ExitCode::from(error.exit_code()),
        };
    }
    let api = match Api::for_current_executable() {
        Ok(api) => api,
        Err(error) => return finish(Response::error(error), cli.json),
    };
    if let Command::RuntimeWorker { ticket } = &cli.command {
        return match api.worker_main(ticket) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(error.exit_code())
            }
        };
    }
    if matches!(&cli.command, Command::ProcessWorker { .. }) {
        return match api.process_worker_main() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(error.exit_code())
            }
        };
    }
    let json = cli.json || matches!(&cli.command, Command::Json);
    let cancel = CancellationToken::new();
    let _signals = match SignalGuard::new(cancel.clone()) {
        Ok(guard) => guard,
        Err(error) => return finish(Response::error(error), json),
    };
    let request = if matches!(&cli.command, Command::Json) {
        bounded_json_input(&mut io::stdin().lock())
    } else {
        cli.into_operation(&api, &cancel).map(Request::new)
    };
    let response = match request {
        Ok(request) => api.execute(request, &cancel),
        Err(error) => Response::error(error),
    };
    finish(response, json)
}

#[cfg(unix)]
struct SignalGuard {
    handle: signal_hook::iterator::Handle,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl SignalGuard {
    fn new(token: CancellationToken) -> Result<Self, ApiError> {
        use signal_hook::consts::signal::{SIGINT, SIGTERM};
        let mut signals =
            signal_hook::iterator::Signals::new([SIGINT, SIGTERM]).map_err(|error| {
                ApiError::invalid(format!(
                    "Cannot install process cancellation signals: {error}"
                ))
            })?;
        let handle = signals.handle();
        let thread = std::thread::Builder::new()
            .name("izu-cancellation".into())
            .spawn(move || {
                for _ in signals.forever() {
                    token.cancel();
                }
            })
            .map_err(|error| {
                ApiError::invalid(format!("Cannot start cancellation listener: {error}"))
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
}

#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(not(unix))]
struct SignalGuard;
#[cfg(not(unix))]
impl SignalGuard {
    fn new(_: CancellationToken) -> Result<Self, ApiError> {
        Ok(Self)
    }
}

fn finish(response: Response, json: bool) -> ExitCode {
    let code = response.exit_code();
    let result = if json {
        write_json(&mut io::stdout().lock(), &response)
    } else if code != 0 {
        write_human(&mut io::stderr().lock(), &response)
    } else {
        write_human(&mut io::stdout().lock(), &response)
    };
    match result {
        Ok(()) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
