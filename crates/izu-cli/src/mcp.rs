//! Bounded MCP stdio, with both current stateless and legacy handshake protocols.
//! Each named tool is a typed operation; it cannot execute a generic shell string.

use izu_api::{
    Api, ApiError, BuildIdentity, CancellationToken, MAX_REQUEST_BYTES, MAX_RESULT_BYTES,
    SCHEMA_VERSION, bounded_json, parse_request, request_schema,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

#[cfg(unix)]
mod stdio;
#[cfg(unix)]
pub use stdio::serve_stdio;

pub const MODERN_VERSION: &str = "2026-07-28";
pub const LEGACY_VERSION: &str = "2025-11-25";
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING: usize = 8;
const MAX_LOCAL_REPLIES: usize = 8;
const MAX_TRACKED: usize = MAX_PENDING + MAX_LOCAL_REPLIES + 3;

#[derive(Default)]
enum LegacyState {
    #[default]
    New,
    Initializing,
    Ready,
}

#[derive(Default)]
struct Session {
    legacy: LegacyState,
}

struct Queued {
    message: Value,
    token: CancellationToken,
    key: Option<String>,
    suppress: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Inflight {
    token: CancellationToken,
    suppress: Arc<AtomicBool>,
}

#[derive(Default)]
struct Registry(Mutex<BTreeMap<String, Inflight>>);

impl Registry {
    fn cancel_all(&self) -> Result<(), ApiError> {
        for request in self.lock()?.values() {
            // EOF cancels execution, not receipt delivery.
            request.token.cancel();
        }
        Ok(())
    }

    fn finish(&self, key: Option<&str>) -> Result<(), ApiError> {
        if let Some(key) = key {
            self.lock()?.remove(key);
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Inflight>>, ApiError> {
        self.0
            .lock()
            .map_err(|_| transport_error("MCP request registry is unavailable"))
    }
}

struct Outbound {
    bytes: Vec<u8>,
    key: Option<String>,
    suppress: Option<Arc<AtomicBool>>,
}

impl Outbound {
    fn response(
        value: Value,
        key: Option<String>,
        suppress: Option<Arc<AtomicBool>>,
    ) -> Result<Self, ApiError> {
        Ok(Self {
            bytes: encode_response(&value)?,
            key,
            suppress,
        })
    }

    fn suppressed(&self) -> bool {
        self.suppress
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}

enum Admission {
    Execute(Queued),
    Reply(Outbound),
    Ignore,
}

/// The decoder is shared by buffered library IO and the readiness-driven CLI.
/// Oversized input is discarded incrementally until its delimiter, never saved.
#[derive(Default)]
struct Framer {
    frame: Vec<u8>,
    oversized: bool,
}

impl Framer {
    fn push(&mut self, input: &[u8]) -> (usize, Option<Result<Vec<u8>, FrameError>>) {
        let newline = input.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(input.len());
        if self
            .frame
            .len()
            .checked_add(length)
            .is_none_or(|size| size > MAX_REQUEST_BYTES)
        {
            self.oversized = true;
            self.frame.clear();
        }
        if !self.oversized {
            self.frame.extend_from_slice(&input[..length]);
        }
        let consumed = length + usize::from(newline.is_some());
        let completed = newline.map(|_| self.finish_frame());
        (consumed, completed)
    }

    fn finish_frame(&mut self) -> Result<Vec<u8>, FrameError> {
        if std::mem::take(&mut self.oversized) {
            self.frame.clear();
            Err(FrameError::Oversized)
        } else {
            Ok(std::mem::take(&mut self.frame))
        }
    }

    fn eof(&mut self) -> Option<Result<Vec<u8>, FrameError>> {
        (self.oversized || !self.frame.is_empty()).then(|| self.finish_frame())
    }
}

#[derive(Debug)]
pub enum FrameError {
    Oversized,
    Io(std::io::Error),
}

/// Reads one newline-delimited frame without allowing the client to choose an
/// allocation size. An oversized frame is drained to its delimiter without
/// accumulating it, so the following frame remains independently parseable.
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, FrameError> {
    let mut framer = Framer::default();
    loop {
        let available = reader.fill_buf().map_err(FrameError::Io)?;
        if available.is_empty() {
            return framer.eof().transpose();
        }
        let (consumed, completed) = framer.push(available);
        reader.consume(consumed);
        if let Some(completed) = completed {
            return completed.map(Some);
        }
    }
}

fn admit(frame: Result<Vec<u8>, FrameError>, registry: &Registry) -> Result<Admission, ApiError> {
    let bytes = match frame {
        Ok(bytes) => bytes,
        Err(FrameError::Oversized) => {
            return Ok(Admission::Reply(Outbound::response(
                rpc_error(
                    Value::Null,
                    -32600,
                    format!("MCP frame exceeds {MAX_REQUEST_BYTES} bytes"),
                    None,
                ),
                None,
                None,
            )?));
        }
        Err(FrameError::Io(error)) => {
            return Err(transport_error(format!("Cannot read MCP input: {error}")));
        }
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(message) => message,
        Err(error) => {
            return Ok(Admission::Reply(Outbound::response(
                rpc_error(Value::Null, -32700, format!("Invalid JSON: {error}"), None),
                None,
                None,
            )?));
        }
    };
    if message.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && message.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
        && message.get("id").is_none()
    {
        if let Some(id) = message.pointer("/params/requestId")
            && let Some(request) = registry.lock()?.get(&id.to_string())
        {
            request.suppress.store(true, Ordering::Release);
            request.token.cancel();
        }
        return Ok(Admission::Ignore);
    }
    let id = valid_id(&message);
    let key = id.as_ref().map(Value::to_string);
    let token = CancellationToken::new();
    let suppress = Arc::new(AtomicBool::new(false));
    if let Some(key) = &key {
        let mut requests = registry.lock()?;
        if requests.contains_key(key) {
            return Ok(Admission::Reply(Outbound::response(
                rpc_error(
                    id.unwrap_or(Value::Null),
                    -32600,
                    "Request ID is already pending",
                    None,
                ),
                None,
                None,
            )?));
        }
        if requests.len() >= MAX_TRACKED {
            return Err(transport_error(
                "MCP pending request/response capacity is exhausted",
            ));
        }
        requests.insert(
            key.clone(),
            Inflight {
                token: token.clone(),
                suppress: suppress.clone(),
            },
        );
    }
    Ok(Admission::Execute(Queued {
        message,
        token,
        key,
        suppress,
    }))
}

fn submit(sender: &mpsc::SyncSender<Queued>, queued: Queued) -> Result<Option<Outbound>, ApiError> {
    match sender.try_send(queued) {
        Ok(()) => Ok(None),
        Err(mpsc::TrySendError::Full(queued)) => {
            if queued.message.get("id").is_none() {
                return Ok(None);
            }
            Outbound::response(
                rpc_error(
                    valid_id(&queued.message).unwrap_or(Value::Null),
                    -32603,
                    "MCP request queue is full; this request was not executed",
                    None,
                ),
                queued.key,
                Some(queued.suppress),
            )
            .map(Some)
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            Err(transport_error("MCP operation worker stopped"))
        }
    }
}

fn transport_error(message: impl Into<String>) -> ApiError {
    ApiError {
        code: izu_api::ErrorCode::Failure,
        message: message.into(),
        next_action: "Transport failure is not rollback. An active operation may have completed; inspect native status, operation history, and the exact publication target before retrying. Undelivered receipt diagnostics are best effort.".into(),
        operation_id: None,
        recovery_operation_id: None,
        retained_paths: Box::default(),
    }
}

/// Blocking IO compatibility entrypoint for library callers. Input and output
/// must themselves be interruptible/drained by the caller; arbitrary BufRead and
/// Write implementations cannot provide the CLI's transport shutdown bound.
pub fn serve<R: BufRead, W: Write + Send>(
    mut reader: R,
    writer: W,
    api: &Api,
) -> Result<(), ApiError> {
    let writer = Mutex::new(writer);
    let registry = Arc::new(Registry::default());
    let (sender, receiver) = mpsc::sync_channel::<Queued>(MAX_PENDING);
    std::thread::scope(|scope| {
        let active = registry.clone();
        let output = &writer;
        let worker = std::thread::Builder::new()
            .name("izu-mcp-operation".into())
            .spawn_scoped(scope, move || -> Result<(), ApiError> {
                let mut session = Session::default();
                for queued in receiver {
                    let response = handle_message(queued.message, api, &queued.token, &mut session);
                    if let Some(response) = response {
                        let frame = Outbound::response(
                            response,
                            queued.key.clone(),
                            Some(queued.suppress),
                        )?;
                        emit_frame(output, &frame)?;
                    }
                    active.finish(queued.key.as_deref())?;
                }
                Ok(())
            })
            .map_err(|error| {
                transport_error(format!("Cannot start MCP operation worker: {error}"))
            })?;
        let read_result = (|| -> Result<(), ApiError> {
            loop {
                let frame = match read_frame(&mut reader) {
                    Ok(Some(bytes)) => Ok(bytes),
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
                let reply = match admit(frame, &registry)? {
                    Admission::Execute(queued) => submit(&sender, queued)?,
                    Admission::Reply(reply) => Some(reply),
                    Admission::Ignore => None,
                };
                if let Some(reply) = reply {
                    emit_frame(&writer, &reply)?;
                    registry.finish(reply.key.as_deref())?;
                }
            }
            Ok(())
        })();
        // EOF or terminal input failure initiates shutdown. Cancel pending work,
        // but still report any durable receipt already returned by the engine.
        let cancellation = registry.cancel_all();
        drop(sender);
        let worker_result = worker
            .join()
            .map_err(|_| transport_error("MCP operation worker terminated unexpectedly"))?;
        read_result?;
        cancellation?;
        worker_result
    })
}

fn valid_id(message: &Value) -> Option<Value> {
    let id = message.get("id")?;
    match id {
        Value::String(text) if text.len() <= 256 => Some(id.clone()),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(id.clone()),
        _ => None,
    }
}

fn handle_message(
    message: Value,
    api: &Api,
    cancel: &CancellationToken,
    session: &mut Session,
) -> Option<Value> {
    let id = valid_id(&message);
    if !message.is_object()
        || message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || message.get("method").and_then(Value::as_str).is_none()
        || message.get("id").is_some() && id.is_none()
    {
        return Some(rpc_error(
            Value::Null,
            -32600,
            "Expected a JSON-RPC 2.0 request with a string method and integer or string id",
            None,
        ));
    }
    let method = message["method"].as_str()?;
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    if id.is_none() {
        if method == "notifications/initialized"
            && matches!(session.legacy, LegacyState::Initializing)
        {
            session.legacy = LegacyState::Ready;
        }
        return None;
    }
    let id = id?;
    if !params.is_object() {
        return Some(rpc_error(id, -32602, "params must be an object", None));
    }
    if method == "initialize" {
        if !matches!(session.legacy, LegacyState::New) {
            return Some(rpc_error(
                id,
                -32600,
                "Legacy initialization already completed",
                None,
            ));
        }
        if params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .is_none()
            || !params.get("capabilities").is_some_and(Value::is_object)
            || params
                .pointer("/clientInfo/name")
                .and_then(Value::as_str)
                .is_none()
            || params
                .pointer("/clientInfo/version")
                .and_then(Value::as_str)
                .is_none()
        {
            return Some(rpc_error(
                id,
                -32602,
                "initialize requires protocolVersion, capabilities, and clientInfo name/version",
                None,
            ));
        }
        session.legacy = LegacyState::Initializing;
        return Some(rpc_result(
            id,
            json!({"protocolVersion":LEGACY_VERSION,"capabilities":{"tools":{"listChanged":false}},"serverInfo":server_info(),"instructions":instructions()}),
            false,
        ));
    }
    let modern = if let Some(meta) = params.get("_meta") {
        let requested = match meta
            .get("io.modelcontextprotocol/protocolVersion")
            .and_then(Value::as_str)
        {
            Some(version) => version,
            None => {
                return Some(rpc_error(
                    id,
                    -32602,
                    "Missing _meta.io.modelcontextprotocol/protocolVersion",
                    None,
                ));
            }
        };
        if requested != MODERN_VERSION {
            return Some(rpc_error(
                id,
                -32022,
                "Unsupported protocol version",
                Some(json!({"supported":[MODERN_VERSION],"requested":requested})),
            ));
        }
        if !meta
            .get("io.modelcontextprotocol/clientCapabilities")
            .is_some_and(Value::is_object)
        {
            return Some(rpc_error(
                id,
                -32602,
                "Missing _meta.io.modelcontextprotocol/clientCapabilities",
                None,
            ));
        }
        true
    } else if matches!(session.legacy, LegacyState::Ready) || method == "ping" {
        false
    } else {
        return Some(rpc_error(
            id,
            -32602,
            "Supply current per-request MCP metadata, or complete initialize and notifications/initialized for the legacy protocol",
            None,
        ));
    };
    let result = match method {
        "server/discover" if modern => Ok(
            json!({"supportedVersions":[MODERN_VERSION,LEGACY_VERSION],"capabilities":{"tools":{"listChanged":false}},"instructions":instructions()}),
        ),
        "ping" => Ok(json!({})),
        "tools/list" => {
            if params
                .get("cursor")
                .is_some_and(|cursor| !cursor.is_null() && cursor.as_str() != Some(""))
            {
                Err(rpc_error(
                    id.clone(),
                    -32602,
                    "This tool list has no additional cursor",
                    None,
                ))
            } else {
                tools()
                    .map(|tools| json!({"tools":tools}))
                    .map_err(|error| rpc_error(id.clone(), -32603, error.to_string(), None))
            }
        }
        "tools/call" => call_tool(&params, api, cancel)
            .map_err(|error| rpc_error(id.clone(), -32602, error, None)),
        _ => Err(rpc_error(
            id.clone(),
            -32601,
            format!("Unknown method: {method}"),
            None,
        )),
    };
    Some(match result {
        Ok(result) => rpc_result(id, result, modern),
        Err(error) => error,
    })
}

fn call_tool(params: &Value, api: &Api, cancel: &CancellationToken) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call requires a tool name".to_string())?;
    let arguments = params
        .get("arguments")
        .ok_or_else(|| "tools/call requires typed request arguments".to_string())?;
    let operation_name = name
        .strip_prefix("izu_")
        .ok_or_else(|| format!("Unknown tool: {name}"))?;
    if managed_launch(operation_name)
        || !operation_names()?
            .iter()
            .any(|supported| supported == operation_name)
    {
        return Err(format!("Unknown tool: {name}"));
    }
    let bytes = serde_json::to_vec(arguments).map_err(|error| error.to_string())?;
    let request = match parse_request(&bytes) {
        Ok(request) => request,
        Err(error) => {
            let response = izu_api::Response::error(error);
            return tool_response(response);
        }
    };
    if request.operation.name() != operation_name {
        return Err("Tool name must match the request's typed operation kind".into());
    }
    tool_response(api.execute(request, cancel))
}

fn tool_response(response: izu_api::Response) -> Result<Value, String> {
    let is_error = response.exit_code() != 0;
    let text = String::from_utf8(
        bounded_json(&response, MAX_RESULT_BYTES).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let structured = serde_json::to_value(response).map_err(|error| error.to_string())?;
    Ok(
        json!({"content":[{"type":"text","text":text}],"structuredContent":structured,"isError":is_error}),
    )
}

fn operation_variants() -> Result<(Value, Vec<Value>), ApiError> {
    let schema = request_schema()?;
    let variants = schema
        .pointer("/$defs/Operation/oneOf")
        .or_else(|| schema.pointer("/$defs/Operation/anyOf"))
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::invalid("Generated Operation schema has no variant set"))?
        .clone();
    Ok((schema, variants))
}

fn operation_names() -> Result<Vec<String>, String> {
    let (_, variants) = operation_variants().map_err(|error| error.to_string())?;
    variants
        .into_iter()
        .map(|variant| {
            variant
                .pointer("/properties/kind/const")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "Generated operation schema has no kind tag".into())
        })
        .collect()
}

pub fn tools() -> Result<Vec<Value>, ApiError> {
    let (schema, variants) = operation_variants()?;
    let mut tools = Vec::new();
    for variant in variants {
        let name = variant
            .pointer("/properties/kind/const")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid("Generated operation schema has no kind tag"))?;
        if managed_launch(name) {
            continue;
        }
        let mut input = schema.clone();
        input["properties"]["operation"] = variant.clone();
        input["properties"]["schema_version"] = json!({"type":"integer","const":SCHEMA_VERSION});
        tools.push(json!({"name":format!("izu_{name}"),"description":format!("Typed izu {name} operation. {}",instructions()),"inputSchema":input,"annotations":{"readOnlyHint":matches!(name,"capabilities"|"schema"|"inspect"|"status"|"diff"|"log"|"show"|"workspace_list"|"ref_list"|"operations"|"candidate_show"|"verify"|"bundle_inspect"|"bundle_verify"|"environment_status"|"jobs_list"|"job_status"|"writer_status"),"openWorldHint":matches!(name,"git_import"|"git_fetch"|"git_push"|"git_export"|"check"|"land_current"|"job_cancel")}}));
    }
    Ok(tools)
}

fn managed_launch(name: &str) -> bool {
    matches!(name, "agent_run" | "managed_start" | "managed_run")
}

fn instructions() -> &'static str {
    "Use schema_version 1 and exact IDs. Mutations need the supplied workspace expectation. Local checkpoints are not remote backups. MCP discovery does not bind a host thread; explicitly manage it with `izu start NAME -- PROGRAM ARGS`. Generic process launch is absent from MCP."
}

fn server_info() -> Value {
    let build = BuildIdentity::default();
    json!({"name":build.product,"version":build.version,"description":format!("Native format {}; build {:?}",build.native_format_version,build.build_id)})
}

fn rpc_result(id: Value, mut result: Value, modern: bool) -> Value {
    if modern {
        result["resultType"] = json!("complete");
        result["_meta"] = json!({"io.modelcontextprotocol/serverInfo":server_info()});
    }
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn rpc_error(id: Value, code: i32, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut error = json!({"code":code,"message":message.into()});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc":"2.0","id":id,"error":error})
}

struct BoundedBytes(Vec<u8>);
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|size| size > MAX_RESPONSE_BYTES)
        {
            return Err(std::io::Error::other("MCP response exceeds output limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_response(value: &Value) -> Result<Vec<u8>, ApiError> {
    let mut bytes = BoundedBytes(Vec::new());
    if serde_json::to_writer(&mut bytes, value).is_err() {
        bytes.0.clear();
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let error = rpc_error(
            id,
            -32603,
            "Operation response exceeds the output limit; the operation may already have completed. Inspect exact native state before retrying",
            Some(receipt_summary(value)),
        );
        serde_json::to_writer(&mut bytes, &error)
            .map_err(|error| ApiError::invalid(error.to_string()))?;
    }
    bytes.0.push(b'\n');
    Ok(bytes.0)
}

fn emit_frame<W: Write>(writer: &Mutex<W>, frame: &Outbound) -> Result<(), ApiError> {
    let mut writer = writer
        .lock()
        .map_err(|_| transport_error("MCP stdout is unavailable"))?;
    if frame.suppressed() {
        return Ok(());
    }
    // Once writing begins, cancellation cannot remove the remainder of a frame.
    writer
        .write_all(&frame.bytes)
        .and_then(|()| writer.flush())
        .map_err(|error| transport_error(format!("Cannot write MCP response: {error}")))
}

/// Retain only authoritative returned IDs/status, never request arguments,
/// source contents, environment values, or arbitrary engine error messages.
fn receipt_summary(response: &Value) -> Value {
    let outcome = response.pointer("/result/structuredContent/outcome");
    let mut ids = BTreeMap::new();
    if let Some(outcome) = outcome {
        for path in [
            "/error/operation_id",
            "/error/recovery_operation_id",
            "/result/data/operation",
            "/result/data/receipt/operation",
            "/result/data/recovery/operation",
            "/result/data/workspace_close/operation",
            "/result/data/recovery_operation",
            "/result/data/receipt/recovery_operation",
            "/result/data/recovery/recovery_operation",
            "/result/data/revision",
            "/result/data/candidate",
            "/result/data/Ready/candidate",
            "/result/data/Ready/revision",
            "/result/data/job/id",
            "/result/data/id",
            "/result/data/object_id",
            "/result/data/attempt",
            "/result/data/evidence",
            "/result/data/land/operation",
            "/result/data/land/revision",
            "/result/data/Applied/receipt/operation",
            "/result/data/Applied/receipt/recovery_operation",
            "/result/data/Conflicted/operation",
            "/result/data/Conflicted/revision",
            "/result/data/preparation/Ready/candidate",
            "/result/data/preparation/Ready/revision",
            "/result/data/verification/manifest/root_operation",
            "/result/data/archive/manifest/root_operation",
        ] {
            if let Some(id) = outcome.pointer(path).and_then(Value::as_str)
                && matches!(id.len(), 32 | 64)
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                ids.insert(path, id);
            }
        }
    }
    json!({"outcome":outcome.and_then(|value| value.get("kind")).and_then(Value::as_str),
        "error_code":outcome.and_then(|value|value.pointer("/error/code")).and_then(Value::as_str),
        "native_ids":ids})
}

#[cfg(not(unix))]
pub fn serve_stdio(_: impl FnOnce() -> Result<Api, ApiError>) -> Result<(), ApiError> {
    Err(ApiError::unsupported(
        "Cancellable MCP stdio on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_id_stays_reserved_until_response_delivery_or_discard() {
        let registry = Registry::default();
        let bytes = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_vec();
        assert!(matches!(
            admit(Ok(bytes.clone()), &registry).expect("first admission"),
            Admission::Execute(_)
        ));
        let duplicate = admit(Ok(bytes.clone()), &registry).expect("duplicate admission");
        let Admission::Reply(reply) = duplicate else {
            panic!("pending ID must reject reuse");
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.bytes).expect("error")["error"]["code"],
            -32600
        );
        // Worker completion alone never removes a key. Only delivered/discarded
        // output invokes finish; queued/full-queue responses obey the same rule.
        registry.finish(Some("1")).expect("delivered");
        assert!(matches!(
            admit(Ok(bytes), &registry).expect("reuse"),
            Admission::Execute(_)
        ));
    }

    #[test]
    fn eof_cancellation_does_not_suppress_receipts_but_protocol_cancel_does() {
        let registry = Registry::default();
        let Admission::Execute(queued) = admit(
            Ok(br#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#.to_vec()),
            &registry,
        )
        .expect("request") else {
            panic!("execute");
        };
        registry.cancel_all().expect("EOF cancellation");
        assert!(queued.token.is_cancelled());
        assert!(!queued.suppress.load(Ordering::Acquire));
        admit(
            Ok(
                br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#
                    .to_vec(),
            ),
            &registry,
        )
        .expect("protocol cancel");
        assert!(queued.suppress.load(Ordering::Acquire));
    }

    #[test]
    fn receipt_diagnostic_preserves_exact_visible_and_recovery_ids_without_arguments() {
        let operation = "a".repeat(64);
        let recovery = "b".repeat(64);
        let response = json!({"result":{"structuredContent":{"outcome":{"kind":"uncertain","error":{"operation_id":operation,"recovery_operation_id":recovery,"message":"secret source"},"result":{"data":{"operation":operation,"contents":"secret source"}}}}}});
        let receipt = receipt_summary(&response);
        assert_eq!(receipt["outcome"], "uncertain");
        assert_eq!(receipt["native_ids"]["/error/operation_id"], operation);
        assert_eq!(
            receipt["native_ids"]["/error/recovery_operation_id"],
            recovery
        );
        assert!(!receipt.to_string().contains("secret source"));
    }
    #[test]
    fn tool_hints_are_conservative_and_generic_launch_is_absent() {
        let tools = tools().expect("derived tools");
        for name in ["izu_agent_run", "izu_managed_start", "izu_managed_run"] {
            assert!(!tools.iter().any(|tool| tool["name"] == name));
        }
        for name in [
            "izu_git_import",
            "izu_git_fetch",
            "izu_git_push",
            "izu_git_export",
            "izu_check",
            "izu_land_current",
            "izu_job_cancel",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == name)
                .expect("external-capable tool");
            assert_eq!(tool["annotations"]["openWorldHint"], true);
        }
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "izu_status")
                .expect("status")["annotations"]["readOnlyHint"],
            true
        );
    }
    #[test]
    fn oversized_frame_drains_without_consuming_next_message() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES + 5];
        input.extend_from_slice(b"\n{}\n");
        let mut reader = Cursor::new(input);
        assert!(read_frame(&mut reader).is_err());
        assert_eq!(
            read_frame(&mut reader).expect("second frame"),
            Some(b"{}".to_vec())
        );
        assert!(read_frame(&mut reader).expect("eof").is_none());
    }

    #[test]
    fn terminal_input_error_stops_worker() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("terminal fixture input failure"))
            }
        }
        impl BufRead for Broken {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Err(std::io::Error::other("terminal fixture input failure"))
            }
            fn consume(&mut self, _: usize) {}
        }
        let api = Api::new(std::env::current_exe().expect("test binary"));
        assert!(serve(Broken, Vec::new(), &api).is_err());
    }
}
