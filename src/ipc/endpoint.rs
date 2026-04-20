//! Cross-platform IPC endpoint naming.
//!
//! Endpoints are scoped per running ccmux instance using its PID, so
//! multiple ccmux processes on one machine don't collide. The endpoint
//! name is also published to child PTYs via the `CCMUX_SOCKET`
//! environment variable so in-pane clients (the secretary, etc.) can
//! find the right server.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

pub const ENV_SOCKET: &str = "CCMUX_SOCKET";

/// Compute the IPC endpoint name for a ccmux instance with the given PID.
///
/// On Windows this is a Named Pipe path (`\\.\pipe\ccmux-<pid>`).
/// On Unix this is a filesystem path under `$XDG_RUNTIME_DIR/ccmux/`,
/// falling back to `$TMPDIR` and then `/tmp/ccmux-$UID/`.
pub fn endpoint_for_pid(pid: u32) -> Result<EndpointName> {
    #[cfg(windows)]
    {
        Ok(EndpointName::pipe(format!(r"\\.\pipe\ccmux-{pid}")))
    }
    #[cfg(unix)]
    {
        let dir = unix_socket_dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        // Restrict directory to owner-only; ignore failure on platforms
        // where chmod isn't expressible (shouldn't happen on unix).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            let _ = std::fs::set_permissions(&dir, perms);
        }
        let path = dir.join(format!("ccmux-{pid}.sock"));
        Ok(EndpointName::socket(path))
    }
}

#[cfg(unix)]
fn unix_socket_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d).join("ccmux"));
        }
    }
    if let Ok(d) = std::env::var("TMPDIR") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d).join(format!("ccmux-{}", uid())));
        }
    }
    Ok(PathBuf::from("/tmp").join(format!("ccmux-{}", uid())))
}

#[cfg(unix)]
fn uid() -> u32 {
    // Avoid pulling in `nix` just for getuid; libc is already a transitive
    // dependency of basically everything on Unix. Until we add it
    // explicitly, fall back to env(USER) if libc isn't available — the
    // value only needs to be stable per-user.
    std::env::var("UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointName {
    repr: String,
    kind: EndpointKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EndpointKind {
    Pipe,
    Socket,
}

impl EndpointName {
    pub fn pipe(name: impl Into<String>) -> Self {
        Self {
            repr: name.into(),
            kind: EndpointKind::Pipe,
        }
    }
    pub fn socket(path: PathBuf) -> Self {
        Self {
            repr: path.display().to_string(),
            kind: EndpointKind::Socket,
        }
    }
    pub fn as_str(&self) -> &str {
        &self.repr
    }
    pub fn kind(&self) -> EndpointKind {
        self.kind
    }
}

/// Look up the endpoint a child process should connect to. Returns an
/// error if the parent ccmux did not publish `CCMUX_SOCKET`.
pub fn endpoint_from_env() -> Result<EndpointName> {
    let s = std::env::var(ENV_SOCKET)
        .map_err(|_| anyhow!("{ENV_SOCKET} not set; are you running inside ccmux?"))?;
    if s.is_empty() {
        return Err(anyhow!("{ENV_SOCKET} is empty"));
    }
    #[cfg(windows)]
    {
        Ok(EndpointName::pipe(s))
    }
    #[cfg(unix)]
    {
        Ok(EndpointName::socket(PathBuf::from(s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_for_pid_includes_pid() {
        let ep = endpoint_for_pid(12345).unwrap();
        assert!(ep.as_str().contains("12345"), "{}", ep.as_str());
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_uses_pipe_prefix() {
        let ep = endpoint_for_pid(1).unwrap();
        assert!(ep.as_str().starts_with(r"\\.\pipe\"), "{}", ep.as_str());
        assert_eq!(ep.kind(), EndpointKind::Pipe);
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_is_socket_path() {
        let ep = endpoint_for_pid(1).unwrap();
        assert!(ep.as_str().ends_with(".sock"), "{}", ep.as_str());
        assert_eq!(ep.kind(), EndpointKind::Socket);
    }

    #[test]
    fn endpoint_from_env_fails_without_var() {
        std::env::remove_var(ENV_SOCKET);
        assert!(endpoint_from_env().is_err());
    }
}
