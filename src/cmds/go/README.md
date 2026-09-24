# Go Ecosystem

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- `go_cmd.rs` uses `GoCommands` sub-enum in main.rs (same pattern as git/cargo)
- `go test` outputs NDJSON (`-json` flag injected by RTK) -- parsed line-by-line as streaming events
- `golangci_cmd.rs` forces `--out-format=json` for structured parsing
- Third-party Go tools are top-level commands (`rtk staticcheck`, `rtk govulncheck`, `rtk gotestsum`, `rtk goreleaser`, `rtk gofmt`, `rtk goimports`), and all but gofmt/goimports are also reached as `rtk go tool <name>` so the version pinned by a `go.mod` `tool` directive still runs. Go-`flag` tools parse arguments with Go's flag rules (atomic single-dash names, parsing stops at the first package); cobra tools with the POSIX grammar
- `staticcheck` runs with `-f json` and groups findings by check, compile errors first, a few locations each; a `-f` you pass is honoured untouched
- `govulncheck` groups called vulnerabilities by module with the highest fix version and one example trace; `-show verbose`, `-format`, `-json`, `-mode` and `-scan` other than symbol pass through
- `gotestsum` writes its test events to a temporary `--jsonfile` (removed afterwards; a `--jsonfile` you pass is read, never deleted) and shows the `rtk go test` view. Verbose formats, `-v` for `go test`, `--watch`, `--rerun-fails` and `--raw-command` pass through
- `goreleaser release`/`build` becomes the outcome, build and archive counts and the artifact list; a failure keeps the step and goreleaser's own `⨯` line. Other subcommands, `--verbose` and `--debug` pass through
- `gofmt -l`/`goimports -l` keep the tool's own lines (the list is often consumed by the next command), capped with recall when that is shorter; `-d` becomes one `path (+a -r)` line per file. Neither is rewritten inside a pipeline, and a run without file arguments formats stdin
