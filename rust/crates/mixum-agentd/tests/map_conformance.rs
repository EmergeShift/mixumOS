//! MAP v1 会话/任务/记忆的端到端测试(真实 WS 回环)。
//!
//! 锁定交接文档 §2 的验收面:session.hello 与 hello 别名、map.v1/mixum.v1
//! 子协议、task.event 的 MAP data 形状、task.reply 人机输入、session.resume
//! 重放环、mem.* 域,以及 map1. 令牌前缀。

use futures_util::{SinkExt, StreamExt};
use mixum_agentd::auth::Authorizer;
use mixum_agentd::display::Broker;
use mixum_agentd::server::{Config, Server};
use mixum_agentd::snapshot::MemorySnapshotEngine;
use mixum_agentd::task::{Engine, ExternalDriver, LocalSandbox, Registry, ShellDriver};
use mixum_protocol::envelope::{new_id, Envelope};
use mixum_protocol::ops::Cap;
use mixum_protocol::token::{Claims, Signer, Verifier};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    addr: std::net::SocketAddr,
    token: String,
    signer: Signer,
    // 保活临时目录(server 的 dataDir 在其中)。
    _dir: tempfile::TempDir,
}

async fn start() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        listen: "127.0.0.1:0".into(),
        data_dir: dir.path().join("data").to_string_lossy().into_owned(),
        work_root: dir.path().join("work").to_string_lossy().into_owned(),
        hostname: "map-test".into(),
        encrypt_snapshots: false,
        ..Default::default()
    };
    let signer = Signer::generate();
    let authorizer = Authorizer::new(Verifier::new(signer.verifying_key()));
    let registry = Arc::new(Registry::new());
    registry.register(ShellDriver).unwrap();
    registry.register(ExternalDriver { command: None }).unwrap();
    let engine = Engine::with_snapshots(
        registry,
        Arc::new(LocalSandbox::new()),
        Some(Arc::new(MemorySnapshotEngine::new())),
    );
    let broker = Broker::new(dir.path().join("display.sock"));
    let server = Server::new(
        config,
        authorizer,
        engine,
        broker,
        Arc::new(MemorySnapshotEngine::new()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = server.serve_plain_listener(listener).await;
    });
    let token = signer
        .mint(Claims {
            sub: "map-test".into(),
            caps: vec![Cap::Admin],
            iat: 0,
            exp: 0,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![],
        })
        .unwrap();
    Harness {
        addr,
        token,
        signer,
        _dir: dir,
    }
}

impl Harness {
    fn token_with(&self, caps: Vec<Cap>) -> String {
        self.signer
            .mint(Claims {
                sub: "map-test-limited".into(),
                caps,
                iat: 0,
                exp: 0,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .unwrap()
    }
}

async fn connect(h: &Harness, protocol: &'static str) -> (Ws, String) {
    let mut request = format!("ws://{}/ws", h.addr).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(protocol));
    let (ws, response) = connect_async(request).await.unwrap();
    let selected = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (ws, selected)
}

async fn send(ws: &mut Ws, op: &str, payload: Value) -> Envelope {
    let request = Envelope::new(op, Some(payload));
    ws.send(Message::Text(request.to_json().into()))
        .await
        .unwrap();
    request
}

async fn recv(ws: &mut Ws) -> Envelope {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                return Envelope::decode(text.as_bytes()).expect("服务端应回合法 envelope")
            }
            Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => continue,
            other => panic!("连接意外结束: {other:?}"),
        }
    }
}

/// 发请求并读回它的 replyTo(期间收到的推送一并返回)。
async fn call(ws: &mut Ws, op: &str, payload: Value) -> (Envelope, Vec<Envelope>) {
    let request = send(ws, op, payload).await;
    let mut pushes = Vec::new();
    loop {
        let envelope = recv(ws).await;
        if envelope.reply_to == request.id {
            return (envelope, pushes);
        }
        pushes.push(envelope);
    }
}

async fn hello(ws: &mut Ws, token: &str, op: &str) -> Envelope {
    let (reply, _) = call(
        ws,
        op,
        json!({"token": token, "client": "conformance-test", "profiles": ["core"]}),
    )
    .await;
    reply
}

fn payload(envelope: &Envelope) -> &Value {
    envelope.payload.as_ref().expect("应答应带 payload")
}

async fn wait_task_event(
    ws: &mut Ws,
    predicate: impl Fn(&Value) -> bool,
) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    loop {
        let envelope = recv(ws).await;
        if envelope.op != "task.event" {
            continue;
        }
        let event = payload(&envelope).clone();
        if predicate(&event) {
            return (event, seen);
        }
        seen.push(event);
    }
}

#[tokio::test]
async fn handshake_aliases_subprotocols_and_prehandshake_rules() {
    let h = start().await;

    // 规范名 + map.v1。
    let (mut ws, selected) = connect(&h, "map.v1").await;
    assert_eq!(selected, "map.v1");
    let reply = hello(&mut ws, &h.token, "session.hello").await;
    assert_eq!(reply.op, "session.hello.ok");
    assert_eq!(reply.v, 1, "envelope.v 保持 1");
    let hello_payload = payload(&reply);
    assert!(
        hello_payload["sessionId"]
            .as_str()
            .is_some_and(|id| id.len() == 26),
        "sessionId 必须是 26 字符 ULID: {}",
        hello_payload["sessionId"]
    );
    assert_eq!(hello_payload["versions"], json!([1]), "versions 应为 [1]");
    assert!(
        hello_payload["profiles"]
            .as_array()
            .is_some_and(|profiles| profiles.iter().any(|p| p == "memory")),
        "hello.ok 必须回告 memory profile: {}",
        hello_payload["profiles"]
    );
    assert!(hello_payload["server"]
        .as_str()
        .is_some_and(|server| server.starts_with("mixum-agentd/")));

    // 旧别名 + mixum.v1 子协议。
    let (mut legacy, selected) = connect(&h, "mixum.v1").await;
    assert_eq!(selected, "mixum.v1");
    let legacy_reply = hello(&mut legacy, &h.token, "hello").await;
    assert_eq!(legacy_reply.op, "hello.ok");
    assert_eq!(payload(&legacy_reply)["versions"], json!([1]));

    // 两者同时提供时优先 map.v1。
    let mut request = format!("ws://{}/ws", h.addr).into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("mixum.v1, map.v1"),
    );
    let (mut both, response) = connect_async(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .unwrap()
            .to_str()
            .unwrap(),
        "map.v1"
    );

    // 握手前:sys.ping / sys.info 允许,task.list 拒绝。
    let (pong, _) = call(&mut both, "sys.ping", json!({})).await;
    assert_eq!(pong.op, "sys.ping.ok");
    let (info, _) = call(&mut both, "sys.info", json!({})).await;
    assert_eq!(info.op, "sys.info.ok");
    assert!(
        payload(&info).get("profiles").is_some() && payload(&info).get("caps").is_none(),
        "握手前 sys.info 只回 profiles/server: {}",
        payload(&info)
    );
    let (denied, _) = call(&mut both, "task.list", json!({})).await;
    assert_eq!(denied.op, "task.list.err");
    assert_eq!(payload(&denied)["code"], "unauthorized");
}

#[tokio::test]
async fn task_events_have_map_shapes_and_task_reply_delivers() {
    let h = start().await;
    let (mut ws, _) = connect(&h, "map.v1").await;
    assert_eq!(hello(&mut ws, &h.token, "session.hello").await.op, "session.hello.ok");

    let (subscribed, _) = call(&mut ws, "task.events", json!({})).await;
    assert_eq!(subscribed.op, "task.events.ok");
    assert_eq!(payload(&subscribed)["subscribed"], true);

    // external agent:input.request → 等 task.reply → 回 input.response → done。
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("agent.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
read hello
printf '%s\n' '{"type":"log","line":"bridge-ready"}'
printf '%s\n' '{"type":"input.request","id":"q1","prompt":"Proceed?","choices":["yes","no"]}'
read response
case "$response" in
  *'"value":"yes"'*) printf '%s\n' '{"type":"log","line":"answered-yes"}' ;;
  *) printf '%s\n' '{"type":"log","line":"answered-other"}' ;;
esac
printf '%s\n' '{"type":"input.request","id":"q2","prompt":"Anything else?"}'
read response2
printf '%s\n' '{"type":"done"}'
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();

    let request = send(
        &mut ws,
        "task.submit",
        json!({
            "instruction": "ask the human",
            "agent": "external",
            "params": { "agentCmd": script.to_string_lossy() },
        }),
    )
    .await;
    let mut early: Vec<Envelope> = Vec::new();
    let submitted = loop {
        let envelope = recv(&mut ws).await;
        if envelope.reply_to == request.id {
            break envelope;
        }
        early.push(envelope);
    };
    assert_eq!(submitted.op, "task.submit.ok");
    let task = payload(&submitted);
    let task_id = task["id"].as_str().unwrap().to_owned();
    assert_eq!(task_id.len(), 26, "task.id 必须是 ULID: {task_id}");

    let mut input_event = None;
    for envelope in &early {
        if envelope.op == "task.event" && payload(envelope)["type"] == "input" {
            input_event = Some(payload(envelope).clone());
        }
    }
    let mut leading_events: Vec<Value> = Vec::new();
    if input_event.is_none() {
        let (event, seen) = wait_task_event(&mut ws, |event| event["type"] == "input").await;
        leading_events.extend(seen);
        input_event = Some(event);
    }
    let input_event = input_event.unwrap();
    assert_eq!(input_event["data"]["replyId"], "q1");
    assert_eq!(input_event["data"]["prompt"], "Proceed?");
    assert_eq!(input_event["data"]["choices"], json!(["yes", "no"]));

    let (answered, pushes) = call(
        &mut ws,
        "task.reply",
        json!({"taskId": task_id, "replyId": "q1", "value": "yes"}),
    )
    .await;
    assert_eq!(answered.op, "task.reply.ok", "{:?}", payload(&answered));
    assert_eq!(payload(&answered)["delivered"], true);
    let mut events: Vec<Value> = early
        .iter()
        .chain(pushes.iter())
        .filter(|envelope| envelope.op == "task.event")
        .map(|envelope| payload(envelope).clone())
        .collect();
    events.extend(leading_events);

    // agent 接着问 q2(任务仍 running):此时重复答复 q1 必须 conflict。
    if !events
        .iter()
        .any(|event| event["type"] == "input" && event["data"]["replyId"] == "q2")
    {
        let (event, seen) =
            wait_task_event(&mut ws, |event| event["data"]["replyId"] == "q2").await;
        events.extend(seen);
        events.push(event);
    }
    let (duplicate, duplicate_pushes) = call(
        &mut ws,
        "task.reply",
        json!({"taskId": task_id, "replyId": "q1", "value": "no"}),
    )
    .await;
    assert_eq!(duplicate.op, "task.reply.err");
    assert_eq!(payload(&duplicate)["code"], "conflict");
    events.extend(
        duplicate_pushes
            .iter()
            .filter(|envelope| envelope.op == "task.event")
            .map(|envelope| payload(envelope).clone()),
    );

    let (second_answer, answer_pushes) = call(
        &mut ws,
        "task.reply",
        json!({"taskId": task_id, "replyId": "q2", "value": "no"}),
    )
    .await;
    assert_eq!(second_answer.op, "task.reply.ok");
    events.extend(
        answer_pushes
            .iter()
            .filter(|envelope| envelope.op == "task.event")
            .map(|envelope| payload(envelope).clone()),
    );

    // 等终态,并检查日志与状态事件的 MAP 形状。
    for _ in 0..400 {
        if events
            .iter()
            .any(|event| event["type"] == "state" && event["data"]["state"] == "completed")
        {
            break;
        }
        let envelope = recv(&mut ws).await;
        if envelope.op == "task.event" {
            events.push(payload(&envelope).clone());
        }
    }
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "state" && event["data"]["state"] == "queued"),
        "state 事件 data 必须是对象: {events:?}"
    );
    assert!(
        events.iter().any(|event| event["type"] == "state"
            && event["data"]["state"] == "completed"),
        "任务应完成: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "log" && event["data"]["line"] == "answered-yes"),
        "agent 应收到 input.response: {events:?}"
    );

    // task.get 返回同一 Task 对象。
    let (got, _) = call(&mut ws, "task.get", json!({"id": task_id})).await;
    assert_eq!(payload(&got)["state"], "completed");
}

#[tokio::test]
async fn session_resume_replays_and_keeps_subscription_and_claims() {
    let h = start().await;
    let (mut first, _) = connect(&h, "map.v1").await;
    let hello_reply = hello(&mut first, &h.token, "session.hello").await;
    let session_id = payload(&hello_reply)["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();

    let (subscribed, _) = call(&mut first, "task.events", json!({})).await;
    assert_eq!(payload(&subscribed)["subscribed"], true);
    let request = send(
        &mut first,
        "task.submit",
        json!({"instruction": "printf resume", "agent": "shell"}),
    )
    .await;
    let mut saw_completed = false;
    for _ in 0..400 {
        let envelope = recv(&mut first).await;
        if envelope.reply_to == request.id {
            continue;
        }
        if envelope.op == "task.event" {
            let event = payload(&envelope);
            if event["type"] == "state" && event["data"]["state"] == "completed" {
                saw_completed = true;
                break;
            }
        }
    }
    assert!(saw_completed);
    first.close(None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 新连接只发 session.resume,不 hello、不重新 task.events。
    let (mut second, _) = connect(&h, "map.v1").await;
    let resume = send(
        &mut second,
        "session.resume",
        json!({"sessionId": session_id, "lastSeq": 0}),
    )
    .await;
    let reply = recv(&mut second).await;
    assert_eq!(reply.reply_to, resume.id);
    assert_eq!(reply.op, "session.resume.ok");
    let replayed = payload(&reply)["replayed"].as_u64().unwrap();
    assert!(replayed >= 2, "至少重放 queued/completed 事件,实得 {replayed}");
    assert_eq!(payload(&reply)["sessionId"], session_id);
    assert_eq!(payload(&reply)["versions"], json!([1]));

    let mut seen_seq = Vec::new();
    for _ in 0..replayed {
        let envelope = recv(&mut second).await;
        assert_eq!(envelope.op, "task.event");
        assert!(envelope.seq > 0);
        seen_seq.push(envelope.seq);
    }
    assert!(
        seen_seq.windows(2).all(|pair| pair[0] < pair[1]),
        "重放顺序必须单调: {seen_seq:?}"
    );

    // claims 已恢复:无需 hello 即可调用受保护 op。
    let (listed, _) = call(&mut second, "task.list", json!({})).await;
    assert_eq!(listed.op, "task.list.ok");
    assert!(!payload(&listed)["tasks"].as_array().unwrap().is_empty());

    // 订阅也保留:新任务的事件不经 task.events 直接推送。
    let request = send(
        &mut second,
        "task.submit",
        json!({"instruction": "printf again", "agent": "shell"}),
    )
    .await;
    let mut saw_push = false;
    for _ in 0..400 {
        let envelope = recv(&mut second).await;
        if envelope.reply_to == request.id {
            continue;
        }
        if envelope.op == "task.event" {
            saw_push = true;
            if payload(&envelope)["type"] == "state"
                && payload(&envelope)["data"]["state"] == "completed"
            {
                break;
            }
        }
    }
    assert!(saw_push, "resume 后订阅应继续生效");

    // 未知/过期会话 → not_found。
    let (mut third, _) = connect(&h, "map.v1").await;
    let (missing, _) = call(
        &mut third,
        "session.resume",
        json!({"sessionId": new_id(), "lastSeq": 0}),
    )
    .await;
    assert_eq!(missing.op, "session.resume.err");
    assert_eq!(payload(&missing)["code"], "not_found");
}

#[tokio::test]
async fn memory_domain_round_trip_and_capabilities() {
    let h = start().await;
    let (mut ws, _) = connect(&h, "map.v1").await;
    assert_eq!(hello(&mut ws, &h.token, "session.hello").await.op, "session.hello.ok");

    let (providers, _) = call(&mut ws, "mem.providers", json!({})).await;
    assert_eq!(providers.op, "mem.providers.ok");
    assert_eq!(payload(&providers)["providers"][0]["id"], "builtin");

    let record = json!({
        "provider": "builtin",
        "kind": "preference",
        "content": { "text": "Prefers concise answers", "structured": { "locale": "zh-CN" } },
        "scope": { "level": "global" },
        "source": { "platform": "chatgpt", "origin": "import" },
        "tags": ["style"],
        "confidence": 0.9,
        "opaque": "AAEC",
    });
    let (put, _) = call(&mut ws, "mem.put", json!({"record": record})).await;
    assert_eq!(put.op, "mem.put.ok", "{:?}", payload(&put));
    let id = payload(&put)["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 26, "memory record id 必须是 ULID");
    assert_eq!(payload(&put)["opaque"], "AAEC");

    let (got, _) = call(&mut ws, "mem.get", json!({"id": id})).await;
    assert_eq!(got.op, "mem.get.ok");
    assert_eq!(payload(&got)["content"]["text"], "Prefers concise answers");

    let (search, _) = call(&mut ws, "mem.search", json!({"query": "concise"})).await;
    assert_eq!(search.op, "mem.search.ok");
    assert_eq!(payload(&search)["hits"][0]["record"]["id"], id);
    assert_eq!(payload(&search)["hits"][0]["score"], 0.9);

    let (list, _) = call(&mut ws, "mem.list", json!({"limit": 10})).await;
    assert_eq!(payload(&list)["records"].as_array().unwrap().len(), 1);

    let (export, _) = call(&mut ws, "mem.export", json!({"format": "map-json"})).await;
    assert_eq!(payload(&export)["records"].as_array().unwrap().len(), 1);

    let (import, _) = call(
        &mut ws,
        "mem.import",
        json!({
            "provider": "builtin",
            "format": "map-json",
            "records": [
                {
                    "provider": "builtin",
                    "kind": "fact",
                    "content": { "text": "Imported fact" },
                    "scope": { "level": "global" }
                },
                { "kind": "fact", "content": { "text": "" }, "scope": { "level": "global" } }
            ]
        }),
    )
    .await;
    assert_eq!(payload(&import)["imported"], 1);
    assert_eq!(payload(&import)["skipped"], 1);

    let (deleted, _) = call(&mut ws, "mem.delete", json!({"id": id})).await;
    assert_eq!(payload(&deleted)["deleted"], true);
    let (missing, _) = call(&mut ws, "mem.get", json!({"id": id})).await;
    assert_eq!(missing.op, "mem.get.err");
    assert_eq!(payload(&missing)["code"], "not_found");

    // 仅 mem.read 的令牌:可读不可写。
    let read_only = h.token_with(vec![Cap::MemRead]);
    let (mut reader, _) = connect(&h, "map.v1").await;
    let reply = hello(&mut reader, &read_only, "session.hello").await;
    assert_eq!(payload(&reply)["caps"], json!(["mem.read"]));
    let (allowed, _) = call(&mut reader, "mem.providers", json!({})).await;
    assert_eq!(allowed.op, "mem.providers.ok");
    let (denied, _) = call(&mut reader, "mem.put", json!({"record": record})).await;
    assert_eq!(denied.op, "mem.put.err");
    assert_eq!(payload(&denied)["code"], "forbidden");

    // 非法记录 → bad_request。
    let (invalid, _) = call(
        &mut ws,
        "mem.put",
        json!({"record": {"provider": "builtin", "kind": "fact", "content": {"text": ""}, "scope": {"level": "global"}}}),
    )
    .await;
    assert_eq!(invalid.op, "mem.put.err");
    assert_eq!(payload(&invalid)["code"], "bad_request");
}
