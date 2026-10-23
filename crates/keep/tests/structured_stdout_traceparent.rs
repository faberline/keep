// HANDWRITE-BEGIN gap="missing-generator:unit-test:2eb048f0" tracker="#2414" reason="Spawn the compiled Keep server with JSON logging, exercise valid invalid and absent W3C traceparent requests, and assert schema identity correlation and Sift agnosticism from captured stdout."
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
const PARENT_SPAN_ID: &str = "00f067aa0ba902b7";
const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";

struct KeepProcess {
    child: Child,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

impl KeepProcess {
    fn spawn(port: u16) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_keep"))
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--disable-persistence",
                "--log-level",
                "info",
                "--log-format",
                "json",
            ])
            .env_remove("RUST_LOG")
            .env_remove("KEEP_LOG_FORMAT")
            .env_remove("KEEP_OTLP_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("SIFT_URL")
            .env_remove("SIFT_ENDPOINT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn real keep process");
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stdout_reader = drain_lines(
            child.stdout.take().expect("keep stdout pipe"),
            Arc::clone(&stdout),
        );
        let stderr_reader = drain_lines(
            child.stderr.take().expect("keep stderr pipe"),
            Arc::clone(&stderr),
        );
        Self {
            child,
            stdout,
            stderr,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr.lock().unwrap().join("\n")
    }

    fn finish(&mut self) -> Vec<String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.stdout_reader.take() {
            reader.join().expect("join keep stdout reader");
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader.join().expect("join keep stderr reader");
        }
        self.stdout.lock().unwrap().clone()
    }
}

impl Drop for KeepProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

fn drain_lines(
    stream: impl std::io::Read + Send + 'static,
    destination: Arc<Mutex<Vec<String>>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            destination.lock().unwrap().push(line);
        }
    })
}

fn reserve_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    listener.local_addr().unwrap().port()
}

async fn wait_ready(process: &mut KeepProcess, client: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client.get(format!("{base}/healthz")).send().await {
            if response.status().is_success() {
                return;
            }
        }
        if let Some(status) = process.child.try_wait().expect("poll keep child") {
            panic!(
                "keep exited before readiness with {status}: {}",
                process.stderr_text()
            );
        }
        assert!(
            Instant::now() < deadline,
            "keep readiness timeout: {}",
            process.stderr_text()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn put_value(client: &reqwest::Client, base: &str, key: &str, traceparent: Option<&str>) {
    let mut request = client
        .put(format!("{base}/kv/{key}"))
        .json(&json!({ "value": "trace-test" }));
    if let Some(traceparent) = traceparent {
        request = request.header("traceparent", traceparent);
    }
    let response = request.send().await.expect("put keep value");
    assert!(
        response.status().is_success(),
        "put {key} failed with {}: {}",
        response.status(),
        response.text().await.unwrap_or_default()
    );
}

fn request_event<'a>(events: &'a [Value], key: &str) -> &'a Value {
    let uri = format!("/kv/{key}");
    events
        .iter()
        .find(|event| event["attributes"]["uri"].as_str() == Some(uri.as_str()))
        .unwrap_or_else(|| panic!("missing request event for {uri}: {events:#?}"))
}

fn required_str<'a>(event: &'a Value, field: &str) -> &'a str {
    event
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field {field}: {event:#}"))
}

/// Validate `value` against the subset of JSON Schema 2020-12 that the shared
/// `axiom.service.log.v1` contract uses, collecting every violation. An unknown
/// keyword panics, so a contract change in core fails this gate instead of
/// being skipped.
fn schema_errors(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        panic!("schema at {path} is not an object: {schema}");
    };
    for (keyword, rule) in schema {
        match keyword.as_str() {
            "$schema" | "$id" | "title" | "aw_ownership" => {}
            // Applied together with `properties` below.
            "additionalProperties" if schema.contains_key("properties") => {}
            "type" => {
                let ty = rule.as_str().expect("type is a string");
                let ok = match ty {
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "string" => value.is_string(),
                    "number" => value.is_number(),
                    "integer" => value.is_i64() || value.is_u64(),
                    "boolean" => value.is_boolean(),
                    "null" => value.is_null(),
                    other => panic!("unsupported schema type {other}"),
                };
                if !ok {
                    errors.push(format!("{path}: expected {ty}, got {value}"));
                }
            }
            "const" if value != rule => {
                errors.push(format!("{path}: expected {rule}, got {value}"))
            }
            "const" => {}
            "enum" => {
                if !rule.as_array().expect("enum array").contains(value) {
                    errors.push(format!("{path}: {value} not in {rule}"));
                }
            }
            "required" => {
                for field in rule.as_array().expect("required array") {
                    let field = field.as_str().expect("required name");
                    if value.get(field).is_none() {
                        errors.push(format!("{path}: missing required `{field}`"));
                    }
                }
            }
            "properties" => {
                let Some(object) = value.as_object() else {
                    continue;
                };
                let properties = rule.as_object().expect("properties object");
                for (field, field_value) in object {
                    let at = format!("{path}.{field}");
                    match (properties.get(field), schema.get("additionalProperties")) {
                        (Some(sub), _) => schema_errors(sub, field_value, &at, errors),
                        (None, Some(Value::Bool(false))) => {
                            errors.push(format!("{at}: property not allowed"))
                        }
                        (None, Some(sub)) if sub.is_object() => {
                            schema_errors(sub, field_value, &at, errors)
                        }
                        (None, _) => {}
                    }
                }
            }
            "additionalProperties" => {
                let Some(object) = value.as_object() else {
                    continue;
                };
                for (field, field_value) in object {
                    let at = format!("{path}.{field}");
                    match rule {
                        Value::Bool(false) => errors.push(format!("{at}: property not allowed")),
                        Value::Bool(true) => {}
                        sub => schema_errors(sub, field_value, &at, errors),
                    }
                }
            }
            "propertyNames" => {
                for field in value.as_object().into_iter().flat_map(|o| o.keys()) {
                    schema_errors(
                        rule,
                        &Value::String(field.clone()),
                        &format!("{path}[key {field}]"),
                        errors,
                    );
                }
            }
            "maxProperties" => {
                let max = rule.as_u64().expect("maxProperties") as usize;
                if value.as_object().is_some_and(|o| o.len() > max) {
                    errors.push(format!("{path}: more than {max} properties"));
                }
            }
            "minLength" | "maxLength" => {
                let Some(text) = value.as_str() else { continue };
                let bound = rule.as_u64().expect("length bound") as usize;
                let len = text.chars().count();
                if (keyword == "minLength" && len < bound)
                    || (keyword == "maxLength" && len > bound)
                {
                    errors.push(format!("{path}: length {len} violates {keyword} {bound}"));
                }
            }
            "pattern" => {
                let Some(text) = value.as_str() else { continue };
                let re =
                    regex::Regex::new(rule.as_str().expect("pattern")).expect("schema pattern");
                if !re.is_match(text) {
                    errors.push(format!("{path}: {text:?} does not match {rule}"));
                }
            }
            "format" => match (rule.as_str(), value.as_str()) {
                (Some("date-time"), Some(text)) => {
                    if chrono::DateTime::parse_from_rfc3339(text).is_err() {
                        errors.push(format!("{path}: {text:?} is not an RFC 3339 date-time"));
                    }
                }
                (Some("date-time"), None) => {}
                (other, _) => panic!("unsupported schema format {other:?}"),
            },
            "oneOf" => {
                let matching = rule
                    .as_array()
                    .expect("oneOf array")
                    .iter()
                    .filter(|branch| {
                        let mut branch_errors = Vec::new();
                        schema_errors(branch, value, path, &mut branch_errors);
                        branch_errors.is_empty()
                    })
                    .count();
                if matching != 1 {
                    errors.push(format!("{path}: {value} matches {matching} oneOf branches"));
                }
            }
            other => panic!("unsupported schema keyword `{other}` at {path}; extend schema_errors"),
        }
    }
}

fn assert_service_log_v1(schema: &Value, event: &Value) {
    let mut errors = Vec::new();
    schema_errors(schema, event, "$", &mut errors);
    assert!(
        errors.is_empty(),
        "invalid axiom.service.log.v1 record: {errors:#?}\n{event:#}"
    );
}

#[test]
fn schema_validator_rejects_malformed_records() {
    let schema = service_observability::service_log_schema_v1();
    let good = json!({
        "schema": "axiom.service.log.v1",
        "timestamp": "2026-10-22T07:53:42.605430Z",
        "severity": "INFO",
        "service": { "name": "keep", "version": "0.4.13" },
        "event": "request",
        "message": "ok",
        "trace_id": TRACE_ID,
        "attributes": { "status": 200, "uri": "/kv/a" },
    });
    assert_service_log_v1(&schema, &good);

    let malformed = [
        ("missing service", json!({ "service": null })),
        ("bad severity", json!({ "severity": "NOTICE" })),
        ("bad timestamp", json!({ "timestamp": "yesterday" })),
        ("bad trace_id", json!({ "trace_id": "XYZ" })),
        ("unknown top-level field", json!({ "sift_token": "x" })),
        (
            "nested attribute",
            json!({ "attributes": { "nested": { "a": 1 } } }),
        ),
        ("empty event", json!({ "event": "" })),
    ];
    for (label, patch) in malformed {
        let mut record = good.clone();
        for (field, value) in patch.as_object().unwrap() {
            if value.is_null() {
                record.as_object_mut().unwrap().remove(field);
            } else {
                record[field] = value.clone();
            }
        }
        let mut errors = Vec::new();
        schema_errors(&schema, &record, "$", &mut errors);
        assert!(
            !errors.is_empty(),
            "validator accepted a record with {label}: {record:#}"
        );
    }
}

fn valid_nonzero_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && value.bytes().any(|byte| byte != b'0')
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_keep_process_correlates_structured_stdout() {
    let port = reserve_port();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .http1_only()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let mut process = KeepProcess::spawn(port);
    wait_ready(&mut process, &client, &base).await;

    put_value(&client, &base, "trace-valid", Some(TRACEPARENT)).await;
    put_value(
        &client,
        &base,
        "trace-invalid",
        Some("00-00000000000000000000000000000000-0000000000000000-01"),
    )
    .await;
    put_value(&client, &base, "trace-missing", None).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let lines = process.finish();
    assert!(!lines.is_empty(), "keep emitted no stdout records");
    let events = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("non-JSON stdout line ({error}): {line}"))
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), lines.len());
    let schema = service_observability::service_log_schema_v1();
    for event in &events {
        assert_service_log_v1(&schema, event);
        assert_eq!(event["service"]["name"], "keep", "{event:#}");
    }

    let valid = request_event(&events, "trace-valid");
    assert_eq!(required_str(valid, "trace_id"), TRACE_ID);
    assert_eq!(required_str(valid, "parent_span_id"), PARENT_SPAN_ID);
    assert_eq!(required_str(valid, "trace_flags"), "01");
    assert!(valid_nonzero_lower_hex(required_str(valid, "span_id"), 16));

    let invalid = request_event(&events, "trace-invalid");
    let invalid_trace = required_str(invalid, "trace_id");
    assert!(valid_nonzero_lower_hex(invalid_trace, 32));
    assert!(valid_nonzero_lower_hex(
        required_str(invalid, "span_id"),
        16
    ));
    assert!(invalid.get("parent_span_id").is_none());

    let missing = request_event(&events, "trace-missing");
    let missing_trace = required_str(missing, "trace_id");
    assert!(valid_nonzero_lower_hex(missing_trace, 32));
    assert!(valid_nonzero_lower_hex(
        required_str(missing, "span_id"),
        16
    ));
    assert!(missing.get("parent_span_id").is_none());
    assert_ne!(invalid_trace, missing_trace);

    assert!(
        !include_str!("../../../Cargo.toml").contains("sift =")
            && !include_str!("../Cargo.toml").contains("sift =")
            && !include_str!("../src/bin/keep.rs").contains("SIFT_"),
        "Keep must not depend on or configure Sift directly"
    );
}
// HANDWRITE-END
