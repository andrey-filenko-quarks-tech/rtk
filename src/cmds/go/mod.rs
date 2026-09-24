// Temporary: `GoFlags::rest` has no reader until gofmt's stdin rule lands.
#[allow(dead_code)]
pub mod go_args;
pub mod go_cmd;
pub mod go_run;
pub mod go_tool;
pub mod golangci_cmd;
pub mod staticcheck_cmd;
