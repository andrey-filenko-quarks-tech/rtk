//! How a third-party Go tool is invoked: as its own binary, or through `go tool` when the
//! module pins it with a `tool` directive in `go.mod`.

use crate::cmds::go::go_run::run_go_passthrough;
use crate::core::runner;
use crate::core::utils::resolved_command;
use anyhow::Result;
use std::ffi::OsString;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolBin {
    Direct,
    GoTool,
}

impl ToolBin {
    pub(crate) fn command(self, tool: &str) -> Command {
        match self {
            ToolBin::Direct => resolved_command(tool),
            ToolBin::GoTool => {
                let mut cmd = resolved_command("go");
                cmd.args(["tool", tool]);
                cmd
            }
        }
    }

    /// The label tracking and the tee use, so `go tool` runs stay distinguishable.
    pub(crate) fn tool_name(self, tool: &str) -> String {
        match self {
            ToolBin::Direct => tool.to_string(),
            ToolBin::GoTool => format!("go tool {tool}"),
        }
    }

    pub(crate) fn passthrough(self, tool: &str, args: &[String], verbose: u8) -> Result<i32> {
        match self {
            ToolBin::Direct => {
                let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
                runner::run_passthrough(tool, &os_args, verbose)
            }
            ToolBin::GoTool => {
                let tool_args: Vec<String> = std::iter::once(tool.to_string())
                    .chain(args.iter().cloned())
                    .collect();
                run_go_passthrough("tool", &tool_args, verbose)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn go_tool_prefixes_the_tool_name() {
        assert_eq!(
            args_of(&ToolBin::GoTool.command("staticcheck")),
            ["tool", "staticcheck"]
        );
        assert!(args_of(&ToolBin::Direct.command("staticcheck")).is_empty());
    }

    #[test]
    fn labels_keep_go_tool_runs_apart() {
        assert_eq!(ToolBin::Direct.tool_name("govulncheck"), "govulncheck");
        assert_eq!(
            ToolBin::GoTool.tool_name("govulncheck"),
            "go tool govulncheck"
        );
    }
}
