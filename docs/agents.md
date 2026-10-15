# Agents

FilmCraft is built to be driven by AI agents, and largely built by them. Part 1 covers using
FilmCraft from an agent. Part 2 covers developing FilmCraft as an agent.

# Part 1: driving FilmCraft

## 1. MCP server

`filmcraft-cli mcp` serves MCP on stdio, or over HTTP with `--http` (see
[HTTP transport](#http-transport)). It has two modes:

| Mode | Command | What it drives |
|---|---|---|
| Headless | `filmcraft-cli mcp --demo` or `--project p.fcproj` (neither = empty project) | an in-process engine session, no window |
| Bridge | `filmcraft-cli mcp --bridge 127.0.0.1:9876` | the running desktop app started with `filmcraft --control 9876` |

Headless mode also takes `--data-dir DIR` (user presets). `--http`, `--token-file`,
`--allow-origin` and `--allow-host` set up the HTTP transport. Any other option is rejected (exit 2):
there are no folder-confinement flags such as `--automation-read-root`, so the server can read
and write wherever the user can.

**Claude Code.** The repository's `.mcp.json` registers both servers (`filmcraft` = bridge,
`filmcraft-headless` = demo). Both point at `target/release/filmcraft-cli`, so build it first:

```sh
cargo build --release -p filmcraft-cli -p filmcraft
./target/release/filmcraft --control 9876 &      # only for the bridge server
claude                                           # approve the project MCP servers when asked
```

To register the server by hand, or for another MCP client, use the absolute binary path:

```sh
claude mcp add filmcraft-headless -- /abs/path/filmcraft/target/release/filmcraft-cli mcp --demo
```

```json
{ "mcpServers": { "filmcraft": { "command": "/abs/path/filmcraft-cli", "args": ["mcp", "--bridge", "127.0.0.1:9876"] } } }
```

### From an installed release

The release packages ship `filmcraft-cli` alongside the desktop app, so no build is needed:

| Install | CLI |
|---|---|
| Windows (MSI) | `C:\Program Files\FilmCraft\filmcraft-cli.exe` by default (wherever you installed it otherwise), not on `PATH` |
| Windows (portable zip) | `filmcraft-cli.exe` next to `filmcraft.exe` in the folder you unzipped |
| Linux (deb, rpm) | `/usr/bin/filmcraft-cli` |
| Linux (Flatpak) | `flatpak run --command=filmcraft-cli ai.storyteller.filmcraft` |
| macOS | the separate `filmcraft-cli-<version>-macos-<arch>.zip` release asset (the `.app` holds only the desktop app) |

```sh
# Windows, default install folder
claude mcp add filmcraft -- "C:\Program Files\FilmCraft\filmcraft-cli.exe" mcp
# Linux, or macOS with the CLI unzipped onto PATH
claude mcp add filmcraft -- filmcraft-cli mcp
```

To run the CLI yourself in PowerShell, put the call operator `&` in front of a quoted path:
`& "C:\Program Files\FilmCraft\filmcraft-cli.exe" mcp --demo`. Without it, PowerShell prints the
string instead of running it.

### HTTP transport

With `--http ADDR`, `filmcraft-cli mcp` serves MCP over
[Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
at `http://ADDR/mcp` instead of stdio. You start it once and leave it running; clients connect by
URL instead of each launching its own child process, as with the HTTP MCP servers for ComfyUI. It
works with both modes, and the tools, resources and conventions are the same as on stdio:

```sh
filmcraft-cli mcp --demo --http 127.0.0.1:9877                    # headless engine
filmcraft --control 9876 &                                        # or the live app ...
filmcraft-cli mcp --bridge 127.0.0.1:9876 --http 127.0.0.1:9877   # ... over HTTP
```

```text
filmcraft-cli: MCP (Streamable HTTP) on http://127.0.0.1:9877/mcp, driving a headless engine with the demo project; no token: any local process can connect. Ctrl+C stops it.
filmcraft-cli: connect Claude Code with: claude mcp add --transport http --scope user filmcraft-http http://127.0.0.1:9877/mcp
```

`--http` is newer than the 0.8.0 release, whose `filmcraft-cli` stops with "unknown mcp option"
(exit 2). `filmcraft-cli help` lists `mcp --http <ADDR>` in builds that have it. Until a release
has it, build from source (see [Windows quick start](#windows-quick-start)).

`ADDR` is `PORT`, `127.0.0.1:PORT`, `[::1]:PORT` or `localhost:PORT`, all loopback. `PORT` alone
means `127.0.0.1:PORT`. `localhost:PORT` listens on `127.0.0.1` and also on `[::1]` at the same
port where the system has IPv6, because a client may resolve `localhost` to either address. If
another program already holds `[::1]:PORT`, it warns. Put `127.0.0.1` in client URLs anyway:
then the client cannot reach another program on `[::1]`.
`127.0.0.1:0` picks a free port, and the lines above, on stderr, show the real one. A port that is
taken exits 1 with a hint (on Windows, reserved port ranges show in
`netsh int ipv4 show excludedportrange protocol=tcp`).

**Stopping.** Ctrl+C stops the server, and so do `SIGTERM` and `SIGHUP` (`kill`, `systemctl stop`,
`docker stop`, a closed terminal) and, on Windows, Ctrl+Break, closing its console window, logging
off and shutting down. The server says which one it got. The port closes at once, calls in flight
are cancelled and sessions closed, and exports get time to delete their partial files: 15 s, or
3 s when Windows is closing the console, logging off or shutting down (it ends the process about
5 s after telling it). The output of an export still running at the end is deleted. A second
signal quits at once. Stdio mode (`mcp` without `--http`) stops on the same signals.

**Every client shares one backend.** All clients drive the same engine session (headless) or the
same app (bridge): one project, one undo history. An edit made by one client is what the next
`doc_inspect` shows the others, `edit.undo` from one client can undo another's edit, and the steps of
two clients' `command_batch` calls can interleave.

#### Connecting clients

**Claude Code.** Add the URL with the HTTP transport. `--scope user` makes it available in every
project; the default scope is the current project only:

```sh
claude mcp add --transport http --scope user filmcraft-http http://127.0.0.1:9877/mcp
claude mcp list                     # filmcraft-http: ✔ Connected, while the server runs
```

With a token (see Security below), add the header after the name and URL, as Claude Code's own
examples do:

```sh
claude mcp add --transport http --scope user filmcraft-http http://127.0.0.1:9877/mcp --header "Authorization: Bearer <token>"
```

That stores the token in your Claude Code settings (`~/.claude.json`). In a shell where the
variable is set, write `"Authorization: Bearer $FILMCRAFT_MCP_TOKEN"` (PowerShell:
`"Authorization: Bearer $env:FILMCRAFT_MCP_TOKEN"`) so you do not have to paste it. To keep the
token out of the settings file, register the server in a `.mcp.json` instead and let Claude Code
fill it in from the environment when it starts:

```json
{
  "mcpServers": {
    "filmcraft-http": {
      "type": "http",
      "url": "http://127.0.0.1:9877/mcp",
      "headers": { "Authorization": "Bearer ${FILMCRAFT_MCP_TOKEN}" }
    }
  }
}
```

`${FILMCRAFT_MCP_TOKEN}` must then be set in the environment Claude Code starts in. On Windows,
`$env:FILMCRAFT_MCP_TOKEN = "..."` lasts only for that PowerShell window. Store it for your user
with `setx FILMCRAFT_MCP_TOKEN "<token>"`, then open a new terminal and start Claude Code (and the
server) from there: `setx` does not change windows that are already open. Without the variable,
`claude mcp list` warns that it is missing and Claude Code sends the text `${FILMCRAFT_MCP_TOKEN}`
as the token: every request gets 401, and after 10 of them 429 for a minute.

The repository's `.mcp.json` keeps the stdio servers, which Claude Code starts by itself; an HTTP
entry only connects while a server is running.

**Other clients.** Any client with the Streamable HTTP transport takes the same URL, as
`{"type": "http", "url": "http://127.0.0.1:9877/mcp"}` or its equivalent. Claude Desktop runs local
servers over stdio, so use the stdio command there. MCP Inspector needs no flag in its default
proxy mode, where its own Node process makes the requests and sends no `Origin`. Only its direct
mode, where the browser calls the server, needs `--allow-origin http://localhost:6274` (see
Security).

**Containers and sandboxes.**

- A client in a Docker Desktop container (Windows, macOS) reaches the host as
  `host.docker.internal`, which Docker Desktop forwards to the host's loopback, so the server can
  stay on `127.0.0.1`. Add `--allow-host host.docker.internal:9877` so its `Host` is accepted.
  Every container on the machine can then connect: without a token the server warns about it.
  Docker Engine on Linux has no such name by default. There, run the client with `--network host`
  and use `127.0.0.1`. Otherwise, listen on the bridge address (e.g. `--http 172.17.0.1:9877`,
  which needs a token) and start the client with `--add-host=host.docker.internal:host-gateway`.
- The Flatpak sandbox has no network by default. The server can listen inside it, but host clients
  cannot reach it until you grant network access:
  `flatpak override --user --share=network ai.storyteller.filmcraft`.

**Bridge mode.** The token and the checks below protect only the HTTP endpoint. The app's
`--control` port has no authentication of its own: any local process can connect to it and drive
the app directly, token or not. Keep it on loopback (it always is) and only enable it while you use
it.

#### Windows quick start

For the desktop app driven over HTTP from PowerShell. The 0.8.0 release has no `--http`, so build
the CLI and the app from a branch that has it, e.g. `mcp-http` of
`https://github.com/GudenDK/filmcraftMCP`. You need Git, Rust from <https://rustup.rs> and the
MSVC build tools (Visual Studio Build Tools, "Desktop development with C++"):

```powershell
git clone --branch mcp-http https://github.com/GudenDK/filmcraftMCP.git filmcraft
cd filmcraft
cargo build --release -p filmcraft-cli -p filmcraft     # the first build takes a while
& .\target\release\filmcraft-cli.exe help              # lists `mcp --http <ADDR>`
```

Pick a port nothing else uses and Windows does not reserve. `9877` usually works; check with:

```powershell
Get-NetTCPConnection -LocalPort 9877 -ErrorAction SilentlyContinue   # no output: free
netsh int ipv4 show excludedportrange protocol=tcp                   # 9877 must not be in a range
```

Start the app with its control channel, then the MCP server in front of it. Leave that window open:
closing it stops the server. (The control port takes no token: see Bridge mode above.)

```powershell
Start-Process .\target\release\filmcraft.exe -ArgumentList '--control', '9876'
& .\target\release\filmcraft-cli.exe mcp --bridge 127.0.0.1:9876 --http 127.0.0.1:9877
```

Use `& .\target\release\filmcraft-cli.exe mcp --demo --http 127.0.0.1:9877` for a headless engine
without the app instead. On `127.0.0.1` Windows Firewall does not ask anything, because nothing
outside the machine can connect. It asks about network access only when you listen on `0.0.0.0` or
a LAN address; allow it on private networks only.

In another terminal, connect Claude Code and check it:

```powershell
claude mcp add --transport http --scope user filmcraft-http http://127.0.0.1:9877/mcp
claude mcp list
```

To require a token as well, store one for your user. This is optional on a single-user PC, since
the server only accepts local connections, but use it when others share the machine:

```powershell
setx FILMCRAFT_MCP_TOKEN ([guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N"))
```

`setx` changes only windows opened later, so stop the server and close the open PowerShell windows
(and Claude Code). In a new window, in the `filmcraft` folder, start the server as before: it reads
`FILMCRAFT_MCP_TOKEN` by itself, and its first line now says `bearer token required`. In another
new window, register Claude Code again with the token:

```powershell
claude mcp remove filmcraft-http --scope user
claude mcp add --transport http --scope user filmcraft-http http://127.0.0.1:9877/mcp --header "Authorization: Bearer $env:FILMCRAFT_MCP_TOKEN"
```

#### Security

Over HTTP any program that can open a socket can reach the server, and the tools write files
wherever the user can, so the endpoint is locked down by default:

- It listens on a loopback address. Any other address (`0.0.0.0:9877`, a LAN IP) needs a token of at
  least 32 characters (else exit 2), and a warning says that plain HTTP carries the token and every
  request in clear text: prefer `ssh -L 9877:127.0.0.1:9877 host`, or a TLS reverse proxy with
  `--allow-host`. On `0.0.0.0` the first lines show the loopback URL, the `Host` values accepted, and
  the `--allow-host <ip>:<port>` LAN clients need (until then they get 403).
- `Host` must name the server: `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>` or the address
  it listens on. A web page that rebinds its DNS name to 127.0.0.1 gets 403.
  `--allow-host NAME[:PORT]` (repeatable) adds names: a LAN name or IP, `host.docker.internal:<port>`,
  a tunnel. A name without a port matches every port. A malformed entry exits 2. A name that is
  not this machine (anything but `localhost`, 127.0.0.0/8, `[::1]` and `host.docker.internal`)
  means requests relayed from the network by a tunnel or reverse proxy, so it needs a token of at
  least 32 characters (else exit 2), as a non-loopback address does. Behind a proxy every request
  comes from the proxy's address (on this machine: 127.0.0.1, like local clients), so the
  per-address limits (connections, wrong tokens) count them all together; the first lines say so.
- A request with an `Origin` header comes from a browser and gets 403, unless the origin is listed
  with `--allow-origin` (repeatable). MCP clients send no `Origin`. An entry is `scheme://host:port`
  with the port written out, exactly as the browser sends it: `http://localhost:6274` and
  `http://127.0.0.1:6274` are different origins. Wildcards (`*`, `:*`), an entry without a port
  (it would match every port), paths and user names exit 2. `null` is accepted with a warning:
  every sandboxed frame and `file:` page sends it, so use it only with a token.
- A listed origin gets CORS headers, so a page there can call the server: its preflight
  (`OPTIONS` with `Access-Control-Request-Method`) gets 204 with `Access-Control-Allow-Origin`,
  `Access-Control-Allow-Methods: GET, POST, DELETE` and the MCP request headers
  (`Content-Type`, `Accept`, `Authorization`, `Mcp-Session-Id`, `Mcp-Protocol-Version`,
  `Mcp-Method`, `Mcp-Name`, `Last-Event-Id`, `Mcp-Param-*`), before the token check because
  browsers send preflights without credentials. Every answer to it carries
  `Access-Control-Allow-Origin`, `Vary: Origin` and
  `Access-Control-Expose-Headers: Mcp-Session-Id, Mcp-Protocol-Version, WWW-Authenticate`. Other
  origins never get CORS headers.
- **Token.** Set `FILMCRAFT_MCP_TOKEN`, or pass `--token-file FILE` (its first line), and every
  request (every method, checked right after `Host` and `Origin`) needs
  `Authorization: Bearer <token>`, else 401. The
  token is compared in constant time and never printed or logged. There is no `--token VALUE`:
  other users can read a command line. Without a token, any local process (any user, on a shared
  machine) can drive FilmCraft; use one on shared machines. Generate one with
  `openssl rand -hex 32`, or in PowerShell with
  `[guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N")`.
- **Token files.** They may be UTF-8 or ASCII, with or without a byte-order mark, or UTF-16 with
  one; line breaks and surrounding spaces are dropped. That covers Windows PowerShell 5.1's
  `'token' > token.txt` (UTF-16) and cmd's `echo`. A file with no usable first line exits 2 with
  a message that never shows its content. A new token written as ASCII always works:
  `Set-Content -Encoding ascii -Path $env:USERPROFILE\filmcraft-token.txt -Value ([guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N"))`
  in PowerShell, `(umask 077; openssl rand -hex 32 > token.txt)` elsewhere. On Linux and macOS a
  token file that other users can read or change gets a warning (`chmod 600` it). On Windows,
  your profile folder is readable only by you and administrators by default.
- After 10 wrong tokens within a minute, an address gets 429 (`Retry-After`) instead of 401 for its
  wrong tokens, for a minute. The right token still gets in: one of 32+ characters cannot be
  guessed anyway, and every local client connects from the same address (127.0.0.1), so a local
  process, or a client set up with the wrong token, must not lock the others out. A token shorter
  than 32 characters (loopback only) could be guessed, so while the address is blocked it is
  refused too; a local process can then lock local clients out for a minute at a time, one more
  reason to use a long token. Requests without an `Authorization` header and browser requests
  (refused for their `Host` or `Origin` first) never count.

#### Limits and timeouts

- **Connections.** At most 32 at once from one address (an IPv4 address, or an IPv6 /64) and 256
  in all. Every loopback address (127.0.0.0/8, ::1) counts as one, this machine: local clients
  share those 32. At a limit, the oldest connection that has not yet made a request past the
  `Host`, `Origin` and token checks is closed to make room, so connections that send nothing (or
  nothing valid) cannot lock out clients that have the token. Only when every connection there has
  made such a request does a new one get `503 Service Unavailable` with `Retry-After: 1` and is
  closed.
- **Slow and idle clients.** A request's line and headers must arrive within 10 s. A keep-alive
  connection with no request for 10 s is closed. A body must arrive within 30 s, else 408. Bodies
  are limited to 4 MiB (413 above).
- **Sessions** (clients on revisions before 2026-07-28). A session with nothing in flight that has
  not been used for 10 minutes is closed: clients that exit without `DELETE` do not pile up. A
  call in flight keeps its session open however long it runs (a long export without a
  `progressToken` included). At most 256 sessions: at the cap, the least recently used idle one is
  closed to make room. Only when every one has a call in flight does `initialize` get 503 with
  `Retry-After: 5`. A request to a closed session gets 404 (`Session not found`), and the client
  should `initialize` again, as the MCP spec says.
- A request id that is still in flight in the same session gets HTTP 400 with JSON-RPC `-32600`.

#### Status codes and logging

A request is checked in the order of this table, and everything down to `Content-Type` is checked
before its body is read. A request refused there costs no body read, gets the same answer whatever
its body holds, and has its connection closed.

| Status | When |
|---|---|
| 503 | too many connections and none can make room (`Retry-After: 1`), answered before the request is read |
| 404 | a path other than `/mcp` |
| 400 | more than one `Host`, `Origin` or `Authorization` header |
| 204 | a listed origin's CORS preflight |
| 400 / 403 | a malformed / foreign `Host` |
| 403 | an `Origin` that is not listed |
| 429 | a wrong token from an address that sent 10 wrong ones in a minute (for a minute; `Retry-After`); with a token under 32 characters, the right one too |
| 401 | no or wrong bearer token (`WWW-Authenticate: Bearer realm="filmcraft"`) |
| 405 | a method other than `GET`, `POST`, `DELETE`, such as an `OPTIONS` that is no CORS preflight |
| 406 | a `POST` whose `Accept` lacks `application/json` or `text/event-stream` |
| 415 | a `POST` whose `Content-Type` is not `application/json`; after the body, a JSON-RPC batch (array) |
| 408 / 413 | a body that takes over 30 s / is over 4 MiB |
| 400 | a body that is not JSON (JSON-RPC `-32700`, id `null`); a request id already in flight (`-32600`) |
| 404 | an unknown or closed session (`Session not found`): `initialize` again |
| 503 | every session has a call in flight (`initialize` only; `Retry-After: 5`) |
| 500 | an internal error (a panic), logged; the server keeps serving |

Refused requests and failures are logged to stderr as `filmcraft-mcp: refused POST from
127.0.0.1:53210: 403 Forbidden: Host header is not allowed (Host "evil.example:9877")`, with the
client's address. There is at most one line per address and reason every 10 s, and the next one
says how many were left out, so a flood of bad requests cannot flood the log. The `Authorization`
header is never logged. `Host` and `Origin` values are quoted and cut short. Programs that embed
the server get the lines through `HttpOptions::log` instead.

#### Protocol details

- Clients on MCP revisions before 2026-07-28 `initialize` and get an `Mcp-Session-Id`; `DELETE`
  ends a session (see Limits for idle ones). 2026-07-28 clients (current Claude Code) send
  self-contained requests and get no session.
- Replies come as an SSE stream (`text/event-stream`) that carries the call's progress notifications
  before its result; a request must `Accept` both `application/json` and `text/event-stream`.
- A body that isn't JSON gets HTTP 400 with the `-32700` parse error (id `null`), as a bad line does
  on stdio. A JSON-RPC batch (array) gets 415.
- A `tools/call` for a tool that does not exist gets JSON-RPC `-32602` `tool not found`, as on
  stdio. A notification naming one is accepted (202) and dropped.
- Cancelling a call: a session client posts `notifications/cancelled`; a 2026-07-28 client closes
  the call's HTTP response (it has no other way: that revision has no session to address it to).
  Either way an export stops and deletes its partial file (see Long exports below). Closing the
  response of a session client's call does *not* cancel it.

### Tools

| Tool | Modes | Purpose |
|---|---|---|
| `command_list` | both | every command: id, label, menu, shortcut, params, enabled now (`filter`, `enabled_only`) |
| `command_run` | both | run a command `{id, params}`; edits are undoable |
| `command_batch` | both | run several commands in order `{steps: [{id, params}], stop_on_error}` → `{completed, failed, results}` |
| `doc_inspect` | both | the project tree and the active sequence in one call (`project_inspect` + `sequence_inspect`) |
| `render_preview` | both | same as `render_frame` |
| `project_inspect` | both | bins and items with ids, types, durations; active sequence |
| `sequence_inspect` | both | the active sequence: tracks, clips (`start` / `end` and `sourceIn` / `sourceOut` in ticks, frames, `speed`, `reverse`: the clip plays its `sourceIn` to `sourceOut` stretch backward, `gainDb`), effects, transitions, markers (name and `comment`), playhead, selection |
| `media_import` | both | import files by absolute path (`paths`, an array; optional `bin`, `image_sequence`) |
| `render_frame` | both | PNG of the program frame at `seconds` (headless renders; bridge screenshots the Program monitor). Read-only: the playhead and selection are left as they were |
| `ui_inspect` | bridge | UI state: tool, workspace, panels, zoom, playback, fps |
| `ui_elements` | bridge | on-screen interactive elements with id, label and rect (`prefix` filter) |
| `ui_click` | bridge | click by `id` or `x`,`y`; `button`, `count`, `modifiers` |
| `ui_drag` | bridge | press–move–release between points or element ids |
| `ui_key` | bridge | key or shortcut: `Space`, `Cmd+K`, `Shift+Delete` |
| `ui_type` | bridge | type text into the focused field |
| `ui_screenshot` | bridge | PNG of the window or one `panel` |
| `ui_control` | bridge | call any control-channel method directly (`ui.set`, `ui.scroll`, `ui.timeline.locate`, …) |

Typical loop: `project_inspect` / `sequence_inspect` → get ids → `command_run` → `render_frame` or
`ui_screenshot` → look at the result → `edit.undo` if it's wrong.

### Conventions

The server follows a few conventions that make it predictable for agents. They are not
FilmCraft-specific: the sibling craft apps' MCP servers can follow the same ones, so an agent that has
driven one app can drive the others.

- **Core tools.** `command_list`, `command_run`, `command_batch`, `doc_inspect` and `render_preview`
  work in both modes. The FilmCraft tools above stay as they are.
- **Titles and annotations.** Every tool has a `title` and the four MCP hints (`readOnlyHint`,
  `destructiveHint`, `idempotentHint`, `openWorldHint`), so a client can let the read-only tools
  (lists, inspects, renders, screenshots) run without asking and ask before edits. When you add a
  tool, set them in its `#[tool(...)]` attribute; a test checks every listed tool.
- **Strict arguments.** An unknown tool argument is a JSON-RPC `-32602` error that names it and the
  accepted ones (`unknown argument "filtr" for command_list; expected: enabled_only, filter`), not a
  silently ignored key. `command_run.params` goes to the command unchanged.
- **Errors.** A failing command is an `isError` result with the engine's message. A line that is not
  JSON gets a `-32700` parse error (id `null`) and the server keeps serving (over HTTP: a body that
  is not JSON, with HTTP status 400). A command that panics is reported as `internal error: …`, and
  the session stays usable.
- **Resources.** `filmcraft://document` (the project tree and active sequence, as `doc_inspect`) and
  `filmcraft://commands` (the command catalog, as `command_list`), both `application/json`. For MCP
  2026-07-28 clients such as current Claude Code, list and read results carry the `ttlMs` and
  `cacheScope` hints that revision requires.

### Long exports: progress and cancellation

`command_run {"id": "file.exportMedia", "params": {…, "wait": true}}` in headless mode blocks until
the file is written. It uses the MCP progress and cancellation utilities, so a client can show
progress and stop it:

- The export runs as the same background job `jobs.list` shows. The session is locked only for short
  polls, so other requests (`ping`, `doc_inspect`, `jobs.list`, …) are answered while it encodes.
- `file.exportMedia` with `wait: true`, an export job in `jobs.list` and each `export.queue.list`
  item carry `offlineMedia`: the offline items the export drew the placeholder for (see §2b), `[]`
  when none. The export itself still succeeds; deciding what to do about it is up to the caller.
- Every job in `jobs.list`, and every encoding item of `export.queue.list`, carries `etaSeconds`: the
  time left at the job's speed over the last 15 seconds, `null` until a second of progress has been
  measured (a loudness pass, a seek) and once it is done. The app shows it as `37% · 2:05 left` in the
  status bar, the Export queue and the Progress panel.
- With `"_meta": {"progressToken": T}` on the `tools/call`, the server sends
  `notifications/progress` `{progressToken: T, progress: <frames done>, total: <frames>, message:
  <job status>}` at most every 100 ms while frames advance. Without a token it sends none.
- `notifications/cancelled` for the request stops the encode at the next batch (`jobs.cancel`),
  deletes the partial output (the movie, or the frames and caption sidecar written so far) and sends
  no response, as the MCP cancellation utility asks. Over HTTP the progress notifications come on
  the call's SSE stream, and a 2026-07-28 client cancels by closing that stream instead (see
  [HTTP transport](#http-transport)).
- Without `wait` the command returns `{job, path}` at once, as before: poll `jobs.list` and stop it
  with `jobs.cancel`. Other exports (`file.exportFrame`, interchange formats, …) finish quickly and
  ignore the token.
- Bridge mode works the same way: the export runs as an app job (the app stays responsive and shows
  "Exporting… NN%" in its status bar) and the call returns when it is written, however long it
  takes. `filmcraft-cli --bridge … exec file.exportMedia … wait=true` blocks the same way.

## 2. Control channel

`filmcraft --control 9876` (or `FILMCRAFT_CONTROL_PORT=9876`) listens on `127.0.0.1` only. Send one
JSON request per line and get one JSON reply per line. Full method table:
[control-protocol.md](control-protocol.md).

```jsonc
{"id":1,"method":"engine.commands"}
{"id":2,"method":"engine.execute","params":{"command":"file.openDemoProject"}}
{"id":3,"method":"engine.execute","params":{"command":"playhead.set","params":{"seconds":2}}}
{"id":4,"method":"engine.execute","params":{"command":"sequence.addEdit"}}
{"id":5,"method":"engine.execute","params":{"command":"effects.apply","params":{"effect":"Gaussian Blur"}}}
{"id":6,"method":"ui.elements","params":{"prefix":"tools."}}
{"id":7,"method":"ui.click","params":{"id":"tools.Razor"}}
{"id":8,"method":"ui.key","params":{"key":"Cmd+Z"}}
{"id":9,"method":"ui.screenshot","params":{"path":"/tmp/timeline.png","panel":"Timeline"}}
```

Replies look like `{"id":4,"ok":true,"result":{"cuts":2}}` or `{"id":7,"ok":false,"error":"…"}`.
Minimal client:

```python
import json, socket
s = socket.create_connection(("127.0.0.1", 9876)); f = s.makefile("rw")
def call(method, **params):
    f.write(json.dumps({"id": 1, "method": method, "params": params}) + "\n"); f.flush()
    return json.loads(f.readline())
print(call("engine.execute", command="sequence.inspect"))
```

Notes:

- Time is in ticks (254 016 000 000 per second). Commands also accept `seconds`, `frame` or
  `timecode`.
- A track parameter (`track`, `audioTrack`) is a track id or a name such as `"V1"` / `"A2"`.
  Leaving it out picks the command's default track; naming a track the sequence does not have is
  an "invalid parameters" error, never a different track.
  In `timeline.place`, `track` is where the picture goes and `audioTrack` where the sound goes;
  an audio track given as `track` places the sound alone, on that track.
  The clip lands on the frame grid: its start and its end (`time + duration`) each snap to the
  nearest frame.
- `sequence.inspect` reports `durationFrames` as the number of frames export renders: a sequence
  that ends partway through a frame counts that frame.
- `engine.execute` also runs UI-only commands (`tool.razor`, `playback.toggle`,
  `window.workspace.color`, `window.panel.<name>`).
- A command that works on the selection is disabled ("no clips selected", "select a clip in the
  Project panel") until something is selected, unless the call names its targets under a key the
  command documents (`clips` / `clip` for timeline clips, `items` / `item` for project items):
  `clip.replaceFromBin {"clips":[12],"item":4}` runs with nothing selected and leaves the
  selection alone. Only the selection condition is answered by the named targets; every other
  one (an open sequence, a filled clipboard, the right kind of clip) still applies, and ids of
  nothing do not count. `command_list` / `engine.commands` keep reporting the selection's state.
- Element ids come from the previous frame. If an element is missing right after a layout change,
  the app retries on later frames before giving up.
- `timeline.move` takes linked partners along while Linked Selection is on: moving a picture
  clip moves its sound by the same offset (pass `"linked": false` to move only the listed clips).
  It returns `{"moved": [ids]}`, every clip that moved.
- Without a window, `filmcraft-cli run script.jsonl` (lines of `{"id":"…","params":{…}}`) runs the
  same commands headlessly. See §2b for the rest of the CLI.

## 2b. Command-line interface

Every command is also one shell call away. Options go anywhere; output is JSON; exit status is 0 on
success, 1 when a command fails and 2 on a usage error. `filmcraft-cli help`, `filmcraft-cli --help`
and `filmcraft-cli -h` print the reference and exit successfully.
If the reader of stdout closes the pipe early (`filmcraft-cli commands | head`), the CLI drops the rest
of its output but still finishes the work, saves included, and the exit status still reports
failures; any other stdout write error is reported and makes the status 1.

```sh
filmcraft-cli commands razor                     # find ids (add --json for machine output)
filmcraft-cli describe timeline.razor            # one command: menu, shortcut, params, enabled now
filmcraft-cli --demo inspect sequence            # project / sequence as JSON
filmcraft-cli --project p.fcproj --save exec timeline.razor seconds=3.5
filmcraft-cli --project p.fcproj exec effects.apply '{"effect":"Gaussian Blur"}'
filmcraft-cli --project p.fcproj exec markers.exportCsv path=review.csv
filmcraft-cli --project p.fcproj --save import a.mov b.wav
filmcraft-cli --project p.fcproj export out.mp4  # format from the extension; waits for the job
filmcraft-cli --project p.fcproj export out --preset "YouTube 1080p Full HD" --start 0 --end 10
filmcraft-cli export --list-presets prores       # built-in + user presets (--data-dir for another library)
filmcraft-cli --project p.fcproj exec export.queue.add preset="Apple ProRes 422 HQ" path=renders/ start=true wait=true
echo '{"id":"file.newBin","params":{"name":"Selects"}}' | filmcraft-cli --project p.fcproj --save run -
filmcraft-cli --bridge 127.0.0.1:9876 exec window.workspace.color   # the running app
```

`key=value` values are parsed as JSON when they can be (`3.5`, `true`, `[1,2]`), otherwise taken as
strings; dotted keys nest (`color.r=1`). `run` prints one JSON line per command
(`{"line","id","ok","result"|"error"}`) and stops at the first failure unless `--keep-going`.
`--save` writes back to `--project`; `--save-as path` writes elsewhere.

Media that is offline when it is needed (the file is missing, was made offline, or can't be read)
renders as the "media not found" placeholder. `export` and `render` then name each such item on
stderr (`item 8 a.mp4 (missing): /path/a.mp4`) and exit 1 after writing the output, so a moved or
unmounted media folder doesn't pass as a good render. `--allow-offline` accepts the placeholder
frames: the warning stays and the exit status is 0. The export result lists the same items as
`offlineMedia` (`[{item, name, path, status, error}]`, `status` as in `media.status`: `missing`,
`offline` or `unreadable`), and `[]` when every clip it drew was online; check before exporting with
`exec media.status` or `exec media.findMissing`.

`markers.exportCsv {path}` exports all markers in the active sequence, including colours hidden
in the UI and markers outside In/Out, sorted by start then id. The UTF-8 CSV has a header and CRLF
records; quotes, commas and embedded newlines are escaped. Columns are Sequence, Marker ID, Name,
Comment, Kind, Color, Start Timecode, End Timecode, Duration Frames, Start Ticks, Duration Ticks and
Frame Rate (exact numerator/denominator). Color uses the user's display label from Settings ▸
Labels. Timecodes include the sequence's start timecode and
drop-frame setting; End is exclusive, and a point marker's End equals Start. Duration Frames is
rounded down; tick columns preserve sub-frame timing (254016000000 ticks per second).
An empty sequence writes just the header. User text starting with a spreadsheet formula character
(`=`, `+`, `-`, `@`, including after whitespace) gets a leading apostrophe in the exported cell.
The export leaves the project, dirty flag and undo history untouched, works with locked tracks,
and uses the host's file service (atomic
on desktop, a download on web). It exports sequence markers only, not clip/source markers;
reports are limited to 100000 markers and a conservative 16 MiB output budget. Invalid timing or a
failed write reports an error. The menu opens the normal save dialog; cancelling writes nothing.

## 3. Verifying UI work

For any UI change, look at the result:

1. `cargo run --release -p filmcraft -- --control 9876`
2. Drive the feature through the control channel or the MCP bridge: commands, then clicks and drags
   by automation id.
3. Assert on `ui.inspect`, `ui.elements` and `sequence.inspect`.
4. Take `ui.screenshot` (the whole window and the panel you changed) and open the PNG.

Screenshots you commit must follow [AGENTS.md](../AGENTS.md) §1: FilmCraft only, openly licensed
media, sidecar and `ATTRIBUTION.md` entry.

# Part 2: developing FilmCraft

## 4. Orientation, in this order

1. [AGENTS.md](../AGENTS.md): absolute rules (assets, clean-room, licences).
2. [CLAUDE.md](../CLAUDE.md): working instructions and non-negotiables.
3. [ROADMAP.md](../ROADMAP.md) for the summary, then [gaps.md](gaps.md) (the ranked shortfalls) first,
   [target-app-parity.md](target-app-parity.md) and [roadmap.md](roadmap.md) (milestones, current focus).
4. [architecture.md](architecture.md), then the README and tests of the crate you'll touch.
5. [contributing.md](contributing.md) (how to add things, gates) and [testing.md](testing.md).

Maintainers also keep a local planning folder, `plan/`. It is gitignored and not in the repository,
and holds the task-level plan, the status file and behaviour reference notes. If you don't have it,
everything you need to contribute is in the public docs above. Ask a maintainer for a task id.

## 5. Autonomous work loop

1. **Orient.** Pick the next task: the next unchecked task in the maintainer status file, or an open
   [gaps.md](gaps.md) entry. Prefer those gaps (stability, `.prproj`, camera media, measurement,
   hardware, Windows/Linux) over adding more checklist items. Read the relevant
   architecture section and crate README.
2. **Plan tests first.** Write down the acceptance test before writing code.
3. **Implement and test.**
4. **Verify.** Run all gates (`cargo xtask ci`). For UI work, run the app with `--control`, drive it
   and look at the screenshots (§3).
5. **Record.** Update the crate README (behaviour decisions, test results, limitations) and
   the progress docs when a milestone moves (ROADMAP.md, docs/roadmap.md, the relevant parity doc
   and gaps.md, with their timestamps). Commit as `M<n>.<k>: …`, then move on to the next task.

For parallel agents, use one git worktree and one `CARGO_TARGET_DIR` per agent, and keep each crate
with one owner ([contributing.md §5](contributing.md#5-parallel-work-several-agents-or-worktrees)).

## 6. Decision rules

Don't block on choices you can make yourself:

- **Behaviour.** Match the publicly documented and observable behaviour of professional editors
  (help pages, published shortcut lists, using the app). Never inspect application internals
  ([AGENTS.md](../AGENTS.md) §2). If behaviour is undocumented, choose what editors most commonly
  expect and record the choice in the crate README.
- **Ask a human only about:**
  - licences outside the allowlist;
  - spending money;
  - publishing or pushing;
  - personal data;
  - destructive git operations;
  - anything AGENTS.md leaves unclear ("when in doubt, leave it out").

## 7. Definition of Done

A feature is done when:

- [ ] its command(s) are registered with label, menu path, shortcut (if any) and a parameter doc;
- [ ] engine tests cover it, including undo/redo and the disabled case;
- [ ] the UI is wired: menu, panel control and shortcut;
- [ ] every new interactive widget has an automation id;
- [ ] you have driven it through the control channel and reviewed a screenshot;
- [ ] all gates pass (`cargo xtask ci`), and new assets have sidecars;
- [ ] the crate README and the progress docs (ROADMAP.md, docs/gaps.md, the parity docs) are updated where relevant;
- [ ] it is committed with its task id.

### Reporting progress honestly

ROADMAP.md and [target-app-parity.md](target-app-parity.md) keep two numbers: **feature breadth**
(does it exist?) and **ready for real work** (does it hold up?). When you update either:

- say how a number was obtained: *measured* (a test, a diff, a benchmark you ran) or *estimated*;
- count an approximation, a stub or a partly wired setting as such, never as done;
- record known bugs and limitations in the crate README and [gaps.md](gaps.md), not only
  what works;
- benchmarks state the machine, the load average and before/after on the same build;
- don't raise a percentage without evidence a reviewer can rerun.
