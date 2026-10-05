use izu_api::{
    Api, ApiError, CancellationToken, ErrorCode, MAX_REQUEST_BYTES, Operation, Outcome, Request,
    bounded_json, parse_request, request_schema,
};
use serde::Serialize;

#[test]
fn unknown_fields_versions_and_oversized_json_are_errors() {
    for input in [
        b"{\"schema_version\":1,\"operation\":{\"kind\":\"capabilities\",\"hidden\":true}}"
            .as_slice(),
        b"{\"schema_version\":88,\"operation\":{\"kind\":\"capabilities\"}}",
        b"{\"schema_version\":1,\"operation\":{\"kind\":\"unknown\"}}",
        b"{",
        b"null",
    ] {
        assert!(parse_request(input).is_err(), "accepted {:?}", input);
    }
    assert!(parse_request(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
    let error = parse_request(b"{\"schema_version\":99,\"operation\":{\"kind\":\"capabilities\"}}")
        .expect_err("unsupported version");
    assert!(matches!(error.code, ErrorCode::UnsupportedSchema));
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn malformed_public_input_does_not_panic() {
    for bytes in [
        vec![],
        vec![0xff],
        vec![0; 4096],
        b"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[".to_vec(),
        b"{\"schema_version\":-1}".to_vec(),
    ] {
        let result = std::panic::catch_unwind(|| parse_request(&bytes));
        assert!(result.is_ok());
        assert!(result.expect("no panic").is_err());
    }
}

#[test]
fn serialization_stops_at_bound_before_wire_value() {
    struct Large;
    impl Serialize for Large {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(None)?;
            for _ in 0..100_000 {
                sequence.serialize_element(&"x".repeat(128))?;
            }
            sequence.end()
        }
    }
    assert!(bounded_json(&Large, 1024).is_err());
    assert!(bounded_json(&vec![1, 2, 3], 32).is_ok());
}

#[test]
fn cancelled_request_does_not_mutate() {
    let temp = tempfile::tempdir().expect("fixture");
    let destination = temp.path().join("never-created");
    let token = CancellationToken::new();
    token.cancel();
    let api = Api::new(std::env::current_exe().expect("test executable"));
    let response = api.execute(
        Request::new(Operation::Init {
            path: destination.clone(),
        }),
        &token,
    );
    assert!(matches!(
        response.outcome,
        Outcome::Error {
            error: ApiError {
                code: ErrorCode::Cancelled,
                ..
            },
            ..
        }
    ));
    assert!(!destination.exists());
}

#[test]
fn generated_schema_describes_the_authoritative_request_enum() {
    let schema = request_schema().expect("schema");
    assert_eq!(schema["additionalProperties"], false);
    let variants = schema
        .pointer("/$defs/Operation/oneOf")
        .and_then(serde_json::Value::as_array)
        .expect("tagged operation variants");
    assert!(variants.iter().any(|variant| {
        variant
            .pointer("/properties/kind/const")
            .and_then(serde_json::Value::as_str)
            == Some("commit")
    }));
    assert!(
        variants
            .iter()
            .all(|variant| variant["additionalProperties"] == false)
    );
}

#[test]
fn legacy_checks_default_to_no_binding_and_schema_exposes_explicit_binding() {
    let request = parse_request(br#"{"schema_version":1,"operation":{"kind":"candidate","context":{"repository":".","workspace":null},"source":"00","target":"main","expected_target":null,"checks":[{"name":"tests","argv":["true"]}],"author":{"name":"explicit","email":"author@example.invalid"}}}"#).expect("legacy check shape");
    let Operation::Candidate { checks, .. } = request.operation else {
        panic!("candidate")
    };
    assert!(checks[0].environment.is_none());
    let schema = request_schema().expect("schema");
    assert!(
        schema
            .pointer("/$defs/CheckDefinition/properties/environment")
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn bounded_file_input_rejects_fifo_symlink_and_oversized_regular_file() {
    let temp = tempfile::tempdir().expect("owned Neural fixture");
    let root = temp.path().canonicalize().expect("real input parent");
    let fifo = root.join("fifo");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("owned FIFO")
            .success()
    );
    let start = std::time::Instant::now();
    assert!(izu_api::read_input_file(&fifo, 64 * 1024).is_err());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "FIFO opening must not wait for a writer"
    );
    let file = root.join("regular");
    std::fs::write(&file, b"complete").expect("regular input");
    assert_eq!(
        izu_api::read_input_file(&file, 8).expect("bounded input"),
        b"complete"
    );
    assert!(izu_api::read_input_file(&file, 7).is_err());
    let link = root.join("alias");
    std::os::unix::fs::symlink(&file, &link).expect("owned symlink");
    assert!(izu_api::read_input_file(&link, 8).is_err());
}

#[test]
fn git_authentication_debug_is_redacted_and_legacy_local_json_remains_typed() {
    let auth = izu_api::GitAuthentication::Basic {
        username: "operator".into(),
        password: "private-fixture-token".into(),
    };
    assert!(!format!("{auth:?}").contains("private-fixture-token"));
    let request = parse_request(br#"{"schema_version":1,"operation":{"kind":"git_fetch","context":{"repository":".","workspace":null},"source":"local-source","refs":[]}}"#).expect("legacy local path");
    assert!(matches!(
        request.operation,
        Operation::GitFetch {
            source: izu_api::GitLocation::Local(_),
            ..
        }
    ));
    assert!(parse_request(br#"{"schema_version":1,"operation":{"kind":"git_fetch","context":{"repository":".","workspace":null},"source":{"url":"https://example.invalid/owned.git","unexpected":true},"refs":[]}}"#).is_err());
}
