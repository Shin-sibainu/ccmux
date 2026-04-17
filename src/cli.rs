use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "ccmux", version, about = "Claude Code Multiplexer")]
pub struct Cli {
    /// Subcommands talk to an already-running ccmux instance over IPC.
    /// When absent, ccmux launches as a TUI (using the flags below).
    #[command(subcommand)]
    pub command: Option<IpcCommand>,

    /// Working directory to change into before launching
    pub dir: Option<PathBuf>,

    /// Command to execute automatically in the initial pane after the shell is ready
    #[arg(long, value_name = "CMD", conflicts_with = "layout")]
    pub exec: Option<String>,

    /// Load a multi-pane layout by name from
    /// `./ccmux-layouts/<NAME>.toml` or `~/.config/ccmux/layouts/<NAME>.toml`
    #[arg(long, value_name = "NAME", conflicts_with = "exec")]
    pub layout: Option<String>,
}

/// Subcommands dispatched to a running ccmux instance via its IPC
/// endpoint. These always exit without starting the TUI.
#[derive(Subcommand, Debug, Clone)]
pub enum IpcCommand {
    /// List panes in the active workspace.
    List,
    /// Write text to a pane. Exactly one of --name / --id / --focused.
    Send {
        /// Pane name (defined in the layout or via `split --id NAME`).
        #[arg(long, conflicts_with_all = ["id", "focused"])]
        name: Option<String>,
        /// Numeric pane id as shown by `ccmux list`.
        #[arg(long, conflicts_with_all = ["name", "focused"])]
        id: Option<usize>,
        /// Target the currently focused pane.
        #[arg(long, conflicts_with_all = ["name", "id"])]
        focused: bool,
        /// Append Enter after the text so the shell executes it.
        #[arg(long)]
        enter: bool,
        /// Text to send.
        text: String,
    },
    /// Move keyboard focus to a pane.
    Focus {
        #[arg(long, conflicts_with = "id")]
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        id: Option<usize>,
    },
    /// Open a new tab with a fresh single pane. Focus switches to the
    /// new tab. Optionally pre-assigns a stable pane name and tab
    /// label.
    NewTab {
        /// Command to run in the new pane.
        #[arg(long)]
        command: Option<String>,
        /// Stable name for the new pane (lookup via `--name`).
        #[arg(long)]
        id: Option<String>,
        /// Tab label (overrides the cwd-derived default).
        #[arg(long)]
        label: Option<String>,
    },
    /// Split a pane and optionally run a command in the new side.
    Split {
        /// Target pane to split. Defaults to the focused pane.
        #[arg(long, conflicts_with_all = ["target_id", "target_focused"])]
        target_name: Option<String>,
        #[arg(long, conflicts_with_all = ["target_name", "target_focused"])]
        target_id: Option<usize>,
        #[arg(long, conflicts_with_all = ["target_name", "target_id"])]
        target_focused: bool,
        /// Split direction.
        #[arg(long, value_parser = ["vertical", "horizontal"])]
        direction: String,
        /// Command to run in the freshly-created pane.
        #[arg(long)]
        command: Option<String>,
        /// Stable name to assign to the new pane so it can be addressed
        /// later via `--name`.
        #[arg(long)]
        id: Option<String>,
    },
}

impl Cli {
    /// Validate fields that clap cannot express declaratively.
    pub fn validate_exec(&self) -> anyhow::Result<()> {
        if let Some(cmd) = &self.exec {
            if cmd.is_empty() {
                anyhow::bail!("--exec value must not be empty");
            }
        }
        Ok(())
    }
}

impl IpcCommand {
    /// Translate a CLI subcommand into the over-the-wire IPC request.
    pub fn to_request(&self) -> anyhow::Result<crate::ipc::Request> {
        use crate::ipc::{Direction, PaneRef, Request};

        fn pick_ref(
            name: &Option<String>,
            id: &Option<usize>,
            focused: bool,
        ) -> anyhow::Result<PaneRef> {
            match (name, id, focused) {
                (Some(n), None, false) => Ok(PaneRef::Name(n.clone())),
                (None, Some(i), false) => Ok(PaneRef::Id(*i)),
                (None, None, true) => Ok(PaneRef::Focused),
                (None, None, false) => Ok(PaneRef::Focused),
                _ => {
                    anyhow::bail!("ambiguous target: use at most one of --name / --id / --focused")
                }
            }
        }

        match self {
            IpcCommand::List => Ok(Request::List),
            IpcCommand::NewTab { command, id, label } => Ok(Request::NewTab {
                command: command.clone(),
                id: id.clone(),
                label: label.clone(),
            }),
            IpcCommand::Send {
                name,
                id,
                focused,
                enter,
                text,
            } => Ok(Request::Send {
                target: pick_ref(name, id, *focused)?,
                data: text.clone(),
                append_enter: *enter,
            }),
            IpcCommand::Focus { name, id } => Ok(Request::Focus {
                target: pick_ref(name, id, false)?,
            }),
            IpcCommand::Split {
                target_name,
                target_id,
                target_focused,
                direction,
                command,
                id,
            } => {
                let dir = match direction.as_str() {
                    "vertical" => Direction::Vertical,
                    "horizontal" => Direction::Horizontal,
                    other => anyhow::bail!("invalid direction: {other}"),
                };
                Ok(Request::Split {
                    target: pick_ref(target_name, target_id, *target_focused)?,
                    direction: dir,
                    command: command.clone(),
                    id: id.clone(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_args() {
        let cli = Cli::try_parse_from(["ccmux"]).unwrap();
        assert_eq!(cli.dir, None);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_directory_only() {
        let cli = Cli::try_parse_from(["ccmux", "/path"]).unwrap();
        assert_eq!(cli.dir, Some(PathBuf::from("/path")));
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_exec_flag() {
        let cli = Cli::try_parse_from(["ccmux", "--exec", "claude /company"]).unwrap();
        assert_eq!(cli.dir, None);
        assert_eq!(cli.exec.as_deref(), Some("claude /company"));
    }

    #[test]
    fn parses_dir_and_exec() {
        let cli = Cli::try_parse_from(["ccmux", ".", "--exec", "cce"]).unwrap();
        assert_eq!(cli.dir, Some(PathBuf::from(".")));
        assert_eq!(cli.exec.as_deref(), Some("cce"));
    }

    #[test]
    fn rejects_empty_exec() {
        let result = Cli::try_parse_from(["ccmux", "--exec", ""]);
        assert!(
            result.is_ok(),
            "clap is permissive; validation happens later"
        );
        let cli = result.unwrap();
        assert_eq!(cli.exec.as_deref(), Some(""));
        assert!(
            cli.validate_exec().is_err(),
            "empty exec should fail validation"
        );
    }

    #[test]
    fn parses_layout_flag() {
        let cli = Cli::try_parse_from(["ccmux", "--layout", "cc-campany"]).unwrap();
        assert_eq!(cli.layout.as_deref(), Some("cc-campany"));
        assert_eq!(cli.exec, None);
    }

    #[test]
    fn rejects_exec_and_layout_together() {
        let result = Cli::try_parse_from(["ccmux", "--exec", "x", "--layout", "y"]);
        assert!(
            result.is_err(),
            "exec and layout must be mutually exclusive"
        );
    }

    // -- Phase 3: IPC subcommands ------------------------------------

    #[test]
    fn parses_list_subcommand() {
        let cli = Cli::try_parse_from(["ccmux", "list"]).unwrap();
        assert!(matches!(cli.command, Some(IpcCommand::List)));
    }

    #[test]
    fn parses_send_subcommand_with_name() {
        let cli = Cli::try_parse_from([
            "ccmux",
            "send",
            "--name",
            "engineering",
            "--enter",
            "echo hi",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::Send {
                name,
                enter,
                text,
                focused,
                ..
            }) => {
                assert_eq!(name.as_deref(), Some("engineering"));
                assert!(enter);
                assert!(!focused);
                assert_eq!(text, "echo hi");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn parses_focus_subcommand_with_id() {
        let cli = Cli::try_parse_from(["ccmux", "focus", "--id", "2"]).unwrap();
        match cli.command {
            Some(IpcCommand::Focus { id, name }) => {
                assert_eq!(id, Some(2));
                assert!(name.is_none());
            }
            other => panic!("expected Focus, got {other:?}"),
        }
    }

    #[test]
    fn parses_split_subcommand() {
        let cli = Cli::try_parse_from([
            "ccmux",
            "split",
            "--direction",
            "horizontal",
            "--command",
            "ccr",
            "--id",
            "research",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::Split {
                direction,
                command,
                id,
                ..
            }) => {
                assert_eq!(direction, "horizontal");
                assert_eq!(command.as_deref(), Some("ccr"));
                assert_eq!(id.as_deref(), Some("research"));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn rejects_send_with_two_targets() {
        let result = Cli::try_parse_from(["ccmux", "send", "--name", "a", "--id", "1", "text"]);
        assert!(result.is_err(), "name and id are mutually exclusive");
    }

    #[test]
    fn split_rejects_bad_direction() {
        let result = Cli::try_parse_from(["ccmux", "split", "--direction", "diagonal"]);
        assert!(result.is_err(), "direction must be vertical or horizontal");
    }

    #[test]
    fn send_to_request_uses_focused_when_no_target() {
        let cli = Cli::try_parse_from(["ccmux", "send", "hi"]).unwrap();
        let cmd = cli.command.unwrap();
        let req = cmd.to_request().unwrap();
        match req {
            crate::ipc::Request::Send { target, data, .. } => {
                assert!(matches!(target, crate::ipc::PaneRef::Focused));
                assert_eq!(data, "hi");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn parses_new_tab_subcommand() {
        let cli = Cli::try_parse_from([
            "ccmux",
            "new-tab",
            "--command",
            "cce",
            "--id",
            "engineering",
            "--label",
            "eng",
        ])
        .unwrap();
        match cli.command {
            Some(IpcCommand::NewTab { command, id, label }) => {
                assert_eq!(command.as_deref(), Some("cce"));
                assert_eq!(id.as_deref(), Some("engineering"));
                assert_eq!(label.as_deref(), Some("eng"));
            }
            other => panic!("expected NewTab, got {other:?}"),
        }
    }

    #[test]
    fn new_tab_to_request_preserves_fields() {
        let cli = Cli::try_parse_from(["ccmux", "new-tab", "--command", "ccr", "--id", "research"])
            .unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::NewTab { command, id, label } => {
                assert_eq!(command.as_deref(), Some("ccr"));
                assert_eq!(id.as_deref(), Some("research"));
                assert!(label.is_none());
            }
            other => panic!("expected NewTab, got {other:?}"),
        }
    }

    #[test]
    fn split_to_request_translates_direction() {
        let cli = Cli::try_parse_from(["ccmux", "split", "--direction", "vertical", "--id", "foo"])
            .unwrap();
        let req = cli.command.unwrap().to_request().unwrap();
        match req {
            crate::ipc::Request::Split { direction, id, .. } => {
                assert!(matches!(direction, crate::ipc::Direction::Vertical));
                assert_eq!(id.as_deref(), Some("foo"));
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }
}
