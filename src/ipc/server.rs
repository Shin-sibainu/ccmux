//! IPC server: accepts connections on a named endpoint and forwards
//! each request to the App's command channel.
//!
//! Wire protocol: newline-delimited JSON. A connection must start with
//! a `Hello` request; the server replies with a [`Response::Hello`]
//! carrying its PID and a per-instance session token. The client then
//! sends exactly one command and reads exactly one response before the
//! server closes its side.
//!
//! Threading model:
//! - One accept thread lives for the process lifetime and blocks on
//!   `listener.incoming()`.
//! - Each connection is handed to a short-lived worker thread so a slow
//!   client can't starve the accept loop.
//! - Workers communicate with the App by pushing an [`AppCommand`] into
//!   the shared `Sender<AppCommand>` and blocking on a [`oneshot`] reply
//!   with a timeout, so an unresponsive App can never hang a worker
//!   indefinitely.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::{prelude::*, ListenerOptions, Stream};

use super::endpoint::EndpointName;
use super::{Direction, PaneRef, Request, Response};
use crate::app::AppCommand;

/// How long a worker waits for the App's event loop to process one
/// command before giving up. The App drains commands every frame
/// (~30Hz), so 5s is orders of magnitude more than the expected latency
/// — a timeout here means the App is genuinely wedged.
const APP_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct IpcServer {
    pub endpoint: EndpointName,
    pub session_token: String,
    // We intentionally don't hold a JoinHandle. The accept thread lives
    // until the process exits, at which point the OS reclaims the pipe
    // or socket. Orderly shutdown is not needed in v1.
}

impl IpcServer {
    /// Bind the listener and start accepting in a background thread.
    pub fn spawn(
        endpoint: EndpointName,
        command_tx: Sender<AppCommand>,
        session_token: String,
    ) -> Result<Self> {
        let listener = bind_listener(&endpoint)
            .with_context(|| format!("bind IPC endpoint {}", endpoint.as_str()))?;

        let token_for_thread = session_token.clone();
        let endpoint_for_log = endpoint.as_str().to_string();
        thread::Builder::new()
            .name("ccmux-ipc-accept".into())
            .spawn(move || {
                accept_loop(listener, command_tx, token_for_thread, endpoint_for_log);
            })
            .context("spawn IPC accept thread")?;

        Ok(Self {
            endpoint,
            session_token,
        })
    }
}

fn bind_listener(endpoint: &EndpointName) -> Result<interprocess::local_socket::Listener> {
    // `to_fs_name` lets us pass an OS-native path (both the Windows
    // pipe name `\\.\pipe\…` and a Unix socket path are absolute file
    // names). `try_overwrite(true)` replaces a stale Unix socket file
    // left behind by a crashed previous instance — on Windows the
    // equivalent is a no-op because Named Pipes don't leak files.
    #[cfg(windows)]
    let name = {
        use interprocess::os::windows::local_socket::NamedPipe;
        endpoint.as_str().to_fs_name::<NamedPipe>()?
    };
    #[cfg(unix)]
    let name = {
        use interprocess::local_socket::GenericFilePath;
        endpoint.as_str().to_fs_name::<GenericFilePath>()?
    };

    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    Ok(listener)
}

fn accept_loop(
    listener: interprocess::local_socket::Listener,
    command_tx: Sender<AppCommand>,
    session_token: String,
    endpoint_for_log: String,
) {
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                // A single failed accept shouldn't kill the server —
                // the OS can recover from transient conditions.
                eprintln!("ccmux IPC: accept failed on {endpoint_for_log}: {e}");
                continue;
            }
        };

        let tx = command_tx.clone();
        let token = session_token.clone();
        if let Err(e) = thread::Builder::new()
            .name("ccmux-ipc-worker".into())
            .spawn(move || {
                if let Err(e) = handle_connection(conn, tx, &token) {
                    eprintln!("ccmux IPC: connection error: {e}");
                }
            })
        {
            // Thread spawn failures are extremely rare (EAGAIN under
            // system pressure). Dropping the connection is safe — the
            // client sees EOF and can retry. We deliberately don't fall
            // back to inline handling because that would block the
            // accept loop behind a slow request.
            eprintln!("ccmux IPC: worker spawn failed, dropping connection: {e}");
        }
    }
}

fn handle_connection(
    conn: Stream,
    command_tx: Sender<AppCommand>,
    session_token: &str,
) -> Result<()> {
    // The stream is split by wrapping in BufReader for line-buffered
    // reads; writes go through BufReader::get_mut. We can't construct
    // two BufReader clones without a split, so we borrow mutably.
    let mut reader = BufReader::new(conn);
    let mut line = String::new();

    // ── 1. Handshake ───────────────────────────────────────
    if read_line_or_eof(&mut reader, &mut line)?.is_none() {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return write_response_line(
                reader.get_mut(),
                &Response::err(format!("parse error on hello: {e}")),
            );
        }
    };
    match req {
        Request::Hello { client_pid: _ } => {
            let hello = Response::Hello {
                server_pid: std::process::id(),
                session_token: session_token.to_string(),
            };
            write_response_line(reader.get_mut(), &hello)?;
        }
        _ => {
            write_response_line(
                reader.get_mut(),
                &Response::err("first message must be hello"),
            )?;
            return Ok(());
        }
    }

    // ── 2. One command ─────────────────────────────────────
    line.clear();
    if read_line_or_eof(&mut reader, &mut line)?.is_none() {
        return Ok(());
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return write_response_line(
                reader.get_mut(),
                &Response::err(format!("parse error: {e}")),
            );
        }
    };
    let resp = dispatch_request(req, &command_tx);
    write_response_line(reader.get_mut(), &resp)?;
    Ok(())
}

fn read_line_or_eof<R: BufRead>(reader: &mut R, buf: &mut String) -> Result<Option<()>> {
    let n = reader.read_line(buf)?;
    Ok(if n == 0 { None } else { Some(()) })
}

fn write_response_line<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    let mut json = serde_json::to_string(resp)?;
    json.push('\n');
    w.write_all(json.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn dispatch_request(req: Request, command_tx: &Sender<AppCommand>) -> Response {
    match req {
        Request::Hello { .. } => Response::err("unexpected duplicate hello"),
        Request::List => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::List { reply: reply_tx })
                .is_err()
            {
                return Response::err("app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(list) => match serde_json::to_value(&list) {
                    Ok(v) => Response::ok_value(v),
                    Err(e) => Response::err(format!("serialize pane list: {e}")),
                },
                Err(e) => Response::err(format!("app did not respond: {e}")),
            }
        }
        Request::Send {
            target,
            data,
            append_enter,
        } => forward_unit(command_tx, |reply| AppCommand::Send {
            target,
            data: data.into_bytes(),
            append_enter,
            reply,
        }),
        Request::Focus { target } => {
            forward_unit(command_tx, |reply| AppCommand::Focus { target, reply })
        }
        Request::Split {
            target,
            direction,
            command,
            id,
        } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::Split {
                    target,
                    direction,
                    command,
                    name: id,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err("app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(new_id)) => Response::ok_value(serde_json::json!({ "id": new_id })),
                Ok(Err(msg)) => Response::err(msg),
                Err(e) => Response::err(format!("app did not respond: {e}")),
            }
        }
        Request::NewTab { command, id, label } => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if command_tx
                .send(AppCommand::NewTab {
                    command,
                    name: id,
                    label,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Response::err("app shutting down");
            }
            match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
                Ok(Ok(new_id)) => Response::ok_value(serde_json::json!({ "id": new_id })),
                Ok(Err(msg)) => Response::err(msg),
                Err(e) => Response::err(format!("app did not respond: {e}")),
            }
        }
    }
}

/// Forward a command whose success result is `()` and translate the
/// reply into a [`Response`]. Factored out because three of the four
/// variants share this exact shape.
fn forward_unit(
    command_tx: &Sender<AppCommand>,
    build: impl FnOnce(oneshot::Sender<std::result::Result<(), String>>) -> AppCommand,
) -> Response {
    let (reply_tx, reply_rx) = oneshot::channel();
    if command_tx.send(build(reply_tx)).is_err() {
        return Response::err("app shutting down");
    }
    match reply_rx.recv_timeout(APP_REPLY_TIMEOUT) {
        Ok(Ok(_)) => Response::ok_unit(),
        Ok(Err(msg)) => Response::err(msg),
        Err(e) => Response::err(format!("app did not respond: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Request;
    use std::sync::mpsc;

    #[test]
    fn dispatch_list_ok_when_app_replies() {
        // Pretend to be the App: spawn a thread that pulls a List
        // command off the channel and replies with an empty list.
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::List { reply }) = rx.recv() {
                reply.send(Vec::new()).unwrap();
            }
        });

        let resp = dispatch_request(Request::List, &tx);
        handle.join().unwrap();

        match resp {
            Response::Ok { data } => {
                // An empty Vec<PaneInfo> serializes to a JSON array.
                assert!(data.is_array(), "expected array, got {data:?}");
                assert_eq!(data.as_array().map(|a| a.len()), Some(0));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_focus_ok_when_app_replies_ok() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply.send(Ok(())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Focused,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_focus_err_when_app_replies_err() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Focus { reply, .. }) = rx.recv() {
                reply.send(Err("pane not found".into())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Focus {
                target: PaneRef::Id(999),
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Err { message } => assert!(message.contains("pane not found")),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_split_returns_new_id() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Split { reply, .. }) = rx.recv() {
                reply.send(Ok(42)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Split {
                target: PaneRef::Focused,
                direction: Direction::Vertical,
                command: None,
                id: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(42));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_send_forwards_data_and_enter() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::Send {
                data,
                append_enter,
                reply,
                ..
            }) = rx.recv()
            {
                assert_eq!(data, b"hello");
                assert!(append_enter);
                reply.send(Ok(())).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::Send {
                target: PaneRef::Name("engineering".into()),
                data: "hello".into(),
                append_enter: true,
            },
            &tx,
        );
        handle.join().unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_new_tab_returns_new_id() {
        let (tx, rx) = mpsc::channel::<AppCommand>();
        let handle = thread::spawn(move || {
            if let Ok(AppCommand::NewTab { reply, .. }) = rx.recv() {
                reply.send(Ok(11)).unwrap();
            }
        });
        let resp = dispatch_request(
            Request::NewTab {
                command: Some("cce".into()),
                id: Some("engineering".into()),
                label: None,
            },
            &tx,
        );
        handle.join().unwrap();
        match resp {
            Response::Ok { data } => {
                assert_eq!(data.get("id").and_then(|v| v.as_u64()), Some(11));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_refuses_second_hello() {
        // Duplicate hello after handshake should be an error path.
        let (tx, _rx) = mpsc::channel::<AppCommand>();
        let resp = dispatch_request(Request::Hello { client_pid: 1 }, &tx);
        match resp {
            Response::Err { message } => assert!(message.contains("hello")),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
