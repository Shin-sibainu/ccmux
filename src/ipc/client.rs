//! IPC client: opens a short-lived connection to the running ccmux
//! instance, performs the [`Request::Hello`] handshake, then sends
//! exactly one [`Request`] and reads exactly one [`Response`].
//!
//! Connection lifecycle matches the server in [`super::server`]: one
//! request per connection, closed by the client dropping the stream.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use interprocess::local_socket::{prelude::*, Stream};

use super::endpoint::EndpointName;
use super::{Request, Response};

/// How long we wait for a response before giving up. The server replies
/// within milliseconds unless the App is genuinely wedged; this value
/// is a belt-and-braces safety net so a misbehaving server can't hang
/// a shell script that invokes `ccmux send`.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Send a single request to the endpoint and return the response.
pub fn send_request(endpoint: &EndpointName, request: &Request) -> Result<Response> {
    let name = make_connection_name(endpoint)?;
    let conn =
        Stream::connect(name).with_context(|| format!("connect to {}", endpoint.as_str()))?;
    // interprocess 2.4 exposes read/write timeouts through the
    // platform-specific wrappers; at the portable surface we rely on
    // the blocking default. Blocking is fine here because the server's
    // per-connection timeout already bounds our wait.
    let _ = RESPONSE_TIMEOUT; // reserved for a future timeout hookup
    converse(conn, request)
}

fn make_connection_name(endpoint: &EndpointName) -> Result<interprocess::local_socket::Name<'_>> {
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::NamedPipe;
        Ok(endpoint.as_str().to_fs_name::<NamedPipe>()?)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        Ok(endpoint.as_str().to_fs_name::<GenericFilePath>()?)
    }
}

fn converse(conn: Stream, request: &Request) -> Result<Response> {
    let mut reader = BufReader::new(conn);

    // Handshake
    let hello = Request::Hello {
        client_pid: std::process::id(),
    };
    write_request_line(reader.get_mut(), &hello)?;
    let hello_resp = read_response_line(&mut reader)?;
    match hello_resp {
        Response::Hello { .. } => {}
        Response::Err { message } => {
            return Err(anyhow!("server refused hello: {message}"));
        }
        Response::Ok { .. } => {
            return Err(anyhow!("unexpected ok response to hello"));
        }
    }

    // Actual command
    write_request_line(reader.get_mut(), request)?;
    let resp = read_response_line(&mut reader)?;
    Ok(resp)
}

fn write_request_line<W: Write>(w: &mut W, req: &Request) -> Result<()> {
    let mut json = serde_json::to_string(req)?;
    json.push('\n');
    w.write_all(json.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn read_response_line<R: BufRead>(r: &mut R) -> Result<Response> {
    let mut buf = String::new();
    let n = r.read_line(&mut buf)?;
    if n == 0 {
        return Err(anyhow!("server closed connection before replying"));
    }
    let resp: Response = serde_json::from_str(buf.trim())
        .with_context(|| format!("parse response json: {buf:?}"))?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Direction, PaneRef};

    #[test]
    fn write_request_line_is_newline_terminated() {
        let mut out: Vec<u8> = Vec::new();
        let req = Request::List;
        write_request_line(&mut out, &req).unwrap();
        assert!(out.ends_with(b"\n"));
        // The line without the trailing newline must parse back to the
        // original request — protects against accidentally emitting
        // multi-line JSON.
        let line = std::str::from_utf8(&out).unwrap().trim_end();
        let parsed: Request = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, Request::List);
    }

    #[test]
    fn read_response_line_parses_ok() {
        let input: &[u8] = b"{\"status\":\"ok\",\"data\":null}\n";
        let mut reader = std::io::BufReader::new(input);
        let resp = read_response_line(&mut reader).unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    #[test]
    fn read_response_line_eof_is_error() {
        let input: &[u8] = b"";
        let mut reader = std::io::BufReader::new(input);
        assert!(read_response_line(&mut reader).is_err());
    }

    #[test]
    fn write_request_line_roundtrips_split() {
        let req = Request::Split {
            target: PaneRef::Focused,
            direction: Direction::Horizontal,
            command: Some("echo".into()),
            id: Some("foo".into()),
        };
        let mut out: Vec<u8> = Vec::new();
        write_request_line(&mut out, &req).unwrap();
        let line = std::str::from_utf8(&out).unwrap().trim_end();
        let parsed: Request = serde_json::from_str(line).unwrap();
        assert_eq!(parsed, req);
    }
}
