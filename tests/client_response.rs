#![cfg(feature = "remote")]

use leviathan::http;
use serde_json::{Value, json};

fn call_with_messages(messages: Vec<Value>, sse: bool) -> anyhow::Result<String> {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", server.server_addr());
    let worker = std::thread::spawn(move || {
        let request = server.recv().unwrap();
        let (body, content_type) = if sse {
            (messages.iter().map(|m| format!("data: {m}\n\n")).collect::<String>(), "text/event-stream")
        } else {
            (messages[0].to_string(), "application/json")
        };
        request
            .respond(
                tiny_http::Response::from_string(body)
                    .with_header(tiny_http::Header::from_bytes("Content-Type", content_type).unwrap()),
            )
            .unwrap();
    });
    let result = http::call_tool(&url, None, "recall", &json!({}));
    worker.join().unwrap();
    result
}

fn response() -> Value {
    json!({"jsonrpc":"2.0","id":1,"result":{
        "content":[{"type":"text","text":"the requested result"}]
    }})
}

#[test]
fn remote_tool_selects_its_response_after_a_server_request() {
    for server_id in [1, 27] {
        let request = json!({"jsonrpc":"2.0","id":server_id,"method":"ping"});
        assert_eq!(call_with_messages(vec![request, response()], true).unwrap(), "the requested result");
    }
}

#[test]
fn remote_tool_reports_missing_response_instead_of_accepting_a_request() {
    let request = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
    assert!(call_with_messages(vec![request], true).is_err());
}

#[test]
fn remote_tool_still_accepts_a_plain_json_response() {
    assert_eq!(call_with_messages(vec![response()], false).unwrap(), "the requested result");
}
