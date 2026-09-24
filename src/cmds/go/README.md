# Go Ecosystem

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- `go_cmd.rs` uses `GoCommands` sub-enum in main.rs (same pattern as git/cargo)
- `go test` outputs NDJSON (`-json` flag injected by RTK) -- parsed line-by-line as streaming events
- `golangci_cmd.rs` forces `--out-format=json` for structured parsing
- `buf` is standalone (`rtk buf`) and also reached from `go tool buf` through `go_cmd`'s tool interception
- buf `lint`/`build`/`breaking` get `--error-format=json` injected before `--` and are grouped by rule; COMPILE errors group on the message with identifiers masked, so one missing import's cascade collapses into two groups, root cause first. `build` reports on stderr, so it filters the combined stream
- buf `format -d` becomes a per-file `+/-` summary with the full diff in recall; `generate` failures keep a plugin panic's first frame only
- the hook rewrite needs the subcommand first: `buf --debug lint` is not auto-rewritten (typing `rtk buf --debug lint` still filters)
- `go mod`, `go list` and `go generate` are further `GoCommands` variants; Go's flags are parsed with atomic single-dash names, stopping at the first package argument as Go does, and an explicit `=false` turns a boolean flag off. Shared helpers live in their own modules: Go flag rules, `go.mod` knowledge (`-C`, `-modfile`, `GOFLAGS`, walking up, workspaces) and passthrough/recall
- `go mod graph` becomes counts, direct requirements with their transitive fan-out, and modules required at several versions (Go-version `go@`/`toolchain@` nodes dropped); the full graph is in recall
- `go mod tidy` prints nothing about what it changed — a warm run that rewrote `go.mod` emits nothing at all — so rtk reads `go.mod` before and after and reports the `+/-/~` changes, `go`/`toolchain` directives included. That change list is information the raw output never holds, which is why it is the one Go output allowed past the never-worse guard (listed in `core/guard.rs`); it is capped, everything else stays guarded, Go's own messages stay on stderr, and an unreadable `go.mod` makes it say "changes unknown", never "no changes"
- `go list` prints a package list's shared module path once; `-m all` shows direct requirements, `-m -u all` direct updates with indirect ones counted; `-f`/`-json` and other machine forms pass through, and the hook never rewrites `go list`/`go mod graph` as a pipeline producer
- `go generate` reads the combined stream (generators such as mockery log on stderr): success collapses to `ok`, a failure keeps the tail, which ends with Go's `running "…"` verdict
