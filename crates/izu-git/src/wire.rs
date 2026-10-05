//! Original Smart HTTP v0 protocol adapter. All network bodies are bounded,
//! redirects are refused, and installed Git only receives an admitted pack.
use crate::tool::GitTool;
use crate::{Error, GitLimits, GitObjectId, Result, pack, validate_git_ref};
use izu_model::CancellationToken;
use reqwest::{Client, Method, header};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

const MAX_PACKET: usize = 65_520;
const UPLOAD: &str = "git-upload-pack";
const RECEIVE: &str = "git-receive-pack";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Advertisement {
    pub refs: BTreeMap<String, GitObjectId>,
    pub head: Option<String>,
    capabilities: BTreeSet<String>,
}
fn invalid(message: &str) -> Error {
    Error::InvalidSource(format!("Smart HTTP: {message}"))
}
fn http_error(tool: &GitTool, error: reqwest::Error) -> Error {
    if error.is_timeout() {
        return Error::TimedOut;
    }
    let error = error.without_url();
    let mut details = error.to_string();
    let mut source = std::error::Error::source(&error);
    for _ in 0..5 {
        let Some(current) = source else {
            break;
        };
        details.push_str(": ");
        details.extend(current.to_string().chars().take(512));
        source = current.source();
    }
    let details = tool.config.authentication.as_ref().map_or_else(
        || details.clone(),
        |authentication| authentication.redact(details.clone()),
    );
    invalid(&details)
}

fn packet<'a>(bytes: &'a [u8], position: &mut usize) -> Result<Option<&'a [u8]>> {
    let start = *position;
    let end = start
        .checked_add(4)
        .ok_or(Error::Limit("protocol cursor"))?;
    let prefix = bytes
        .get(start..end)
        .ok_or_else(|| invalid("truncated packet length"))?;
    let mut length = 0_usize;
    for value in prefix {
        let value = match value {
            b'0'..=b'9' => value - b'0',
            b'a'..=b'f' => value - b'a' + 10,
            b'A'..=b'F' => value - b'A' + 10,
            _ => return Err(invalid("nonhex packet length")),
        };
        length = length
            .checked_mul(16)
            .and_then(|length| length.checked_add(usize::from(value)))
            .ok_or(Error::Limit("protocol packet length"))?;
    }
    *position = end;
    if length == 0 {
        return Ok(None);
    }
    if !(4..=MAX_PACKET).contains(&length) {
        return Err(invalid("unsupported delimiter or packet size"));
    }
    let end = start
        .checked_add(length)
        .ok_or(Error::Limit("protocol cursor"))?;
    let value = bytes
        .get(*position..end)
        .ok_or_else(|| invalid("truncated packet"))?;
    *position = end;
    Ok(Some(value))
}
fn append_packet(output: &mut Vec<u8>, payload: &[u8]) -> Result<()> {
    let length = payload
        .len()
        .checked_add(4)
        .ok_or(Error::Limit("protocol packet"))?;
    if length > MAX_PACKET {
        return Err(Error::Limit("protocol packet"));
    }
    output
        .try_reserve(length)
        .map_err(|_| Error::Limit("protocol request allocation"))?;
    output.extend_from_slice(format!("{length:04x}").as_bytes());
    output.extend_from_slice(payload);
    Ok(())
}

fn advertisement(bytes: &[u8], service: &str, limits: &GitLimits) -> Result<Advertisement> {
    let mut position = 0;
    if packet(bytes, &mut position)? != Some(format!("# service={service}\n").as_bytes()) {
        return Err(invalid("missing service advertisement"));
    }
    if packet(bytes, &mut position)?.is_some() {
        return Err(invalid("service header has no flush"));
    }
    let mut refs = BTreeMap::new();
    let mut capabilities = BTreeSet::<String>::new();
    let mut first = true;
    while let Some(line) = packet(bytes, &mut position)? {
        if line.starts_with(b"ERR ") {
            return Err(invalid("server refused discovery"));
        }
        if line.starts_with(b"shallow ") {
            return Err(Error::Unsupported(vec![
                "shallow remote advertisement".into(),
            ]));
        }
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line =
            std::str::from_utf8(line).map_err(|_| invalid("non-UTF8 reference advertisement"))?;
        let value = if first {
            first = false;
            let (value, advertised) = line
                .split_once('\0')
                .ok_or_else(|| invalid("missing first-reference capabilities"))?;
            if advertised.len() > 64 * 1024 {
                return Err(Error::Limit("remote capabilities"));
            }
            for capability in advertised.split_ascii_whitespace() {
                if capability.chars().any(char::is_control) {
                    return Err(invalid("invalid capability"));
                }
                if capability.starts_with("object-format=") && capability != "object-format=sha1" {
                    return Err(Error::Unsupported(vec![capability.into()]));
                }
                capabilities.insert(capability.into());
            }
            value
        } else {
            if line.contains('\0') {
                return Err(invalid("duplicate capability delimiter"));
            }
            line
        };
        let (id, name) = value
            .split_once(' ')
            .ok_or_else(|| invalid("malformed reference advertisement"))?;
        if id == GitObjectId::zero_hex() && name == "capabilities^{}" {
            if !refs.is_empty() {
                return Err(invalid("empty advertisement mixed with refs"));
            }
            continue;
        }
        let id: GitObjectId = id.parse()?;
        if name == "HEAD" {
            continue;
        }
        if let Some(peeled) = name.strip_suffix("^{}") {
            validate_git_ref(peeled)?;
            continue;
        }
        validate_git_ref(name)?;
        if refs.len() >= limits.max_refs {
            return Err(Error::Limit("remote refs"));
        }
        if refs.insert(name.into(), id).is_some() {
            return Err(invalid("duplicate remote reference"));
        }
    }
    if position != bytes.len() {
        return Err(invalid("trailing advertisement data"));
    }
    let head = capabilities
        .iter()
        .find_map(|value| value.strip_prefix("symref=HEAD:"))
        .map(str::to_owned);
    if let Some(head) = &head {
        validate_git_ref(head)?;
    }
    Ok(Advertisement {
        refs,
        head,
        capabilities,
    })
}

fn client(tool: &GitTool) -> Result<Client> {
    let limits = &tool.config.limits;
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .https_only(true)
        .http1_only()
        .timeout(limits.command_timeout)
        .connect_timeout(limits.command_timeout.min(Duration::from_secs(10)))
        .read_timeout(limits.command_timeout.min(Duration::from_secs(10)))
        .pool_max_idle_per_host(0)
        .user_agent("izu-git/0.1");
    if tool.config.https_root_certificates.len() > 8 {
        return Err(Error::Limit("explicit HTTPS roots"));
    }
    for certificate in &tool.config.https_root_certificates {
        if certificate.len() > 64 * 1024 {
            return Err(Error::Limit("explicit HTTPS certificate"));
        }
        let certificate = reqwest::Certificate::from_der(certificate)
            .map_err(|_| invalid("invalid explicit DER trust anchor"))?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|_| invalid("could not initialize verified TLS"))
}

/// A dedicated current-thread reactor allows cancellation to drop an in-flight
/// request even when the synchronous adapter is called from another reactor.
fn exchange(
    tool: &GitTool,
    url: &str,
    service: &str,
    body: Option<Vec<u8>>,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let validated = crate::GitSource::https(url)?;
    let crate::GitSource::Https(url) = validated else {
        return Err(invalid("HTTPS URL required"));
    };
    let authorization = tool
        .config
        .authentication
        .as_ref()
        .map(|authentication| authentication.authorization(&url))
        .transpose()?;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(|source| Error::Io { action: "create bounded HTTP reactor", source })?;
            let result = runtime.block_on(async {
                let perform = async {
                    let client = client(tool)?;
                    let root = url.trim_end_matches('/');
                    let request_url = if body.is_some() { format!("{root}/{service}") } else { format!("{root}/info/refs?service={service}") };
                    let response_type = if body.is_some() { format!("application/x-{service}-result") } else { format!("application/x-{service}-advertisement") };
                    let method = if body.is_some() { Method::POST } else { Method::GET };
                    let mut request = client.request(method, request_url).header(header::ACCEPT, &response_type).header(header::ACCEPT_ENCODING, "identity");
                    if let Some(authorization) = authorization { request = request.header(header::AUTHORIZATION, authorization); }
                    if let Some(body) = body { request = request.header(header::CONTENT_TYPE, format!("application/x-{service}-request")).body(body); }
                    let mut response = request.send().await.map_err(|error| http_error(tool, error))?;
                    if response.status() != reqwest::StatusCode::OK { return Err(invalid(&format!("HTTP status {}", response.status().as_u16()))); }
                    let content_type = response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).and_then(|value| value.split(';').next());
                    if content_type != Some(response_type.as_str()) { return Err(Error::Unsupported(vec!["remote does not expose the expected Smart HTTP service".into()])); }
                    if response.headers().get(header::CONTENT_ENCODING).is_some_and(|value| value != "identity") { return Err(Error::Unsupported(vec!["HTTP content encoding".into()])); }
                    if response.content_length().is_some_and(|length| length > limit as u64) { return Err(Error::Limit("HTTP response body")); }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|error| http_error(tool, error))? {
                        if bytes.len().checked_add(chunk.len()).is_none_or(|length| length > limit) { return Err(Error::Limit("HTTP response body")); }
                        bytes.try_reserve(chunk.len()).map_err(|_| Error::Limit("HTTP response allocation"))?;
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(bytes)
                };
                tokio::select! {
                    value = perform => value,
                    _ = tokio::time::sleep(tool.config.limits.command_timeout) => Err(Error::TimedOut),
                    _ = async { loop { if cancel.is_cancelled() { break; } tokio::time::sleep(Duration::from_millis(25)).await; } } => Err(Error::Cancelled),
                }
            });
            // Dropped network futures release sockets; DNS worker completion
            // is a library/OS boundary, not an unbounded adapter join.
            runtime.shutdown_timeout(Duration::from_millis(100));
            result
        }).join().map_err(|_| invalid("HTTP reactor failed"))?
    })
}

fn discover(
    tool: &GitTool,
    url: &str,
    service: &str,
    cancel: &CancellationToken,
) -> Result<Advertisement> {
    let limit = tool
        .config
        .limits
        .max_refs
        .checked_mul(1200)
        .and_then(|length| length.checked_add(64 * 1024))
        .ok_or(Error::Limit("remote advertisement"))?;
    advertisement(
        &exchange(tool, url, service, None, limit, cancel)?,
        service,
        &tool.config.limits,
    )
}
pub(crate) fn refs(tool: &GitTool, url: &str, cancel: &CancellationToken) -> Result<Advertisement> {
    discover(tool, url, UPLOAD, cancel)
}
pub(crate) fn target_ref(
    tool: &GitTool,
    url: &str,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<GitObjectId>> {
    Ok(discover(tool, url, RECEIVE, cancel)?
        .refs
        .get(name)
        .copied())
}

fn upload_response(bytes: &[u8], tool: &GitTool) -> Result<Vec<u8>> {
    let mut position = 0;
    if packet(bytes, &mut position)? != Some(b"NAK\n") {
        return Err(invalid("unexpected upload acknowledgement"));
    }
    let mut pack = Vec::new();
    let mut progress = 0_usize;
    loop {
        if bytes
            .get(position..)
            .is_some_and(|bytes| bytes.starts_with(b"PACK"))
        {
            let value = &bytes[position..];
            if value.len() > tool.config.limits.max_pack_bytes {
                return Err(Error::Limit("compressed Git pack"));
            }
            pack.try_reserve(value.len())
                .map_err(|_| Error::Limit("pack response allocation"))?;
            pack.extend_from_slice(value);
            position = bytes.len();
            break;
        }
        let Some(value) = packet(bytes, &mut position)? else {
            break;
        };
        let Some((&channel, data)) = value.split_first() else {
            return Err(invalid("empty sideband packet"));
        };
        match channel {
            1 => {
                if pack
                    .len()
                    .checked_add(data.len())
                    .is_none_or(|length| length > tool.config.limits.max_pack_bytes)
                {
                    return Err(Error::Limit("compressed Git pack"));
                }
                pack.try_reserve(data.len())
                    .map_err(|_| Error::Limit("pack response allocation"))?;
                pack.extend_from_slice(data);
            }
            2 => {
                progress = progress
                    .checked_add(data.len())
                    .ok_or(Error::Limit("server progress"))?;
                if progress > tool.config.limits.max_stderr_bytes {
                    return Err(Error::Limit("server progress"));
                }
            }
            3 => return Err(invalid("server reported fatal upload failure")),
            _ => return Err(invalid("unknown sideband channel")),
        }
        if position == bytes.len() {
            break;
        }
    }
    if position != bytes.len() || !pack.starts_with(b"PACK") {
        return Err(invalid("incomplete upload response"));
    }
    Ok(pack)
}

pub(crate) fn fetch(
    tool: &GitTool,
    url: &str,
    cache: &Path,
    advertised: &Advertisement,
    wants: &[GitObjectId],
    cancel: &CancellationToken,
) -> Result<()> {
    if wants.is_empty() {
        return Ok(());
    }
    let mut body = Vec::new();
    let mut capabilities = Vec::new();
    for capability in ["side-band-64k", "ofs-delta", "no-progress"] {
        if advertised.capabilities.contains(capability) {
            capabilities.push(capability);
        }
    }
    let mut seen = BTreeSet::new();
    let allowed: BTreeSet<_> = advertised.refs.values().copied().collect();
    for id in wants {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !allowed.contains(id) {
            return Err(invalid("requested object is not advertised"));
        }
        if !seen.insert(*id) {
            continue;
        }
        let first = seen.len() == 1;
        let line = if first && !capabilities.is_empty() {
            format!("want {id} {}\n", capabilities.join(" "))
        } else {
            format!("want {id}\n")
        };
        append_packet(&mut body, line.as_bytes())?;
    }
    body.extend_from_slice(b"0000");
    append_packet(&mut body, b"done\n")?;
    let limit = tool
        .config
        .limits
        .max_pack_bytes
        .checked_mul(2)
        .and_then(|length| length.checked_add(tool.config.limits.max_stderr_bytes))
        .and_then(|length| length.checked_add(64 * 1024))
        .ok_or(Error::Limit("pack transfer body"))?;
    let response = exchange(tool, url, UPLOAD, Some(body), limit, cancel)?;
    let pack = upload_response(&response, tool)?;
    drop(response);
    pack::validate(&pack, &tool.config.limits, cancel)?;
    let max = format!("--max-input-size={}", tool.config.limits.max_pack_bytes);
    tool.run_pack(
        cache,
        "index admitted HTTP pack",
        &GitTool::args(&[
            "-c",
            "pack.writeReverseIndex=false",
            "index-pack",
            "--stdin",
            "--strict",
            "--threads=1",
            &max,
        ]),
        &pack,
        256,
        cancel,
    )?;
    Ok(())
}

pub(crate) fn fetch_lease(
    tool: &GitTool,
    url: &str,
    cache: &Path,
    name: &str,
    expected: GitObjectId,
    cancel: &CancellationToken,
) -> Result<()> {
    let advertised = refs(tool, url, cancel)?;
    let observed = advertised.refs.get(name).copied();
    if observed != Some(expected) {
        return Err(Error::LeaseMismatch {
            expected: Some(expected),
            observed,
        });
    }
    fetch(tool, url, cache, &advertised, &[expected], cancel)
}

/// A negative report-status is a definite rejection. A missing or malformed
/// receipt remains uncertain after the request may have reached receive-pack.
pub(crate) fn push(
    tool: &GitTool,
    url: &str,
    cache: &Path,
    name: &str,
    expected: Option<GitObjectId>,
    new: GitObjectId,
    cancel: &CancellationToken,
) -> std::result::Result<(), (Box<Error>, bool)> {
    let prepare = || -> Result<Vec<u8>> {
        let advertised = discover(tool, url, RECEIVE, cancel)?;
        let observed = advertised.refs.get(name).copied();
        if observed != expected {
            return Err(Error::LeaseMismatch { expected, observed });
        }
        if !advertised.capabilities.contains("report-status") {
            return Err(Error::Unsupported(vec![
                "receive-pack report-status is required".into(),
            ]));
        }
        let input = format!("{new}\n");
        let pack = tool
            .run_pack(
                cache,
                "prepare bounded publication pack",
                &GitTool::args(&[
                    "-c",
                    "pack.windowMemory=8m",
                    "pack-objects",
                    "--stdout",
                    "--revs",
                    "--threads=1",
                    "--window=0",
                    "--depth=0",
                ]),
                input.as_bytes(),
                tool.config.limits.max_pack_bytes,
                cancel,
            )?
            .stdout;
        pack::validate(&pack, &tool.config.limits, cancel)?;
        let mut body = Vec::new();
        let old = expected
            .map(|id| id.to_string())
            .unwrap_or_else(|| GitObjectId::zero_hex().into());
        append_packet(
            &mut body,
            format!("{old} {new} {name}\0report-status\n").as_bytes(),
        )?;
        body.extend_from_slice(b"0000");
        body.try_reserve(pack.len())
            .map_err(|_| Error::Limit("publication request allocation"))?;
        body.extend_from_slice(&pack);
        Ok(body)
    };
    let body = prepare().map_err(|error| (Box::new(error), true))?;
    let limit = tool
        .config
        .limits
        .max_stderr_bytes
        .checked_add(64 * 1024)
        .ok_or_else(|| (Box::new(Error::Limit("publication receipt")), true))?;
    let response = exchange(tool, url, RECEIVE, Some(body), limit, cancel)
        .map_err(|error| (Box::new(error), false))?;
    let parsed = (|| -> Result<Option<String>> {
        let mut position = 0;
        let unpack =
            packet(&response, &mut position)?.ok_or_else(|| invalid("missing unpack receipt"))?;
        let unpack = std::str::from_utf8(unpack)
            .map_err(|_| invalid("non-UTF8 unpack receipt"))?
            .strip_suffix('\n')
            .ok_or_else(|| invalid("unterminated unpack receipt"))?;
        let unpack = unpack
            .strip_prefix("unpack ")
            .filter(|message| !message.is_empty() && !message.chars().any(char::is_control))
            .ok_or_else(|| invalid("malformed unpack receipt"))?;
        let line =
            packet(&response, &mut position)?.ok_or_else(|| invalid("missing ref receipt"))?;
        let line = std::str::from_utf8(line)
            .map_err(|_| invalid("non-UTF8 ref receipt"))?
            .strip_suffix('\n')
            .ok_or_else(|| invalid("unterminated ref receipt"))?;
        let rejected = if line == format!("ok {name}") {
            if unpack != "ok" {
                return Err(invalid("contradictory unpack and ref receipt"));
            }
            None
        } else if let Some(reason) = line.strip_prefix(&format!("ng {name} ")) {
            if reason.is_empty() || reason.chars().any(char::is_control) {
                return Err(invalid("malformed negative ref receipt"));
            }
            Some(if unpack == "ok" {
                reason.into()
            } else {
                format!("unpack {unpack}; {reason}")
            })
        } else {
            return Err(invalid("unexpected publication receipt"));
        };
        if packet(&response, &mut position)?.is_some() || position != response.len() {
            return Err(invalid("trailing publication receipt"));
        }
        Ok(rejected)
    })()
    .map_err(|error| (Box::new(error), false))?;
    if let Some(reason) = parsed {
        let reason = tool.config.authentication.as_ref().map_or_else(
            || reason.clone(),
            |authentication| authentication.redact(reason.clone()),
        );
        return Err((
            Box::new(Error::CommandFailed {
                operation: "remote receive-pack rejection".into(),
                code: None,
                stderr: reason,
            }),
            true,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_packets_and_unknown_object_formats_are_refused() {
        for bytes in [b"zzzz".as_slice(), b"0001", b"ffffx", b"0007a", b"0003"] {
            assert!(packet(bytes, &mut 0).is_err());
        }
        let mut response = Vec::new();
        append_packet(&mut response, b"# service=git-upload-pack\n").expect("fixture");
        response.extend_from_slice(b"0000");
        append_packet(
            &mut response,
            b"1111111111111111111111111111111111111111 refs/heads/main\0object-format=sha256\n",
        )
        .expect("fixture");
        response.extend_from_slice(b"0000");
        assert!(matches!(
            advertisement(&response, UPLOAD, &GitLimits::default()),
            Err(Error::Unsupported(_))
        ));
    }
}
