//! Pure parts of the MCP server: `handle_basic` for every method of ARCHITECTURE §10.4
//! (including protocol version negotiation), the golden `tool_definitions`, `parse_tool_call`
//! for every tool, `reply_to_json`, `base64` (RFC 4648 vectors) and `Timeouts`.

mod mcp_support;

use lemma_agent::mcp::{
    self, COMPILE_GRACE, LATEST_PROTOCOL_VERSION, PROTOCOL_VERSIONS, RENDER_GRACE, SERVER_NAME,
    TOOL_NAMES, TOOL_TIMEOUT, Timeouts, VERSION, base64, handle_basic, parse_tool_call,
    reply_to_json, tool_definitions,
};
use lemma_agent::{EditorRequest, ToolContent, ToolReply};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/mcp_tool_definitions.json"
);

fn parse(line: &str) -> Value {
    serde_json::from_str(line).expect("valid JSON")
}

fn error_code(reply: &Value) -> i64 {
    reply["error"]["code"].as_i64().expect("an error response")
}

// ---------------------------------------------------------------------------------------------
// handle_basic
// ---------------------------------------------------------------------------------------------

#[test]
fn the_cli_2_1_288_handshake_gets_the_verified_replies() {
    let [discover, initialize, initialized, list] = mcp_support::cli_handshake();
    assert_eq!(
        handle_basic(&parse(discover)),
        Some(json!({"jsonrpc":"2.0","id":"server-discover-probe-1","result":{}}))
    );
    assert_eq!(
        handle_basic(&parse(initialize)),
        Some(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "lemma", "version": VERSION }
            }
        }))
    );
    assert_eq!(handle_basic(&parse(initialized)), None);
    assert_eq!(
        handle_basic(&parse(list)),
        Some(json!({"jsonrpc":"2.0","id":1,"result":{"tools": tool_definitions()}}))
    );
}

fn initialize(version: Value) -> Value {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "initialize",
        "params": {
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" }
        }
    });
    handle_basic(&msg).expect("initialize is answered")
}

#[test]
fn initialize_echoes_a_supported_version() {
    assert_eq!(PROTOCOL_VERSIONS, ["2025-11-25", "2025-06-18"]);
    for &version in PROTOCOL_VERSIONS {
        let reply = initialize(json!(version));
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["protocolVersion"], version);
        assert_eq!(reply["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(
            reply["result"]["serverInfo"],
            json!({ "name": SERVER_NAME, "version": VERSION })
        );
    }
}

#[test]
fn initialize_offers_the_latest_version_for_an_unsupported_one() {
    assert_eq!(LATEST_PROTOCOL_VERSION, "2025-11-25");
    for version in ["2024-11-05", "2025-03-26", "2026-07-28", "1.0", ""] {
        let reply = initialize(json!(version));
        assert_eq!(
            reply["result"]["protocolVersion"], "2025-11-25",
            "requested {version}"
        );
    }
}

#[test]
fn initialize_without_a_version_string_is_invalid_params() {
    for version in [json!(null), json!(20251125), json!(["2025-11-25"])] {
        let reply = initialize(version);
        assert_eq!(error_code(&reply), -32602);
        assert_eq!(reply["id"], 7);
    }
    let reply = handle_basic(&json!({"jsonrpc":"2.0","id":"x","method":"initialize"})).unwrap();
    assert_eq!(error_code(&reply), -32602);
    assert_eq!(reply["id"], "x");
}

#[test]
fn ping_and_server_discover_answer_an_empty_result() {
    for (id, method) in [
        (json!(0), "ping"),
        (json!("p"), "ping"),
        (json!(5), "server/discover"),
    ] {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        assert_eq!(
            handle_basic(&msg),
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))
        );
    }
}

#[test]
fn tools_list_returns_every_tool_and_ignores_a_cursor() {
    let reply = handle_basic(&parse(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"cursor":"abc"}}"#,
    ))
    .unwrap();
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOL_NAMES);
    assert!(reply["result"].get("nextCursor").is_none());
}

#[test]
fn notifications_and_responses_get_no_reply() {
    for msg in [
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"user"}}),
        json!({"jsonrpc":"2.0","method":"notifications/roots/list_changed"}),
        json!({"jsonrpc":"2.0","method":"some/unknown/notification"}),
        json!({"jsonrpc":"2.0","id":4,"result":{}}),
        json!({"jsonrpc":"2.0","id":4,"error":{"code":-1,"message":"no"}}),
    ] {
        assert_eq!(handle_basic(&msg), None, "{msg}");
    }
}

#[test]
fn unknown_methods_are_method_not_found() {
    for method in [
        "resources/list",
        "prompts/list",
        "logging/setLevel",
        "completion/complete",
        "frobnicate",
    ] {
        let reply = handle_basic(&json!({"jsonrpc":"2.0","id":"m1","method":method})).unwrap();
        assert_eq!(error_code(&reply), -32601, "{method}");
        assert_eq!(reply["id"], "m1");
        assert_eq!(reply["jsonrpc"], "2.0");
        assert!(reply["error"]["message"].as_str().unwrap().contains(method));
    }
}

#[test]
fn tools_call_is_not_handled_by_handle_basic() {
    let reply = handle_basic(&parse(&mcp_support::tool_call(9, "list_files", json!({})))).unwrap();
    assert_eq!(error_code(&reply), -32603);
    assert_eq!(reply["id"], 9);
}

#[test]
fn invalid_messages_are_invalid_requests() {
    let cases = [
        // (message, expected code, expected id)
        (
            json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]),
            -32600,
            json!(null),
        ),
        (json!("ping"), -32600, json!(null)),
        (json!(42), -32600, json!(null)),
        (json!({}), -32600, json!(null)),
        (json!({"id":1,"method":"ping"}), -32600, json!(1)),
        (
            json!({"jsonrpc":"1.0","id":1,"method":"ping"}),
            -32600,
            json!(1),
        ),
        (
            json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
            -32600,
            json!(null),
        ),
        (
            json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}),
            -32600,
            json!(null),
        ),
        (
            json!({"jsonrpc":"2.0","id":{"a":1},"method":"ping"}),
            -32600,
            json!(null),
        ),
        (
            json!({"jsonrpc":"2.0","id":true,"method":"ping"}),
            -32600,
            json!(null),
        ),
        (json!({"jsonrpc":"2.0","id":2,"method":7}), -32600, json!(2)),
        (json!({"jsonrpc":"2.0","id":2}), -32600, json!(2)),
        (
            json!({"jsonrpc":"2.0","id":3,"method":"ping","params":[1]}),
            -32602,
            json!(3),
        ),
        (
            json!({"jsonrpc":"2.0","id":3,"method":"ping","params":"x"}),
            -32602,
            json!(3),
        ),
    ];
    for (msg, code, id) in cases {
        let reply = handle_basic(&msg).unwrap_or_else(|| panic!("no reply to {msg}"));
        assert_eq!(error_code(&reply), code, "{msg}");
        assert_eq!(reply["id"], id, "{msg}");
        assert!(reply.get("result").is_none());
    }
    // Negative and very large integer ids are valid.
    for id in [json!(-1), json!(u64::MAX)] {
        let reply = handle_basic(&json!({"jsonrpc":"2.0","id":id,"method":"ping"})).unwrap();
        assert_eq!(reply["id"], id);
        assert_eq!(reply["result"], json!({}));
    }
}

// ---------------------------------------------------------------------------------------------
// tool_definitions
// ---------------------------------------------------------------------------------------------

#[test]
fn tool_definitions_match_the_golden_file() {
    let actual = tool_definitions();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let mut text = serde_json::to_string_pretty(&actual).unwrap();
        text.push('\n');
        std::fs::write(GOLDEN, text).expect("write the golden file");
        return;
    }
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(GOLDEN).expect("read tests/mcp_tool_definitions.json"),
    )
    .expect("golden JSON");
    if actual != golden {
        let a = actual.as_array().unwrap();
        let g = golden.as_array().unwrap();
        for (x, y) in a.iter().zip(g) {
            assert_eq!(x, y, "tool {} differs from the golden file", x["name"]);
        }
        panic!(
            "tool_definitions has {} tools, the golden file {} (UPDATE_GOLDEN=1 rewrites it)",
            a.len(),
            g.len()
        );
    }
}

fn tool(name: &str) -> Value {
    tool_definitions()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
        .clone()
}

fn properties(name: &str) -> Vec<String> {
    let mut keys: Vec<String> = tool(name)["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn every_tool_is_well_formed_and_mentions_the_review() {
    let tools = tool_definitions();
    let tools = tools.as_array().unwrap();
    assert_eq!(tools.len(), 10);
    for t in tools {
        let name = t["name"].as_str().unwrap();
        assert!(TOOL_NAMES.contains(&name));
        assert!(!t["title"].as_str().unwrap().is_empty(), "{name}");
        let description = t["description"].as_str().unwrap();
        assert!(
            description.contains("red/green hunks"),
            "{name}: the description must say the user reviews changes as red/green hunks"
        );
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}");
        assert_eq!(schema["additionalProperties"], false, "{name}");
        let props = schema["properties"].as_object().unwrap();
        for (prop, spec) in props {
            assert!(spec["type"].is_string(), "{name}.{prop}");
            assert!(spec["description"].is_string(), "{name}.{prop}");
        }
        for required in schema["required"].as_array().into_iter().flatten() {
            assert!(props.contains_key(required.as_str().unwrap()), "{name}");
        }
        assert!(t["annotations"].is_object(), "{name}");
    }
}

#[test]
fn file_tools_use_the_argument_names_of_claudes_builtins() {
    assert_eq!(properties("read_file"), ["file_path", "limit", "offset"]);
    assert_eq!(
        tool("read_file")["inputSchema"]["required"],
        json!(["file_path"])
    );
    assert_eq!(
        properties("edit"),
        ["file_path", "new_string", "old_string", "replace_all"]
    );
    assert_eq!(
        tool("edit")["inputSchema"]["required"],
        json!(["file_path", "old_string", "new_string"])
    );
    assert_eq!(properties("write"), ["content", "file_path"]);
    assert_eq!(
        tool("write")["inputSchema"]["required"],
        json!(["file_path", "content"])
    );
    assert_eq!(properties("outline"), ["file_path"]);
    assert_eq!(properties("get_diagnostics"), ["file_path"]);
    assert_eq!(properties("render_page"), ["page"]);
    for name in ["list_files", "compile", "get_selection", "get_review_state"] {
        assert!(properties(name).is_empty(), "{name}");
    }
}

#[test]
fn only_tools_that_change_nothing_are_marked_read_only() {
    for name in TOOL_NAMES {
        let read_only = tool(name)["annotations"]["readOnlyHint"].as_bool().unwrap();
        let expected = !matches!(name, "edit" | "write" | "compile");
        assert_eq!(read_only, expected, "{name}");
    }
}

#[test]
fn every_schema_property_is_accepted_by_parse_tool_call() {
    // A value of the declared type for each property, so the schema and the parser agree.
    for name in TOOL_NAMES {
        let t = tool(name);
        let mut args = serde_json::Map::new();
        for (prop, spec) in t["inputSchema"]["properties"].as_object().unwrap() {
            let value = match spec["type"].as_str().unwrap() {
                "string" => json!("main.tex"),
                "integer" => json!(3),
                "boolean" => json!(true),
                other => panic!("unexpected type {other}"),
            };
            args.insert(prop.clone(), value);
        }
        parse_tool_call(name, &Value::Object(args))
            .unwrap_or_else(|e| panic!("{name} rejects its own schema: {e}"));
    }
}

// ---------------------------------------------------------------------------------------------
// parse_tool_call
// ---------------------------------------------------------------------------------------------

fn err(name: &str, args: Value) -> String {
    match parse_tool_call(name, &args) {
        Err(e) => e,
        Ok(req) => panic!("{name} {args} parsed as {req:?}"),
    }
}

fn ok(name: &str, args: Value) -> EditorRequest {
    parse_tool_call(name, &args).unwrap_or_else(|e| panic!("{name} {args}: {e}"))
}

#[test]
fn read_file_arguments() {
    assert_eq!(
        ok(
            "read_file",
            json!({"file_path": "/p/main.tex", "offset": 10, "limit": 20})
        ),
        EditorRequest::ReadFile {
            path: PathBuf::from("/p/main.tex"),
            offset: Some(10),
            limit: Some(20),
        }
    );
    assert_eq!(
        ok("read_file", json!({"file_path": "chapters/yoneda.tex"})),
        EditorRequest::ReadFile {
            path: PathBuf::from("chapters/yoneda.tex"),
            offset: None,
            limit: None,
        }
    );
    // Lenient where unambiguous: null optionals, integral floats, decimal strings, offset 0.
    assert_eq!(
        ok(
            "read_file",
            json!({"file_path": "a.tex", "offset": null, "limit": "40"})
        ),
        EditorRequest::ReadFile {
            path: PathBuf::from("a.tex"),
            offset: None,
            limit: Some(40),
        }
    );
    assert_eq!(
        ok("read_file", json!({"file_path": "a.tex", "offset": 5.0})),
        EditorRequest::ReadFile {
            path: PathBuf::from("a.tex"),
            offset: Some(5),
            limit: None,
        }
    );
    assert!(matches!(
        ok("read_file", json!({"file_path": "a.tex", "offset": 0})),
        EditorRequest::ReadFile {
            offset: Some(0),
            ..
        }
    ));
    assert!(matches!(
        ok(
            "read_file",
            json!({"file_path": "a.tex", "limit": u32::MAX})
        ),
        EditorRequest::ReadFile {
            limit: Some(u32::MAX),
            ..
        }
    ));

    assert_eq!(
        err("read_file", json!({})),
        "read_file: missing required argument \"file_path\""
    );
    assert_eq!(
        err("read_file", json!(null)),
        "read_file: missing required argument \"file_path\""
    );
    assert_eq!(
        err("read_file", json!({"file_path": 12})),
        "read_file: argument \"file_path\" must be a string, got 12"
    );
    assert_eq!(
        err("read_file", json!({"file_path": ""})),
        "read_file: argument \"file_path\" is empty"
    );
    assert_eq!(
        err("read_file", json!({"file_path": "a\u{0}b"})),
        "read_file: argument \"file_path\" contains a NUL character"
    );
    assert_eq!(
        err("read_file", json!({"file_path": "a.tex", "offset": "ten"})),
        "read_file: argument \"offset\" must be a non-negative integer (a 1-based line number), \
         got \"ten\""
    );
    for bad in [
        json!(-1),
        json!(1.5),
        json!(true),
        json!([1]),
        json!(4294967296u64),
    ] {
        let e = err("read_file", json!({"file_path": "a.tex", "offset": bad}));
        assert!(
            e.starts_with("read_file: argument \"offset\" must be"),
            "{e}"
        );
        assert!(e.ends_with(&format!("got {bad}")), "{e}");
    }
    assert_eq!(
        err("read_file", json!({"file_path": "a.tex", "limit": 0})),
        "read_file: argument \"limit\" must be an integer of at least 1 (a number of lines), got 0"
    );
    assert_eq!(
        err("read_file", json!({"file_path": "a.tex", "path": "a.tex"})),
        "read_file: unexpected argument \"path\"; read_file takes file_path, offset, limit"
    );
    assert_eq!(
        err("read_file", json!(["a.tex"])),
        "read_file: arguments must be a JSON object, got [\"a.tex\"]"
    );
    assert_eq!(
        err("read_file", json!("a.tex")),
        "read_file: arguments must be a JSON object, got \"a.tex\""
    );
}

#[test]
fn edit_arguments() {
    let full = json!({
        "file_path": "main.tex",
        "old_string": "\\section{Intro}",
        "new_string": "\\section{Introduction}",
        "replace_all": true
    });
    assert_eq!(
        ok("edit", full),
        EditorRequest::Edit {
            path: PathBuf::from("main.tex"),
            old_string: "\\section{Intro}".into(),
            new_string: "\\section{Introduction}".into(),
            replace_all: true,
        }
    );
    let minimal = json!({"file_path": "main.tex", "old_string": "a", "new_string": ""});
    assert_eq!(
        ok("edit", minimal),
        EditorRequest::Edit {
            path: PathBuf::from("main.tex"),
            old_string: "a".into(),
            new_string: String::new(),
            replace_all: false,
        }
    );
    for (value, expected) in [
        (json!(false), false),
        (json!("true"), true),
        (json!("False"), false),
        (json!(null), false),
    ] {
        let args =
            json!({"file_path":"m.tex","old_string":"a","new_string":"b","replace_all":value});
        assert!(
            matches!(ok("edit", args), EditorRequest::Edit { replace_all, .. } if replace_all == expected),
            "{value}"
        );
    }

    assert_eq!(
        err("edit", json!({"file_path": "m.tex", "new_string": "b"})),
        "edit: missing required argument \"old_string\""
    );
    assert_eq!(
        err(
            "edit",
            json!({"file_path": "m.tex", "old_string": "a", "new_string": null})
        ),
        "edit: missing required argument \"new_string\""
    );
    assert_eq!(
        err("edit", json!({"old_string": "a", "new_string": "b"})),
        "edit: missing required argument \"file_path\""
    );
    assert_eq!(
        err(
            "edit",
            json!({"file_path": "m.tex", "old_string": 1, "new_string": "b"})
        ),
        "edit: argument \"old_string\" must be a string, got 1"
    );
    assert_eq!(
        err(
            "edit",
            json!({"file_path":"m.tex","old_string":"a","new_string":"b","replace_all":1})
        ),
        "edit: argument \"replace_all\" must be true or false, got 1"
    );
    assert_eq!(
        err(
            "edit",
            json!({"file_path":"m.tex","old_str":"a","new_str":"b"})
        ),
        "edit: unexpected arguments \"new_str\", \"old_str\"; edit takes file_path, old_string, \
         new_string, replace_all"
    );
}

#[test]
fn write_arguments() {
    assert_eq!(
        ok(
            "write",
            json!({"file_path": "new/notes.tex", "content": "x\ny\n"})
        ),
        EditorRequest::Write {
            path: PathBuf::from("new/notes.tex"),
            content: "x\ny\n".into(),
        }
    );
    assert_eq!(
        ok("write", json!({"file_path": "empty.tex", "content": ""})),
        EditorRequest::Write {
            path: PathBuf::from("empty.tex"),
            content: String::new(),
        }
    );
    assert_eq!(
        err("write", json!({"file_path": "a.tex"})),
        "write: missing required argument \"content\""
    );
    assert_eq!(
        err(
            "write",
            json!({"file_path": "a.tex", "content": {"text": "x"}})
        ),
        "write: argument \"content\" must be a string, got {\"text\":\"x\"}"
    );
    assert_eq!(
        err(
            "write",
            json!({"file_path": "a.tex", "content": "x", "mode": "append"})
        ),
        "write: unexpected argument \"mode\"; write takes file_path, content"
    );
}

#[test]
fn tools_without_arguments() {
    for (name, expected) in [
        ("list_files", EditorRequest::ListFiles),
        ("compile", EditorRequest::Compile),
        ("get_selection", EditorRequest::Selection),
        ("get_review_state", EditorRequest::ReviewState),
    ] {
        assert_eq!(ok(name, json!({})), expected);
        assert_eq!(ok(name, json!(null)), expected);
        assert_eq!(
            err(name, json!({"verbose": true})),
            format!("{name} takes no arguments, got \"verbose\"")
        );
        assert_eq!(
            err(name, json!(true)),
            format!("{name}: arguments must be a JSON object, got true")
        );
    }
}

#[test]
fn optional_path_tools() {
    assert_eq!(
        ok("outline", json!({})),
        EditorRequest::Outline { path: None }
    );
    assert_eq!(
        ok("outline", json!({"file_path": null})),
        EditorRequest::Outline { path: None }
    );
    assert_eq!(
        ok("outline", json!({"file_path": "/abs/main.tex"})),
        EditorRequest::Outline {
            path: Some(PathBuf::from("/abs/main.tex"))
        }
    );
    assert_eq!(
        ok("get_diagnostics", json!({})),
        EditorRequest::Diagnostics { path: None }
    );
    assert_eq!(
        ok("get_diagnostics", json!({"file_path": "main.tex"})),
        EditorRequest::Diagnostics {
            path: Some(PathBuf::from("main.tex"))
        }
    );
    assert_eq!(
        err("outline", json!({"file_path": ["main.tex"]})),
        "outline: argument \"file_path\" must be a string (a file path), got [\"main.tex\"]"
    );
    assert_eq!(
        err("get_diagnostics", json!({"file_path": ""})),
        "get_diagnostics: argument \"file_path\" is empty"
    );
    assert_eq!(
        err("get_diagnostics", json!({"severity": "error"})),
        "get_diagnostics: unexpected argument \"severity\"; get_diagnostics takes file_path"
    );
}

#[test]
fn render_page_arguments() {
    assert_eq!(
        ok("render_page", json!({"page": 3})),
        EditorRequest::RenderPage { page: 3 }
    );
    assert_eq!(
        ok("render_page", json!({"page": "12"})),
        EditorRequest::RenderPage { page: 12 }
    );
    assert_eq!(
        err("render_page", json!({})),
        "render_page: missing required argument \"page\""
    );
    assert_eq!(
        err("render_page", json!({"page": 0})),
        "render_page: argument \"page\" must be an integer of at least 1 (a 1-based page \
         number), got 0"
    );
    for bad in [json!(-2), json!(2.5), json!("two"), json!(false)] {
        let e = err("render_page", json!({"page": bad}));
        assert!(
            e.starts_with("render_page: argument \"page\" must be an integer of at least 1"),
            "{e}"
        );
    }
    assert_eq!(
        err("render_page", json!({"page": 1, "scale": 2})),
        "render_page: unexpected argument \"scale\"; render_page takes page"
    );
}

#[test]
fn unknown_tools_are_named_with_the_list_of_tools() {
    let e = err("Edit", json!({}));
    assert_eq!(
        e,
        "unknown tool \"Edit\"; the tools are read_file, edit, write, list_files, outline, \
         compile, get_diagnostics, get_selection, render_page, get_review_state"
    );
    assert!(err("mcp__lemma__edit", json!({})).starts_with("unknown tool"));
}

#[test]
fn long_values_are_abbreviated_in_errors() {
    let long = "x".repeat(500);
    let e = err("read_file", json!({"file_path": "a.tex", "offset": long}));
    assert!(e.len() < 200, "{e}");
    assert!(e.ends_with('…'), "{e}");
    // Abbreviation never splits a UTF-8 character.
    let e = err(
        "read_file",
        json!({"file_path": "a.tex", "offset": "ℤ".repeat(100)}),
    );
    assert!(e.ends_with('…'), "{e}");
}

// ---------------------------------------------------------------------------------------------
// reply_to_json and base64
// ---------------------------------------------------------------------------------------------

#[test]
fn text_replies() {
    let reply = ToolReply {
        content: vec![ToolContent::Text("Edited main.tex: lines 3–4".into())],
        is_error: false,
    };
    assert_eq!(
        reply_to_json(&reply),
        json!({"content":[{"type":"text","text":"Edited main.tex: lines 3–4"}],"isError":false})
    );
    let error = ToolReply {
        content: vec![ToolContent::Text("path outside project root".into())],
        is_error: true,
    };
    assert_eq!(
        reply_to_json(&error),
        json!({"content":[{"type":"text","text":"path outside project root"}],"isError":true})
    );
    let empty = ToolReply {
        content: vec![],
        is_error: false,
    };
    assert_eq!(reply_to_json(&empty), json!({"content":[],"isError":false}));
}

#[test]
fn image_replies_are_base64_png_in_order() {
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    let reply = ToolReply {
        content: vec![
            ToolContent::Png(png.clone()),
            ToolContent::Text("PNG copy: /run/user/1000/lemma/page-2.png".into()),
        ],
        is_error: false,
    };
    let v = reply_to_json(&reply);
    assert_eq!(
        v,
        json!({
            "content": [
                {"type": "image", "data": "iVBORw0KGgoAAAANSUhEUg==", "mimeType": "image/png"},
                {"type": "text", "text": "PNG copy: /run/user/1000/lemma/page-2.png"}
            ],
            "isError": false
        })
    );
    assert_eq!(
        decode_base64(v["content"][0]["data"].as_str().unwrap()),
        png
    );
}

#[test]
fn base64_matches_rfc_4648_test_vectors() {
    // RFC 4648 §10.
    for (input, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
    }
    // The last two symbols of the standard (not URL-safe) alphabet.
    assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
    assert_eq!(base64(&[0, 0, 0]), "AAAA");
    assert_eq!(base64(&[0x14, 0xfb, 0x9c, 0x03, 0xd9, 0x7e]), "FPucA9l+");
}

/// A straightforward decoder for checking round trips.
fn decode_base64(s: &str) -> Vec<u8> {
    let value = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {c}"),
        }
    };
    assert_eq!(s.len() % 4, 0);
    let mut out = Vec::new();
    for quad in s.as_bytes().chunks(4) {
        let pad = quad.iter().filter(|&&c| c == b'=').count();
        let n = quad[..4 - pad]
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &c)| n | value(c) << (18 - 6 * i));
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    out
}

#[test]
fn base64_round_trips_every_byte_and_length() {
    let all: Vec<u8> = (0..=255u8).collect();
    assert_eq!(decode_base64(&base64(&all)), all);
    // Deterministic pseudo-random data of every length up to 300.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for len in 0..300usize {
        let data: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let encoded = base64(&data);
        assert_eq!(encoded.len(), len.div_ceil(3) * 4);
        assert!(
            encoded
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
        );
        assert_eq!(decode_base64(&encoded), data, "length {len}");
    }
}

// ---------------------------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------------------------

#[test]
fn compile_gets_the_build_timeout_plus_twenty_seconds_and_render_page_ten_more() {
    assert_eq!(TOOL_TIMEOUT, Duration::from_secs(30));
    assert_eq!(COMPILE_GRACE, Duration::from_secs(20));
    let default = Timeouts::default();
    assert_eq!(default.tool, Duration::from_secs(30));
    assert_eq!(default.compile, Duration::from_secs(200));
    let t = Timeouts::for_build_timeout(Duration::from_secs(60));
    assert_eq!(t.compile, Duration::from_secs(80));
    assert_eq!(
        t.for_request(&EditorRequest::Compile),
        Duration::from_secs(80)
    );
    for req in [
        EditorRequest::ListFiles,
        EditorRequest::Write {
            path: "a".into(),
            content: String::new(),
        },
    ] {
        assert_eq!(t.for_request(&req), Duration::from_secs(30));
    }
    // render_page may first wait for a running build as long as compile, then renders.
    assert_eq!(RENDER_GRACE, Duration::from_secs(10));
    assert_eq!(
        t.for_request(&EditorRequest::RenderPage { page: 1 }),
        Duration::from_secs(90)
    );
    assert_eq!(
        default.for_request(&EditorRequest::RenderPage { page: 1 }),
        Duration::from_secs(210)
    );
    let short = Timeouts {
        tool: Duration::from_secs(60),
        compile: Duration::from_secs(5),
    };
    assert_eq!(
        short.for_request(&EditorRequest::RenderPage { page: 1 }),
        Duration::from_secs(60),
        "never less than the tool timeout"
    );
    assert_eq!(
        Timeouts::for_build_timeout(Duration::MAX).compile,
        Duration::MAX
    );
    assert_eq!(mcp::TIMEOUT_TEXT, "editor did not answer in time");
}
