#![cfg(feature = "remote")]

use leviathan::http;
use serde_json::json;

#[test]
fn remote_tool_accepts_a_leading_sse_bom_without_changing_text() {
    for prefix in ["", "\u{feff}"] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            let reply = json!({"jsonrpc":"2.0","id":1,"result":{
                "content":[{"type":"text","text":"result \u{feff} preserved"}]
            }});
            let body = format!("{prefix}data: {reply}\n\n");
            let response = tiny_http::Response::from_string(body)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap());
            request.respond(response).unwrap();
        });
        let result = http::call_tool(&url, None, "recall", &json!({}));
        worker.join().unwrap();
        assert_eq!(result.unwrap(), "result \u{feff} preserved", "prefix {prefix:?}");
    }
}
