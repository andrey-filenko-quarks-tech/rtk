# Go Ecosystem

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- `go_cmd.rs` uses `GoCommands` sub-enum in main.rs (same pattern as git/cargo)
- `go test` outputs NDJSON (`-json` flag injected by RTK) -- parsed line-by-line as streaming events
- `golangci_cmd.rs` forces `--out-format=json` for structured parsing
- `go mod`, `go list` and `go generate` are further `GoCommands` variants; Go's flags are parsed with atomic single-dash names, stopping at the first package argument as Go does
- `go mod graph` becomes counts, direct requirements with their transitive fan-out, and modules required at several versions (Go-version `go@`/`toolchain@` nodes dropped); the full graph is in recall
- `go mod tidy` prints nothing about what it changed, so rtk diffs `go.mod` before and after and reports `+/-/~` changes — the one Go output allowed past the never-worse guard (see `core/guard.rs`)
- `go list` prints a package list's shared module path once; `-m all` shows direct requirements, `-m -u all` direct updates with indirect ones counted; `-f`/`-json` and other machine forms pass through, and the hook never rewrites `go list`/`go mod graph` as a pipeline producer
- `go generate` collapses success to `ok` and keeps a failing generator's last lines
