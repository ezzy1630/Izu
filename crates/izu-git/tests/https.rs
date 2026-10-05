//! Disposable loopback TLS exchanges with installed Git's actual CGI service.
//! No real endpoint, credential, GitHub repository or network publication.
use base64::Engine;
use izu_engine::{Repository, Selection};
use izu_git::{
    Error, GitAdapter, GitObjectId, GitSignature, GitSource, GitToolConfig, HttpsAuthentication,
    ImportOptions, NonFastForwardPolicy, PushRequest, RefImport, WorkerLauncher,
};
use izu_model::{CancellationToken, Identity, RefName};
use izu_process::{BaseEnvironment, CommandSpec, RunOptions};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TOKEN: &str = "owned-loopback-credential";
fn worker() -> WorkerLauncher {
    WorkerLauncher {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_izu-git-test-worker")),
        prefix_args: Vec::new(),
    }
}
fn cancel() -> CancellationToken {
    CancellationToken::new()
}
fn reference() -> RefName {
    RefName::new("main").expect("fixture ref")
}
fn git(root: &Path, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut command = Command::new("/usr/bin/git");
    command.current_dir(root).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(path) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", path);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Source Author")
        .env("GIT_AUTHOR_EMAIL", "source@example.test")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "Source Committer")
        .env("GIT_COMMITTER_EMAIL", "source@example.test")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args([
            "-c",
            "core.hooksPath=/nonexistent-izu-fixture",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("owned fixture Git");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("fixture input");
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().expect("fixture result");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn oid(root: &Path, name: &str) -> GitObjectId {
    String::from_utf8(git(root, &["rev-parse", name], None))
        .expect("ID")
        .trim()
        .parse()
        .expect("ID")
}
fn fixture_remote(root: &Path) -> PathBuf {
    let source = root.join("source");
    fs::create_dir(&source).expect("source");
    git(
        &source,
        &["init", "--template=", "--initial-branch=main"],
        None,
    );
    fs::create_dir(source.join("src")).expect("source dir");
    fs::write(source.join("src/message.txt"), b"original source\n").expect("source");
    git(&source, &["add", "."], None);
    git(&source, &["commit", "-m", "Original source"], None);
    let remote = root.join("remote.git");
    git(
        root,
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    git(&remote, &["config", "http.receivepack", "true"], None);
    remote
}
fn commit(repository: &Repository, previous: izu_model::RevisionId) {
    fs::write(
        repository.root_path().join("src/message.txt"),
        b"native source after HTTPS\n",
    )
    .expect("native source");
    let token = cancel();
    let workspace = repository
        .workspace(repository.workspace_id(), &token)
        .expect("workspace");
    let receipt = repository
        .commit(
            workspace.id,
            workspace.expected,
            Selection::All,
            "Native HTTPS source\n".into(),
            Identity {
                name: "Native Author".into(),
                email: "native@example.test".into(),
            },
            &token,
        )
        .expect("native commit");
    repository
        .update_ref(&reference(), Some(previous), Some(receipt.revision), &token)
        .expect("native publication");
}
fn request(url: &str, old: GitObjectId) -> PushRequest {
    PushRequest {
        remote: GitSource::https(url).expect("explicit URL"),
        native_ref: reference(),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(old),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: Some(GitSignature {
            name: "Native Committer".into(),
            email: "native-committer@example.test".into(),
            timestamp_unix_seconds: 1800000000,
            timezone_minutes: 0,
        }),
    }
}
#[derive(Clone, Debug)]
struct Exchange {
    method: String,
    path: String,
    authenticated: bool,
}
struct Server {
    url: String,
    certificate: Vec<u8>,
    mode: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
    errors: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    fn start(root: &Path) -> Self {
        let key = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()])
            .expect("owned TLS fixture");
        let certificate = key.cert.der().to_vec();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("fixture TLS")
            .with_no_client_auth()
            .with_single_cert(
                vec![key.cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.signing_key.serialize_der())),
            )
            .expect("fixture TLS identity");
        let tls = Arc::new(tls);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("owned loopback");
        listener.set_nonblocking(true).expect("fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let stop = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU8::new(0));
        let exchanges = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let root = fs::canonicalize(root).expect("fixture canonical path");
        let (thread_stop, thread_mode, thread_exchanges, thread_errors) = (
            stop.clone(),
            mode.clone(),
            exchanges.clone(),
            errors.clone(),
        );
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        if let Err(error) = serve(
                            socket,
                            &tls,
                            &root,
                            &thread_mode,
                            &thread_stop,
                            &thread_exchanges,
                        ) {
                            thread_errors.lock().expect("fixture errors").push(error);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => {
                        thread_errors
                            .lock()
                            .expect("fixture errors")
                            .push(error.to_string());
                        break;
                    }
                }
            }
        });
        Self {
            url: format!("https://{address}/remote.git"),
            certificate,
            mode,
            stop,
            exchanges,
            errors,
            thread: Some(thread),
        }
    }
    fn config(&self) -> GitToolConfig {
        GitToolConfig {
            executable: PathBuf::from("/usr/bin/git"),
            worker: Some(worker()),
            authentication: Some(
                HttpsAuthentication::basic(&self.url, "fixture-user", TOKEN.into())
                    .expect("scoped fixture token"),
            ),
            https_root_certificates: vec![self.certificate.clone()],
            ..GitToolConfig::default()
        }
    }
    fn adapter(&self) -> GitAdapter {
        GitAdapter::new(self.config()).expect("verified fixture adapter")
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("owned TLS server shutdown");
        }
    }
}
fn write_response(
    stream: &mut impl Write,
    status: u16,
    content_type: &str,
    headers: &str,
    body: &[u8],
) -> std::result::Result<(), String> {
    write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len()).map_err(|error| error.to_string())?;
    stream.write_all(body).map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}
fn serve(
    socket: TcpStream,
    tls: &Arc<rustls::ServerConfig>,
    root: &Path,
    mode: &AtomicU8,
    stop: &AtomicBool,
    exchanges: &Mutex<Vec<Exchange>>,
) -> std::result::Result<(), String> {
    socket
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let connection =
        rustls::ServerConnection::new(tls.clone()).map_err(|error| error.to_string())?;
    let mut stream = rustls::StreamOwned::new(connection, socket);
    let mut bytes = Vec::new();
    let mut block = [0_u8; 8192];
    let end = loop {
        if bytes.len() > 64 * 1024 {
            return Err("fixture request headers exceeded cap".into());
        }
        let count = stream.read(&mut block).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("fixture request ended early".into());
        }
        bytes.extend_from_slice(&block[..count]);
        if let Some(position) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let text = std::str::from_utf8(&bytes[..end]).map_err(|_| "fixture header encoding")?;
    let mut lines = text.split("\r\n");
    let request = lines.next().ok_or("fixture missing request")?;
    let mut first = request.split(' ');
    let method = first.next().ok_or("fixture method")?.to_owned();
    let path = first.next().ok_or("fixture path")?.to_owned();
    let mut length = 0_usize;
    let mut authorization = None;
    let mut content_type = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                length = value.trim().parse().map_err(|_| "fixture length")?;
            }
            if name.eq_ignore_ascii_case("Authorization") {
                authorization = Some(value.trim().to_owned());
            }
            if name.eq_ignore_ascii_case("Content-Type") {
                content_type = Some(value.trim().to_owned());
            }
        }
    }
    if length > 4 * 1024 * 1024 {
        return Err("fixture request body exceeds cap".into());
    }
    while bytes.len() - end < length {
        let count = stream.read(&mut block).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("fixture body ended early".into());
        }
        bytes.extend_from_slice(&block[..count]);
    }
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("fixture-user:{TOKEN}"))
    );
    let authenticated = authorization.as_deref() == Some(&expected);
    exchanges
        .lock()
        .map_err(|_| "fixture ledger")?
        .push(Exchange {
            method: method.clone(),
            path: path.clone(),
            authenticated,
        });
    if !authenticated || mode.load(Ordering::Acquire) == 3 {
        return write_response(
            &mut stream,
            401,
            "text/plain",
            "",
            format!("{TOKEN} {expected}").as_bytes(),
        );
    }
    if mode.load(Ordering::Acquire) == 1 {
        return write_response(
            &mut stream,
            302,
            "text/plain",
            "Location: https://127.0.0.1/forbidden\r\n",
            b"redirect",
        );
    }
    if mode.load(Ordering::Acquire) == 2 {
        let until = Instant::now() + Duration::from_secs(10);
        while !stop.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        return Ok(());
    }
    let (path_info, query) = path.split_once('?').unwrap_or((&path, ""));
    if !matches!(
        path_info,
        "/remote.git/info/refs" | "/remote.git/git-upload-pack" | "/remote.git/git-receive-pack"
    ) {
        return Err("fixture endpoint scope".into());
    }
    let mut command = CommandSpec::new("/usr/bin/git");
    command
        .arg("http-backend")
        .current_dir(root)
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REMOTE_USER", "fixture-user")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", length.to_string())
        .env("CONTENT_TYPE", content_type.unwrap_or_default())
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(path) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", path);
    }
    let options = RunOptions {
        stdin_limit: 4 * 1024 * 1024,
        stdout_limit: 4 * 1024 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(5),
        cleanup_timeout: Duration::from_secs(2),
        base_environment: BaseEnvironment::Clear,
        ..RunOptions::default()
    };
    let result = izu_process::run(
        &command,
        &bytes[end..end + length],
        &options,
        &worker(),
        &|| stop.load(Ordering::Acquire),
    )
    .map_err(|error| error.to_string())?;
    if !result.cleanup.cooperative_stop_succeeded()
        || !result.exit.is_some_and(|status| status.success())
        || !result.stdout.eof
    {
        return Err(format!("fixture CGI failed: {:?}", result.termination));
    }
    let output = &result.stdout.bytes;
    let split = output
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| {
            output
                .windows(2)
                .position(|value| value == b"\n\n")
                .map(|position| (position, 2))
        })
        .ok_or("fixture CGI header")?;
    let headers = std::str::from_utf8(&output[..split.0]).map_err(|_| "fixture CGI headers")?;
    let content_type = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("Content-Type"))
                .map(|(_, value)| value.trim())
        })
        .ok_or("fixture CGI content type")?;
    if mode.load(Ordering::Acquire) == 4 && path_info == "/remote.git/git-receive-pack" {
        let moved =
            fs::read_to_string(root.join("fixture-move-id")).map_err(|error| error.to_string())?;
        let moved: GitObjectId = moved
            .trim()
            .parse()
            .map_err(|error: Error| error.to_string())?;
        git(
            &root.join("remote.git"),
            &["update-ref", "refs/heads/main", &moved.to_string()],
            None,
        );
        return write_response(&mut stream, 200, content_type, "", b"000cgarbage\n0000");
    }
    write_response(
        &mut stream,
        200,
        content_type,
        "",
        &output[split.0 + split.1..],
    )
}

#[test]
fn authenticated_https_git_native_change_roundtrip() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    let remote = fixture_remote(fixture.path());
    let old = oid(&remote, "main");
    let server = Server::start(fixture.path());
    let adapter = server.adapter();
    let native = fixture.path().join("native");
    let (repository, imported) = adapter
        .clone_into(
            &native,
            &GitSource::https(&server.url).expect("URL"),
            &ImportOptions::default(),
            &cancel(),
        )
        .unwrap_or_else(|error| {
            let errors = server.errors.lock().expect("fixture diagnostics").clone();
            panic!("actual HTTPS clone: {error:?}; fixture errors: {errors:?}");
        });
    assert_eq!(
        fs::read(native.join("src/message.txt")).expect("materialized source"),
        b"original source\n"
    );
    commit(&repository, imported.revision_mapping[&old]);
    let pushed = adapter
        .push_ref(&repository, &request(&server.url, old), &cancel())
        .expect("actual HTTPS receive-pack publication");
    assert_eq!(oid(&remote, "main"), pushed.published);
    assert_eq!(oid(&remote, "main^"), old);
    assert_eq!(
        git(&remote, &["show", "main:src/message.txt"], None),
        b"native source after HTTPS\n"
    );
    let exchanges = server.exchanges.lock().expect("ledger");
    assert!(exchanges.iter().all(|exchange| exchange.authenticated));
    assert!(exchanges.iter().any(
        |exchange| exchange.method == "POST" && exchange.path == "/remote.git/git-upload-pack"
    ));
    assert!(exchanges.iter().any(
        |exchange| exchange.method == "POST" && exchange.path == "/remote.git/git-receive-pack"
    ));
    let errors = server.errors.lock().expect("fixture errors").clone();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(unix)]
#[test]
fn https_receive_policy_and_concurrent_external_target_movement_are_respected() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().expect("Neural fixture");
    let remote = fixture_remote(fixture.path());
    let old = oid(&remote, "main");
    let server = Server::start(fixture.path());
    let adapter = server.adapter();
    let (repository, imported) = adapter
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::https(&server.url).expect("URL"),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("clone");
    commit(&repository, imported.revision_mapping[&old]);
    let hook = remote.join("hooks/pre-receive");
    fs::write(&hook, b"#!/bin/sh\ncat >/dev/null\nexit 1\n").expect("owned policy");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).expect("fixture hook");
    assert!(matches!(
        adapter.push_ref(&repository, &request(&server.url, old), &cancel()),
        Err(Error::CommandFailed { .. })
    ));
    assert_eq!(oid(&remote, "main"), old);
    let tree = oid(&remote, "main^{tree}");
    let moved: GitObjectId = String::from_utf8(git(
        &remote,
        &["commit-tree", &tree.to_string(), "-p", &old.to_string()],
        Some(b"External HTTP movement\n"),
    ))
    .expect("ID")
    .trim()
    .parse()
    .expect("ID");
    fs::write(&hook, format!("#!/bin/sh\ncat >/dev/null\n/usr/bin/env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES /usr/bin/git --git-dir=. update-ref refs/heads/main {moved} {old}\n")).expect("owned race fixture");
    assert!(
        matches!(adapter.push_ref(&repository, &request(&server.url, old), &cancel()), Err(Error::LeaseMismatch { expected: Some(expected), observed: Some(observed) }) if expected == old && observed == moved)
    );
    assert_eq!(oid(&remote, "main"), moved);
}

#[test]
fn https_redirect_authentication_scope_and_remote_payload_limits_are_explicit() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    fixture_remote(fixture.path());
    let server = Server::start(fixture.path());
    let source = GitSource::https(&server.url).expect("URL");
    server.mode.store(1, Ordering::Release);
    let target = fixture.path().join("redirect-refused");
    assert!(
        server
            .adapter()
            .clone_into(&target, &source, &ImportOptions::default(), &cancel())
            .is_err()
    );
    assert!(!target.exists());
    assert_eq!(server.exchanges.lock().expect("ledger").len(), 1);
    server.mode.store(3, Ordering::Release);
    let error = server
        .adapter()
        .inventory(&source, &cancel())
        .expect_err("HTTP denies authentication");
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("fixture-user:{TOKEN}"));
    assert!(!error.to_string().contains(TOKEN));
    assert!(!error.to_string().contains(&encoded));
    server.mode.store(0, Ordering::Release);
    let mut config = server.config();
    config.limits.max_pack_bytes = 32;
    let bounded = GitAdapter::new(config).expect("bounded adapter");
    let target = fixture.path().join("pack-refused");
    assert!(matches!(
        bounded.clone_into(&target, &source, &ImportOptions::default(), &cancel()),
        Err(Error::Limit(_))
    ));
    assert!(!target.exists());
    let mut mismatched = server.config();
    mismatched.authentication = Some(
        HttpsAuthentication::basic(
            "https://example.test/different.git",
            "fixture",
            TOKEN.into(),
        )
        .expect("different scope"),
    );
    let before = server.exchanges.lock().expect("ledger").len();
    assert!(
        GitAdapter::new(mismatched)
            .expect("adapter")
            .inventory(&source, &cancel())
            .is_err()
    );
    assert_eq!(server.exchanges.lock().expect("ledger").len(), before);
}

#[test]
fn https_cancellation_drops_an_inflight_request_within_the_bounded_grace() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    fixture_remote(fixture.path());
    let server = Server::start(fixture.path());
    server.mode.store(2, Ordering::Release);
    let token = cancel();
    let signal = token.clone();
    let cancel_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        signal.cancel();
    });
    let started = Instant::now();
    let target = fixture.path().join("cancelled");
    assert!(matches!(
        server.adapter().clone_into(
            &target,
            &GitSource::https(&server.url).expect("URL"),
            &ImportOptions::default(),
            &token
        ),
        Err(Error::Cancelled)
    ));
    cancel_thread.join().expect("fixture cancellation");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!target.exists());
}

#[test]
fn malformed_receipt_after_real_publication_and_external_movement_remains_uncertain() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    let remote = fixture_remote(fixture.path());
    let old = oid(&remote, "main");
    let server = Server::start(fixture.path());
    let adapter = server.adapter();
    let (repository, imported) = adapter
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::https(&server.url).expect("URL"),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("clone");
    commit(&repository, imported.revision_mapping[&old]);
    let tree = oid(&remote, "main^{tree}");
    let moved: GitObjectId = String::from_utf8(git(
        &remote,
        &["commit-tree", &tree.to_string(), "-p", &old.to_string()],
        Some(b"External movement after HTTP publication\n"),
    ))
    .expect("ID")
    .trim()
    .parse()
    .expect("ID");
    fs::write(fixture.path().join("fixture-move-id"), moved.to_string())
        .expect("owned external publisher target");
    server.mode.store(4, Ordering::Release);
    let error = adapter
        .push_ref(&repository, &request(&server.url, old), &cancel())
        .expect_err("malformed acknowledgement");
    assert!(matches!(error, Error::PublicationUncertain(_)), "{error:?}");
    assert_eq!(oid(&remote, "main"), moved);
    assert!(error.to_string().contains("attempted"));
    assert!(error.to_string().contains(&moved.to_string()));
}

#[test]
fn https_explicit_branch_scope_reports_unrelated_tags_and_pull_refs() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    let remote = fixture_remote(fixture.path());
    let main = oid(&remote, "main");
    git(&remote, &["tag", "v1"], None);
    git(
        &remote,
        &["update-ref", "refs/pull/42/head", &main.to_string()],
        None,
    );
    let server = Server::start(fixture.path());
    let source = GitSource::https(&server.url).expect("URL");
    let adapter = server.adapter();
    let options = ImportOptions {
        refs: vec![RefImport {
            git_ref: "refs/heads/main".into(),
            native_ref: reference(),
            expected: None,
        }],
    };
    let (repository, imported) = adapter
        .clone_into(
            &fixture.path().join("selected"),
            &source,
            &options,
            &cancel(),
        )
        .expect("selected HTTPS branch");
    assert!(imported.inventory.unsupported.is_empty());
    assert_eq!(imported.inventory.omitted_refs.len(), 2);
    assert_eq!(imported.inventory.omitted_refs["refs/tags/v1"], main);
    assert_eq!(imported.inventory.omitted_refs["refs/pull/42/head"], main);
    assert_eq!(repository.view(&cancel()).expect("refs").refs.len(), 1);
    assert!(matches!(
        adapter.clone_into(
            &fixture.path().join("full-refused"),
            &source,
            &ImportOptions::default(),
            &cancel()
        ),
        Err(Error::Unsupported(_))
    ));
}
