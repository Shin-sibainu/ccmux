//! IPC protocol for controlling a running ccmux instance from outside.
//!
//! Wire format: newline-delimited JSON. Each line is one [`Request`] from
//! client to server, followed by exactly one [`Response`] from server to
//! client. Connections are short-lived in the v1 protocol — clients open,
//! send one request, read one response, close.

use serde::{Deserialize, Serialize};

pub mod client;
pub mod endpoint;
pub mod server;

/// One IPC call from a client to the running ccmux instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// First message after connecting — exchanges PIDs and a session
    /// token so a stale socket file with a re-used PID cannot be
    /// silently mistaken for a live instance.
    Hello { client_pid: u32 },
    /// List all panes in the active workspace.
    List,
    /// Write `data` to the target pane's PTY. If `append_enter` is true,
    /// a newline is appended so the shell executes the command.
    Send {
        target: PaneRef,
        data: String,
        #[serde(default)]
        append_enter: bool,
    },
    /// Split the target pane and (optionally) run a command in the new
    /// pane. The new pane is named `id` if provided.
    Split {
        target: PaneRef,
        direction: Direction,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Move keyboard focus to the target pane.
    Focus { target: PaneRef },
    /// Create a new tab with a fresh single pane. The server switches
    /// focus to the new tab (matching the existing Alt+T keybinding).
    NewTab {
        /// Startup command for the new pane.
        #[serde(default)]
        command: Option<String>,
        /// Stable name to register for the new pane so it can be
        /// addressed via `PaneRef::Name` later.
        #[serde(default)]
        id: Option<String>,
        /// Override the tab label (otherwise derived from the cwd).
        #[serde(default)]
        label: Option<String>,
    },
}

/// Identifies a pane in a request. Names are user-friendly and stable
/// across splits; numeric ids are stable across the session but assigned
/// internally; `Focused` is "whichever pane is focused right now".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRef {
    Id(usize),
    Name(String),
    Focused,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Vertical,
    Horizontal,
}

/// One entry in the `List` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub focused: bool,
}

/// Server reply to one [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Successful call. `data` is request-specific (e.g. the pane list).
    Ok {
        #[serde(default)]
        data: serde_json::Value,
    },
    /// Hello reply: server identifies itself with PID and a session
    /// token derived from its start time, so the client can detect
    /// PID re-use from a previous crashed instance.
    Hello {
        server_pid: u32,
        session_token: String,
    },
    /// Server-side failure. `message` is human-readable and not
    /// machine-parseable; clients should match on `code` if added later.
    Err { message: String },
}

impl Response {
    pub fn ok_unit() -> Self {
        Response::Ok {
            data: serde_json::Value::Null,
        }
    }
    pub fn ok_value(value: serde_json::Value) -> Self {
        Response::Ok { data: value }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Response::Err {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(req: &Request) -> Request {
        let s = serde_json::to_string(req).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn list_request_roundtrips() {
        assert_eq!(roundtrip(&Request::List), Request::List);
    }

    #[test]
    fn send_request_roundtrips_with_enter() {
        let r = Request::Send {
            target: PaneRef::Name("engineering".into()),
            data: "hello".into(),
            append_enter: true,
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn split_request_roundtrips() {
        let r = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Vertical,
            command: Some("cce".into()),
            id: Some("engineering".into()),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn pane_ref_id_serializes_with_numeric() {
        let s = serde_json::to_string(&PaneRef::Id(7)).unwrap();
        assert!(s.contains("\"id\""), "{s}");
        assert!(s.contains("7"), "{s}");
    }

    #[test]
    fn pane_ref_focused_serializes_with_unit() {
        let s = serde_json::to_string(&PaneRef::Focused).unwrap();
        assert!(s.contains("focused"), "{s}");
    }

    #[test]
    fn unknown_command_fails_to_parse() {
        let bad = r#"{"cmd":"explode","target":{"focused":null}}"#;
        let parsed: Result<Request, _> = serde_json::from_str(bad);
        assert!(parsed.is_err());
    }

    #[test]
    fn response_err_serializes_message() {
        let r = Response::err("nope");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"err\""), "{s}");
        assert!(s.contains("nope"), "{s}");
    }

    #[test]
    fn response_ok_unit_has_null_data() {
        let r = Response::ok_unit();
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ok\""), "{s}");
        assert!(s.contains("null"), "{s}");
    }

    #[test]
    fn hello_request_carries_pid() {
        let r = Request::Hello { client_pid: 42 };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn new_tab_request_roundtrips() {
        let r = Request::NewTab {
            command: Some("cce".into()),
            id: Some("engineering".into()),
            label: Some("eng".into()),
        };
        assert_eq!(roundtrip(&r), r);
    }

    #[test]
    fn new_tab_request_defaults_all_fields() {
        let minimal = r#"{"cmd":"new_tab"}"#;
        let parsed: Request = serde_json::from_str(minimal).unwrap();
        match parsed {
            Request::NewTab {
                command: None,
                id: None,
                label: None,
            } => {}
            other => panic!("expected empty NewTab, got {other:?}"),
        }
    }

    #[test]
    fn hello_response_carries_token() {
        let r = Response::Hello {
            server_pid: 100,
            session_token: "abc".into(),
        };
        let parsed: Response = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(parsed, r);
    }
}
