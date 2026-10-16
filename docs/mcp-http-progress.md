# MCP over HTTP: plan and progress

Notes on adding a Streamable HTTP transport to the FilmCraft MCP server, so MCP clients connect by
URL (`http://127.0.0.1:<port>/mcp`) to a long-running server instead of launching `filmcraft-cli`
as a stdio child process, the way ComfyUI's MCP servers are used. This file is the hand-off point:
if a session stops, the next one starts here.

**State (2026-10-16): done.**
- Built, reviewed over seven rounds, and gated.
- Nothing has been run on Windows yet; the user's first run on Windows is the next step.
- Usage is documented in [agents.md § HTTP transport](agents.md#http-transport), with a
  [Windows quick start](agents.md#windows-quick-start).

## Branches

- `mcp-http`: three clean commits on top of `main` 8309f21.
  1. The transport, the tests, and the small engine change.
  2. The docs.
  3. This file. Drop it before proposing the work upstream.
- `mcp-http-history`: the same end result as the 67 commits it was built in, one per fix, for
  `git log` / `git blame` archaeology.

## Goal

`filmcraft-cli mcp --http 127.0.0.1:9877` serves the existing MCP tools and resources over
Streamable HTTP, in headless mode (`--demo`, `--project`, `--data-dir`) and in bridge mode
(`--bridge 127.0.0.1:9876`, driving the running desktop app). stdio stays the default and
behaves as before.

## Decisions

| Date | Decision |
|---|---|
| 2026-10-15 | **Option A**: the HTTP transport goes in `filmcraft-cli mcp` (a separate process, like ComfyUI MCP servers in front of ComfyUI's API), not inside the desktop app (Option B, possible later). |
| 2026-10-15 | Work is pushed to `mcp-http` regularly; work-in-progress snapshots are squashed into green commits at the end (done 2026-10-16). |
| 2026-10-15 | **Token on by default**, loopback too: other programs and users on the PC can't use the HTTP endpoint without it. `--no-token` opts out, on loopback only. In bridge mode the app's `--control` port still has no authentication. |

## What was built

- **Transport** (`crates/automation/src/http_transport*`).
  - rmcp 3.5.1's `StreamableHttpService` on hyper 1, at `/mcp`.
  - A gate checks, before reading any body: path, token, `Host` (DNS rebinding), `Origin`,
    `Accept`, `Content-Type`, and requests relayed by a proxy or tunnel.
  - CORS only for origins listed with `--allow-origin`.
  - Connection caps that close unauthenticated connections first; header, body and idle
    timeouts; back-off after repeated wrong tokens.
  - A session cap with idle reaping and LRU eviction that never touches a session with a call in
    flight.
  - Refusals are logged to stderr, rate-limited, never with the token.
- **Token** (`apps/filmcraft-cli/src/token.rs`).
  - Sources, first match wins: `FILMCRAFT_MCP_TOKEN`, then `--token-file`, then a token
    generated on first start into `mcp-token` in the per-user data folder (`create_new`, mode
    0600, owner checked, links and FIFOs refused).
  - `filmcraft-cli mcp-token` prints it; `--new` replaces it and `--path` shows where it is.
  - The banner prints a ready `claude mcp add … --header` command that reads the token, never
    the token itself.
  - Non-loopback binds, tunnel names, `--allow-origin null` and relayed requests need a token of
    32+ characters.
- **Long calls** (`crates/automation/src/long_job.rs`).
  - Covers `file.exportMedia`, `export.quick`, batch export steps and exports started without
    `wait`.
  - They report progress and are cancelled by `notifications/cancelled`, a dropped 2026-07-28
    connection, `DELETE`, a closed stdio input or a stop.
  - Their partial files are deleted as soon as they stop; a finished export is never deleted.
- **Stopping:** Ctrl+C, SIGTERM, SIGHUP and the Windows console events cancel exports first and
  close the port at once, in stdio mode too.
- **Engine:** `autosave::per_user_data_dir` is split out of `default_data_dir`; behaviour is
  unchanged.
- **Size:** the release `filmcraft-cli` grows from 42,060,768 to 44,741,672 bytes (+6.4%). The
  desktop app doesn't depend on the automation crate and is unchanged.

## Verification

- **End to end with real clients.**
  - Claude Code 2.1.300 connects ("Connected" in `claude mcp list`, including with the printed
    token command) and calls tools.
  - The MCP Inspector CLI lists and calls tools.
  - Bridge mode drove the real desktop app.
  - Legacy sessions and 2026-07-28 stateless requests, progress over SSE, cancellation, two
    clients sharing one project.
- **Gates on the final code.**
  - `cargo fmt --check`; workspace `cargo clippy --all-targets --release -D warnings`; `cargo
    clippy --target x86_64-pc-windows-gnu` for the automation and CLI crates.
  - Release tests: `filmcraft-automation` 93/93; `filmcraft-cli` 45 unit + 26 CLI tests + 1 of 2
    workflow tests; `filmcraft-engine` 638/639.
  - `xtask layers`, `assets` and `wasm` (49 crates).
- **The two test failures happen on unmodified `main` too.** Both are checks against this
  sandbox's ffmpeg / ffprobe 6.1.1, confirmed by running them on 8309f21:
  - `phone_vfr_clip_extract_gain_title_to_h264_mp4`: the same frame-20 difference, 19.04;
  - `every_builtin_preset_exports_and_ffprobe_confirms`: ffprobe 6.1 has no APV decoder.
- **Not run:** the rest of `cargo test --workspace --release`, because its build exceeds this
  sandbox's 2-hour job limit. The other crates' code and dependency versions are unchanged
  (the only shared change is the engine refactor, whose tests ran).

## Known limitations

- **Untested on Windows:** the Windows-only code is only compiled and lint-checked
  (`x86_64-pc-windows-gnu`). That covers console-event handling, PowerShell banner lines,
  profile-privacy warnings and the Windows paths in tests.
- **Logoff and shutdown:** Windows may not tell `filmcraft-cli` (it loads graphics libraries)
  that the user is logging off or shutting down. Handling that needs a hidden window in
  `crates/platform`; the docs say to stop the server first.
- **Bridge mode:** the desktop app's `--control` port has no token, so local programs can still
  drive the app directly.
- **Claude Desktop** can't use a localhost URL (its custom connectors are reached from
  Anthropic's cloud); it keeps using the stdio server.
- **Before upstreaming:** the Windows quick start names this fork and branch, and CLAUDE.md's
  gate list doesn't yet include the Windows cross clippy.

## Review history

Every round was reviewed by independent protocol, security and integration reviewers, who
tested the release binary (each fix has a regression test that failed before it).

| Round | Highlights |
|---|---|
| Prototype | HTTP transport, Host/Origin checks, optional token, Ctrl+C clean-up |
| 1 | Connection caps and timeouts, token rules off loopback, checks before the body, CORS for listed origins, session reaping, SIGTERM / Windows console events, PowerShell token files, docs |
| 2 | Every rmcp session tracked at the source (aborted `initialize` leaked memory), auth back-off can't lock local clients out, tunnel names need a token |
| 3 | Aborted `DELETE` session leak (high), Windows accept-reset throttling, stops cancel exports first, relayed requests need a token; **token on by default** |
| 4 | Exports cancelled promptly on stop / `DELETE`, no-wait exports cleaned up, stored-token hardening, safe PowerShell / POSIX header commands |
| 5 | A finished export is never deleted, batch and lock-waiting exports cancellable, owner check, Windows test paths |
| 6 | Owner-check swap race closed, Windows `PATH` test, `export.quick` tracked, non-UTF-8 stdio lines, Flatpak / renamed-binary hints |
| 7 | Closed stdio input cancels only exports, stops wait only for exports, stdio clean-up of cut no-wait exports, Windows link check, test isolation |

Two container restarts interrupted the work (once mid-fix, once mid-CI). Nothing was lost:
every commit had been mirrored to GitHub.

## Resuming in a new session

1. Check out `mcp-http` from `GudenDK/filmcraftMCP` and read this file.
2. Next steps: build on Windows (see the quick start), run the server, connect Claude Code, and
   report anything that differs from the docs.
