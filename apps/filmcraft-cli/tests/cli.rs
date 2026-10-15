//! End-to-end tests of the CLI binary (headless backend).

use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_filmcraft-cli")).args(args).output().expect("spawn filmcraft-cli")
}

fn json_out(o: &Output) -> Value {
    assert!(o.status.success(), "failed: {}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).expect("JSON on stdout")
}

#[test]
fn help_and_usage_errors() {
    let h = cli(&["help"]);
    assert!(h.status.success());
    assert!(String::from_utf8_lossy(&h.stdout).contains("exec <id>"));
    assert_eq!(cli(&[]).status.code(), Some(2));
    assert_eq!(cli(&["frobnicate"]).status.code(), Some(2));
    assert_eq!(cli(&["exec", "file.newBin", "notkv"]).status.code(), Some(2));
    // Invalid --bridge is a usage error (exit 2) for both exec and mcp (#921).
    assert_eq!(cli(&["--bridge", "0.0.0.0:9876", "exec", "inspect.project"]).status.code(), Some(2));
    assert_eq!(cli(&["mcp", "--bridge", "0.0.0.0:9876"]).status.code(), Some(2));
}

/// `filmcraft-cli args` (without `FILMCRAFT_MCP_TOKEN` unless `env` sets it, with a data
/// directory of its own), once it has exited: a usage error that started a server instead fails
/// the test after 30 s rather than hanging it.
fn cli_exit(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut p = Spawned::with_env(args, env);
    let (status, stderr) = p.wait();
    let stdout: Vec<String> = p.out.iter().collect();
    Output { status, stdout: stdout.join("\n").into_bytes(), stderr: stderr.into_bytes() }
}

/// A test folder under the system's temporary folder, removed when the test ends (also when it
/// fails).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("fc-cli-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn is_generated_token(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `mcp --http`: bad, missing or non-loopback addresses with too short a token, `--no-token`
/// anywhere but on loopback alone, and HTTP options without `--http` are usage errors (exit 2),
/// and none of them stores a token; the help lists the options.
#[test]
fn mcp_http_usage_errors() {
    let data = TempDir::new("usage");
    let data_dir = data.0.to_str().unwrap().to_string();
    let no_env = [("FILMCRAFT_DATA_DIR", data_dir.as_str())];
    for args in [
        &["mcp", "--http", "nonsense"][..],
        &["mcp", "--http", "example.com:9000"],
        &["mcp", "--http"],
        &["mcp", "--token-file", "t.txt"],
        &["mcp", "--no-token"],
        &["mcp", "--allow-origin", "http://localhost:6274"],
        &["mcp", "--http", "127.0.0.1:0", "--token-file", "/nonexistent/token"],
        // a missing value never falls back to the stored token or the default data directory
        &["mcp", "--http", "127.0.0.1:0", "--token-file"],
        &["mcp", "--http", "127.0.0.1:0", "--data-dir"],
        // origins need scheme://host:port (no wildcard, no port-less entry matching every port)
        &["mcp", "--http", "127.0.0.1:0", "--allow-origin", "*"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-origin", "http://localhost"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-origin", "http://localhost:*"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-origin", "localhost:6274"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-host", "a b"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-host", "lan.example:http"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-origin", "http://127.1:6274"],
        // --no-token: loopback only, and not with what lets others in, nor with a token
        &["mcp", "--http", "0.0.0.0:0", "--no-token"],
        &["mcp", "--http", "[::]:0", "--no-token"],
        &["mcp", "--http", "192.168.1.5:9000", "--no-token"],
        &["mcp", "--http", "127.0.0.1:0", "--no-token", "--allow-host", "my-tunnel.trycloudflare.com"],
        &["mcp", "--http", "127.0.0.1:0", "--no-token", "--allow-host", "host.docker.internal:9877"],
        &["mcp", "--http", "127.0.0.1:0", "--no-token", "--allow-origin", "null"],
        &["mcp", "--http", "127.0.0.1:0", "--no-token", "--allow-origin", "https://example.com:443"],
        &["mcp", "--http", "127.0.0.1:0", "--no-token", "--token-file", "t.txt"],
        &["mcp", "--http", "127.0.0.1:0", "--no-token=yes"],
    ] {
        let o = cli_exit(args, &no_env);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {err}");
        if args.contains(&"--no-token") {
            assert!(err.contains("--no-token"), "{args:?}: {err}");
        }
    }
    let o = cli_exit(&["mcp", "--http", "127.0.0.1:0", "--no-token"], &[no_env[0], ("FILMCRAFT_MCP_TOKEN", "some-token")]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(err.contains("contradicts FILMCRAFT_MCP_TOKEN") && !err.contains("some-token"), "{err}");
    let o = cli_exit(&["mcp", "--http", "127.0.0.1:0"], &[no_env[0], ("FILMCRAFT_MCP_TOKEN", "has space")]);
    assert_eq!(o.status.code(), Some(2), "{}", String::from_utf8_lossy(&o.stderr));
    // reachable from the network (directly, or through a tunnel): a token too short to resist
    // guessing is refused
    let short = "x".repeat(31);
    for args in [
        &["mcp", "--http", "0.0.0.0:0"][..],
        &["mcp", "--http", "127.0.0.1:0", "--allow-host", "proxy.example:443"],
        &["mcp", "--http", "127.0.0.1:0", "--allow-origin", "null"],
    ] {
        let o = cli_exit(args, &[no_env[0], ("FILMCRAFT_MCP_TOKEN", &short)]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("at least 32 characters") && !err.contains(&short), "{err}");
    }
    assert!(!data.path("mcp-token").exists(), "a usage error stored a token");
    for args in
        [&["mcp-token", "--bogus"][..], &["mcp-token", "extra"], &["mcp-token", "--new", "--path"], &["mcp-token", "--data-dir"], &["mcp-token", "--new=1"]]
    {
        let o = cli_exit(args, &no_env);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
    assert!(!data.path("mcp-token").exists(), "a usage error stored a token");
    let help = String::from_utf8_lossy(&cli(&["help"]).stdout).into_owned();
    for want in ["mcp --http <ADDR>", "--token-file", "--no-token", "mcp-token [--new | --path]", "--allow-origin", "--allow-host", "FILMCRAFT_MCP_TOKEN"] {
        assert!(help.contains(want), "help lacks {want}");
    }
}

/// `PATH` without the folder of the program under test: as for a user, who has no build folder
/// on it (cargo adds it on Windows for tests).
fn path_without_the_program() -> std::ffi::OsString {
    let dir = std::path::Path::new(env!("CARGO_BIN_EXE_filmcraft-cli")).parent().and_then(|d| std::fs::canonicalize(d).ok());
    let path = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<std::path::PathBuf> = std::env::split_paths(&path).filter(|p| std::fs::canonicalize(p).ok() != dir).collect();
    std::env::join_paths(kept).unwrap_or(path)
}

/// The command in backticks that `line` says `prints it`.
fn print_command(line: &str) -> String {
    let before = line.split("` prints it").next().filter(|b| b.len() < line.len()).unwrap_or_else(|| panic!("no `… prints it` in {line}"));
    before.rsplit_once('`').map(|(_, c)| c.to_string()).unwrap_or_else(|| panic!("no command in {line}"))
}

/// Whether `print`, a command the server printed, is `exe` by its full path, quoted for the shell
/// it is printed for (PowerShell runs a quoted path after `&`), then `rest`. The path may be
/// spelled as the system reports the running program (Linux resolves links): the same file.
fn runs_program(print: &str, exe: &std::path::Path, rest: &str) -> bool {
    let Some(quoted) = print.strip_suffix(rest) else { return false };
    let quoted = if cfg!(windows) { quoted.strip_prefix("& ") } else { Some(quoted) };
    let Some(path) = quoted.and_then(|q| q.strip_prefix('\'')).and_then(|q| q.strip_suffix('\'')) else { return false };
    let path = if cfg!(windows) { path.replace("''", "'") } else { path.replace(r"'\''", "'") };
    std::fs::canonicalize(path).ok() == std::fs::canonicalize(exe).ok()
}

/// The program under test.
fn this_program() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_BIN_EXE_filmcraft-cli"))
}

/// The stderr lines a server prints before it serves: up to and including the `claude mcp add`
/// line.
fn banner(p: &Spawned) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let l = Spawned::line(&p.err);
        let done = l.contains("connect Claude Code");
        lines.push(l);
        if done {
            return lines;
        }
    }
}

/// The `host:port` of the first `http://host:port/mcp` URL in the banner.
fn banner_addr(lines: &[String]) -> String {
    url_addr(lines.iter().find(|l| l.contains("/mcp")).unwrap_or_else(|| panic!("no URL in {lines:?}")))
}

/// By default `mcp --http` stores a new random token in the data directory (private, no line
/// break), answers 401 without it and 200 with it, and tells where the token is and how to hand it
/// to Claude Code without ever printing it.
#[test]
fn mcp_http_requires_the_stored_token_by_default() {
    let p = Spawned::new(&["mcp", "--demo", "--http", "127.0.0.1:0"]);
    let lines = banner(&p);
    let addr = banner_addr(&lines);
    let file = p.data_dir().join("mcp-token");
    let token = std::fs::read_to_string(&file).unwrap();
    assert!(is_generated_token(&token), "{token:?}");
    #[cfg(unix)]
    assert_eq!((mode(&file), mode(&p.data_dir())), (0o600, 0o700));
    let shown = file.display().to_string();
    // FILMCRAFT_DATA_DIR is set for this server only: the command names the folder; it runs this
    // program by its full path, as it is not on PATH (the bare name failed on Windows)
    let print = print_command(&lines[0]);
    assert!(runs_program(&print, this_program(), &format!(" mcp-token --data-dir '{}'", p.data_dir().display())), "{print}");
    assert!(lines[0].contains(&format!("bearer token required (the one stored in {shown}; `{print}` prints it)")), "{lines:?}");
    assert_eq!(p.run_printed(&print), token, "the printed command prints the server's token");
    let made = lines.iter().find(|l| l.contains("made a new random MCP token")).unwrap_or_else(|| panic!("{lines:?}"));
    assert!(made.contains(&format!("stored it in {shown}")) && made.ends_with(&format!("; `{print}` prints it, `{print} --new` replaces it")), "{made}");
    // on Windows the folder's permissions decide: in the user's profile, you and administrators
    let private = if cfg!(windows) { ", in your user profile, where only you and administrators can read it;" } else { ", readable only by you;" };
    assert!(made.contains(&format!("stored it in {shown}{private}")), "{made}");
    let paste = lines.last().unwrap();
    let read = if cfg!(windows) { format!("$((Get-Content -LiteralPath '{shown}' -TotalCount 1).Trim())") } else { format!("$(cat '{shown}')") };
    assert!(
        paste.ends_with(&format!("claude mcp add --transport http --scope user filmcraft-http http://{addr}/mcp --header \"Authorization: Bearer {read}\"")),
        "{paste}"
    );
    assert!(lines.iter().all(|l| !l.contains(&token)), "the banner shows the token: {lines:?}");
    let refused = http_initialize(&addr, None);
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
    assert!(http_initialize(&addr, Some("0".repeat(64).as_str())).starts_with("HTTP/1.1 401"));
    let reply = http_initialize(&addr, Some(&token));
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.to_ascii_lowercase().contains("mcp-session-id:"), "{reply}");
    assert!(reply.contains(r#""name":"filmcraft""#), "{reply}");
    // nothing printed later shows it either (the refusal log)
    while let Ok(l) = p.err.recv_timeout(std::time::Duration::from_millis(300)) {
        assert!(!l.contains(&token), "{l}");
    }
}

/// A restarted server, and `mcp-token`, use the token stored the first time; `mcp-token --new`
/// replaces it (the next server takes the new one) and `--path` says where it is.
#[test]
fn the_stored_token_is_reused_printed_and_replaced() {
    let data = TempDir::new("reuse");
    let dir = data.0.to_str().unwrap().to_string();
    let env = [("FILMCRAFT_DATA_DIR", dir.as_str())];
    let file = data.path("mcp-token");
    let printed = cli_exit(&["mcp-token"], &env);
    assert!(printed.status.success(), "{}", String::from_utf8_lossy(&printed.stderr));
    let token = String::from_utf8(printed.stdout).unwrap();
    assert!(is_generated_token(&token), "{token:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), token);
    assert!(String::from_utf8_lossy(&printed.stderr).contains("made a new MCP token") && !String::from_utf8_lossy(&printed.stderr).contains(&token));
    #[cfg(unix)]
    assert_eq!(mode(&file), 0o600);
    for _ in 0..2 {
        let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0"], &env);
        let lines = banner(&p);
        assert!(!lines.iter().any(|l| l.contains("made a new")), "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains(&token)));
        assert!(http_initialize(&banner_addr(&lines), Some(&token)).starts_with("HTTP/1.1 200"));
    }
    let again = cli_exit(&["mcp-token"], &env);
    assert_eq!(String::from_utf8(again.stdout).unwrap(), token);
    assert!(again.stderr.is_empty(), "{}", String::from_utf8_lossy(&again.stderr));
    let path = cli_exit(&["mcp-token", "--path"], &env);
    assert_eq!(String::from_utf8(path.stdout).unwrap(), file.display().to_string());
    let rotated = cli_exit(&["mcp-token", "--new"], &env);
    assert!(rotated.status.success());
    let new = String::from_utf8(rotated.stdout).unwrap();
    assert!(is_generated_token(&new) && new != token);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), new);
    assert!(!String::from_utf8_lossy(&rotated.stderr).contains(&new));
    #[cfg(unix)]
    assert_eq!(mode(&file), 0o600);
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0"], &env);
    let addr = banner_addr(&banner(&p));
    assert!(http_initialize(&addr, Some(&token)).starts_with("HTTP/1.1 401"));
    assert!(http_initialize(&addr, Some(&new)).starts_with("HTTP/1.1 200"));
    // --data-dir picks another one, for the server and for mcp-token
    let other = data.path("other dir");
    let other_s = other.to_str().unwrap();
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0", "--data-dir", other_s], &env);
    let lines = banner(&p);
    let other_token = std::fs::read_to_string(other.join("mcp-token")).unwrap();
    assert!(is_generated_token(&other_token) && other_token != new);
    assert!(lines[0].contains("--data-dir") && lines[0].contains("other dir"), "{lines:?}");
    assert!(http_initialize(&banner_addr(&lines), Some(&other_token)).starts_with("HTTP/1.1 200"));
    assert_eq!(String::from_utf8(cli_exit(&["mcp-token", "--data-dir", other_s], &env).stdout).unwrap(), other_token);
    // FILMCRAFT_MCP_TOKEN set: mcp-token still prints the stored one, and says the server uses the variable
    let noted = cli_exit(&["mcp-token"], &[env[0], ("FILMCRAFT_MCP_TOKEN", "from-the-environment-0123456789-abc")]);
    assert_eq!(String::from_utf8(noted.stdout).unwrap(), new);
    assert!(String::from_utf8_lossy(&noted.stderr).contains("FILMCRAFT_MCP_TOKEN is set"));
}

/// Without `FILMCRAFT_DATA_DIR` or `--data-dir`, the token is stored in the app's per-user data
/// directory (here under a test home); when there is no usable one, the server and `mcp-token`
/// fail (exit 1) and say how to give a token instead.
#[test]
fn the_stored_token_lives_in_the_per_user_data_directory() {
    let home = TempDir::new("home");
    let env: Vec<(&str, String)> = vec![
        ("FILMCRAFT_DATA_DIR", String::new()),
        ("HOME", home.path("home").display().to_string()),
        ("XDG_DATA_HOME", home.path("xdg").display().to_string()),
        ("APPDATA", home.path("appdata").display().to_string()),
    ];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // one component at a time: a path written with `/` is not what Windows prints, with `\`
    let want = if cfg!(target_os = "macos") {
        home.path("home").join("Library").join("Application Support").join("FilmCraft").join("mcp-token")
    } else if cfg!(windows) {
        home.path("appdata").join("FilmCraft").join("mcp-token")
    } else {
        home.path("xdg").join("filmcraft").join("mcp-token")
    };
    let o = cli_exit(&["mcp-token", "--path"], &env);
    assert_eq!(std::path::PathBuf::from(String::from_utf8(o.stdout).unwrap()), want);
    assert!(!want.exists(), "--path stores nothing");
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0"], &env);
    let lines = banner(&p);
    let token = std::fs::read_to_string(&want).unwrap();
    assert!(is_generated_token(&token));
    // where `mcp-token` looks from any shell: no --data-dir needed
    let print = print_command(&lines[0]);
    assert!(runs_program(&print, this_program(), " mcp-token"), "{print}");
    assert!(lines[0].contains(&format!("the one stored in {}; `{print}` prints it", want.display())), "{lines:?}");
    assert_eq!(p.run_printed(&print), token);
    drop(p);
    // with the program first on PATH, by its name
    let dir = this_program().parent().unwrap().to_path_buf();
    let path = std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path_without_the_program()))).unwrap();
    let path = path.to_str().unwrap();
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0"], &[&env[..], &[("PATH", path)]].concat());
    let lines = banner(&p);
    assert!(lines[0].contains(&format!("the one stored in {}; `filmcraft-cli mcp-token` prints it", want.display())), "{lines:?}");
    assert_eq!(p.run_printed("filmcraft-cli mcp-token"), token);
    drop(p);
    // a relative XDG_DATA_HOME or HOME would put the token in the current folder: refused
    let cwd = home.path("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let relative = [("FILMCRAFT_DATA_DIR", ""), ("HOME", "rel-home"), ("XDG_DATA_HOME", "rel-xdg"), ("APPDATA", "rel-appdata")];
    for args in [&["mcp", "--http", "127.0.0.1:0"][..], &["mcp-token"], &["mcp-token", "--path"]] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_filmcraft-cli"));
        cmd.args(args).envs(relative).current_dir(&cwd);
        let mut p = Spawned::spawn(cmd);
        let (status, err) = p.wait();
        assert_eq!(status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains("not an absolute path") && err.contains("--data-dir"), "{args:?}: {err}");
    }
    assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0, "nothing stored in the current folder");
    // a data directory that cannot be made (a file is in the way)
    let blocked = home.path("blocked");
    std::fs::write(&blocked, b"not a folder").unwrap();
    let blocked_s = blocked.to_str().unwrap();
    for args in [&["mcp", "--http", "127.0.0.1:0"][..], &["mcp-token"]] {
        let o = cli_exit(args, &[("FILMCRAFT_DATA_DIR", blocked_s)]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains("mcp-token") && !err.contains("panicked"), "{err}");
    }
    let o = cli_exit(&["mcp", "--http", "127.0.0.1:0"], &[("FILMCRAFT_DATA_DIR", blocked_s)]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--token-file") && err.contains("FILMCRAFT_MCP_TOKEN"), "{err}");
    // an unusable stored token: the command that replaces it names its data directory, here given
    // with --data-dir (`mcp-token --new` alone would replace the default one)
    let bad = home.path("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("mcp-token"), "\r\n").unwrap();
    let again = format!("mcp-token --data-dir '{}' --new", bad.display());
    for args in [&["mcp", "--http", "127.0.0.1:0", "--data-dir", bad.to_str().unwrap()][..], &["mcp-token", "--data-dir", bad.to_str().unwrap()]] {
        let o = cli_exit(args, &env);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains("unusable") && err.contains(&again), "{args:?}: {err}");
    }
}

/// A portable install's program (`portable.txt` beside it) finds its own `data` folder and no
/// other. Pointed at the per-user folder with `--data-dir`, as the Windows warning and the docs
/// advise on a shared PC, it names that folder in the `mcp-token` command it prints; for its own
/// folder it needs none. Run as printed, the command prints the server's token. It used to leave
/// `--data-dir` out for the per-user folder: that printed (and made) another token, in the
/// portable folder, which the server refused.
#[test]
fn a_portable_install_prints_commands_for_its_own_data_folder() {
    let root = TempDir::new("portable");
    let bin = root.path("FilmCraft portable");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_filmcraft-cli"));
    let copy = bin.join(exe.file_name().unwrap());
    if std::fs::hard_link(exe, &copy).is_err() {
        return; // the temporary folder is on another drive or file system
    }
    std::fs::write(bin.join("portable.txt"), "").unwrap();
    let homes: Vec<(&str, String)> =
        ["HOME", "XDG_DATA_HOME", "APPDATA", "LOCALAPPDATA"].iter().map(|k| (*k, root.path(&k.to_ascii_lowercase()).display().to_string())).collect();
    let mut env: Vec<(&str, &str)> = homes.iter().map(|(k, v)| (*k, v.as_str())).collect();
    env.push(("FILMCRAFT_DATA_DIR", ""));
    let per_user = String::from_utf8(cli_exit(&["mcp-token", "--path"], &env).stdout).unwrap();
    let per_user = std::path::Path::new(&per_user).parent().unwrap().to_path_buf();
    let spawn = |args: &[&str]| {
        let mut cmd = Command::new(&copy);
        cmd.args(args).envs(env.iter().copied());
        Spawned::spawn(cmd)
    };
    // the per-user folder: named
    let p = spawn(&["mcp", "--demo", "--http", "127.0.0.1:0", "--data-dir", per_user.to_str().unwrap()]);
    let print = print_command(&banner(&p)[0]);
    assert!(runs_program(&print, &copy, &format!(" mcp-token --data-dir '{}'", per_user.display())), "{print}");
    let token = std::fs::read_to_string(per_user.join("mcp-token")).unwrap();
    assert_eq!(p.run_printed(&print), token);
    assert!(!bin.join("data").join("mcp-token").exists(), "no other token made");
    drop(p);
    // its own folder: found without --data-dir
    let p = spawn(&["mcp", "--demo", "--http", "127.0.0.1:0"]);
    let print = print_command(&banner(&p)[0]);
    assert!(runs_program(&print, &copy, " mcp-token"), "{print}");
    let own = std::fs::read_to_string(bin.join("data").join("mcp-token")).unwrap();
    assert_ne!(own, token);
    assert_eq!(p.run_printed(&print), own);
}

/// `FILMCRAFT_MCP_TOKEN` comes first, then `--token-file`, then the stored token; the stored one
/// is not made while another is used.
#[test]
fn mcp_http_token_precedence() {
    let data = TempDir::new("precedence");
    let dir = data.0.to_str().unwrap().to_string();
    let env_token = "env-token-0123456789-0123456789-abcdef";
    let file_token = "file-token-0123456789-0123456789-abcde";
    let file = data.path("token.txt");
    std::fs::write(&file, format!("{file_token}\n")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let f = file.to_str().unwrap();
    let p =
        Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0", "--token-file", f], &[("FILMCRAFT_DATA_DIR", &dir), ("FILMCRAFT_MCP_TOKEN", env_token)]);
    let lines = banner(&p);
    assert!(lines.iter().any(|l| l.contains("warning: FILMCRAFT_MCP_TOKEN is set") && l.contains("--token-file")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("bearer token required (from FILMCRAFT_MCP_TOKEN)")), "{lines:?}");
    let paste = lines.last().unwrap();
    let var = if cfg!(windows) { "$env:FILMCRAFT_MCP_TOKEN" } else { "$FILMCRAFT_MCP_TOKEN" };
    assert!(paste.ends_with(&format!("--header \"Authorization: Bearer {var}\"")), "{paste}");
    assert!(lines.iter().all(|l| !l.contains(env_token) && !l.contains(file_token)), "{lines:?}");
    let addr = banner_addr(&lines);
    assert!(http_initialize(&addr, Some(file_token)).starts_with("HTTP/1.1 401"));
    assert!(http_initialize(&addr, Some(env_token)).starts_with("HTTP/1.1 200"));
    drop(p);
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0", "--token-file", f], &[("FILMCRAFT_DATA_DIR", &dir)]);
    let lines = banner(&p);
    let addr = banner_addr(&lines);
    assert!(http_initialize(&addr, Some(file_token)).starts_with("HTTP/1.1 200"));
    assert!(lines.iter().all(|l| !l.contains(file_token)), "{lines:?}");
    assert!(!data.path("mcp-token").exists(), "the stored token is made only when used");
}

/// `--no-token` on loopback serves without a token, warns that any local program can drive
/// FilmCraft, and prints the command without a header.
#[test]
fn mcp_http_no_token_on_loopback() {
    let p = Spawned::new(&["mcp", "--demo", "--http", "127.0.0.1:0", "--no-token"]);
    let lines = banner(&p);
    assert!(lines[0].contains("no token (--no-token)"), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("warning: --no-token: any program on this machine")), "{lines:?}");
    let addr = banner_addr(&lines);
    assert!(lines.last().unwrap().ends_with(&format!("claude mcp add --transport http --scope user filmcraft-http http://{addr}/mcp")), "{lines:?}");
    assert!(http_initialize(&addr, None).starts_with("HTTP/1.1 200"));
    assert!(!p.data_dir().join("mcp-token").exists(), "--no-token stores no token");
}

/// The `host:port` of the `http://host:port/mcp` URL in a banner line.
fn url_addr(banner: &str) -> String {
    banner.split("http://").nth(1).and_then(|r| r.split("/mcp").next()).unwrap_or_else(|| panic!("no URL in {banner:?}")).to_string()
}

/// POST `initialize` to `http://addr/mcp` (with the bearer `token`), on a connection that gives up
/// after a minute either way; the whole reply.
fn http_initialize(addr: &str, token: Option<&str>) -> String {
    let body =
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(60))).unwrap();
    s.set_write_timeout(Some(std::time::Duration::from_secs(60))).unwrap();
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{auth}Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut s, &mut reply).unwrap();
    reply
}

/// `--http localhost:PORT` listens on 127.0.0.1 and, where the system has IPv6, on [::1] at the
/// same port (clients may resolve `localhost` to either); without IPv6 it still starts. The
/// banner gives the 127.0.0.1 URL, and the last line the `claude mcp add` command for it.
#[test]
fn mcp_http_on_localhost_listens_on_both_loopback_addresses() {
    let p = Spawned::new(&["mcp", "--demo", "--http", "localhost:0"]);
    let lines = banner(&p);
    let addr = banner_addr(&lines);
    assert!(addr.starts_with("127.0.0.1:"), "{lines:?}");
    let port = addr.trim_start_matches("127.0.0.1:");
    assert!(lines.last().unwrap().contains(&format!("claude mcp add --transport http --scope user filmcraft-http http://{addr}/mcp --header")), "{lines:?}");
    let token = p.stored_token();
    assert!(http_initialize(&addr, Some(&token)).starts_with("HTTP/1.1 200"));
    let ipv6 = std::net::TcpListener::bind("[::1]:0").is_ok();
    assert_eq!(lines[0].contains(&format!("(and [::1]:{port})")), ipv6, "{lines:?}");
    if ipv6 {
        let v6 = format!("[::1]:{port}");
        assert!(http_initialize(&v6, None).starts_with("HTTP/1.1 401"));
        let reply = http_initialize(&v6, Some(&token));
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    }
}

/// `--token-file` reads what Windows PowerShell 5.1 writes (`'token' > token.txt`: UTF-16 with a
/// byte-order mark and CRLF) as the token, and warns, without showing it, when other users can
/// read the file.
#[test]
fn mcp_http_token_file_in_utf16_with_a_permission_warning() {
    let token = "token-file-test-0123456789-abcdefghij";
    let dir = TempDir::new("token-file");
    let file = dir.path("token.txt");
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(format!("{token}\r\n").encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&file, utf16).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let p = Spawned::new(&["mcp", "--demo", "--http", "127.0.0.1:0", "--token-file", file.to_str().unwrap()]);
    let mut line = Spawned::line(&p.err);
    if cfg!(unix) {
        assert!(line.contains("warning: the token file") && line.contains("can be read by other users") && line.contains("chmod 600"), "{line}");
        assert!(!line.contains(token), "{line}");
        line = Spawned::line(&p.err);
    }
    assert!(line.contains("bearer token required") && !line.contains(token), "{line}");
    let addr = url_addr(&line);
    let paste = Spawned::line(&p.err);
    let shown = file.display();
    let read = if cfg!(windows) {
        format!("$((Get-Content -LiteralPath '{shown}' -TotalCount 1).Trim())")
    } else {
        format!(r"$(LC_ALL=C tr -d ' \t\r\000\357\273\277\376\377' < '{shown}' | head -n 1)")
    };
    assert!(
        paste.contains(&format!("claude mcp add --transport http --scope user filmcraft-http http://{addr}/mcp --header \"Authorization: Bearer {read}\"")),
        "{paste}"
    );
    assert!(!paste.contains(token), "{paste}");
    assert!(http_initialize(&addr, None).starts_with("HTTP/1.1 401"));
    let reply = http_initialize(&addr, Some(token));
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(!p.data_dir().join("mcp-token").exists());
    drop(p);
    // a file with no token in it is a usage error that says how to write one
    for bytes in [&b""[..], b"\r\n", &[0x41, 0, 0x42, 0]] {
        std::fs::write(&file, bytes).unwrap();
        let o = cli_exit(&["mcp", "--http", "127.0.0.1:0", "--token-file", file.to_str().unwrap()], &[]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{err}");
        assert!(err.contains("Set-Content -Encoding ascii"), "{err}");
    }
}

/// A spawned `filmcraft-cli`, killed when the test ends (also when it fails), with its stdout
/// and stderr lines arriving on channels so a test never blocks on a silent child. It runs with
/// a data directory and home of its own (unless the test sets them), never the user's.
struct Spawned {
    child: std::process::Child,
    // only the unix-only signal test writes to it
    #[cfg_attr(not(unix), allow(dead_code))]
    stdin: Option<std::process::ChildStdin>,
    out: std::sync::mpsc::Receiver<String>,
    err: std::sync::mpsc::Receiver<String>,
    /// Its own folder, with its default data directory (`data`) and home.
    root: TempDir,
    /// The environment it was given on top of this process's.
    env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
}

impl Spawned {
    fn new(args: &[&str]) -> Self {
        Self::with_env(args, &[])
    }
    fn with_env(args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_filmcraft-cli"));
        cmd.args(args).envs(env.iter().copied());
        Self::spawn(cmd)
    }
    /// `cmd` (filmcraft-cli, or a program that runs it) without `FILMCRAFT_MCP_TOKEN` unless set,
    /// and in a folder of its own for the data directory and home unless set. Its user profile
    /// (Windows) is the temporary folder, where every test folder is, and the folder of the
    /// program under test is not on its `PATH` (cargo puts it there on Windows), unless set.
    fn spawn(mut cmd: Command) -> Self {
        use std::io::BufRead;
        let root = TempDir::new("spawned");
        let set = |k: &str| cmd.get_envs().any(|(n, _)| n == k);
        let (token_set, own) = (set("FILMCRAFT_MCP_TOKEN"), ["FILMCRAFT_DATA_DIR", "HOME", "XDG_DATA_HOME", "APPDATA", "LOCALAPPDATA"].map(|k| (k, !set(k))));
        let (profile_set, path_set) = (set("USERPROFILE"), set("PATH"));
        if !token_set {
            cmd.env_remove("FILMCRAFT_MCP_TOKEN");
        }
        for (k, unset) in own {
            if unset {
                let dir = if k == "FILMCRAFT_DATA_DIR" { "data".to_string() } else { k.to_ascii_lowercase() };
                cmd.env(k, root.path(&dir));
            }
        }
        if !profile_set {
            cmd.env("USERPROFILE", std::env::temp_dir());
        }
        if !path_set {
            cmd.env("PATH", path_without_the_program());
        }
        let env = cmd.get_envs().map(|(k, v)| (k.to_os_string(), v.map(std::ffi::OsStr::to_os_string))).collect();
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn filmcraft-cli");
        let lines = |r: Box<dyn std::io::Read + Send>| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for l in std::io::BufReader::new(r).lines().map_while(Result::ok) {
                    let _ = tx.send(l);
                }
            });
            rx
        };
        let out = lines(Box::new(child.stdout.take().unwrap()));
        let err = lines(Box::new(child.stderr.take().unwrap()));
        Self { stdin: child.stdin.take(), child, out, err, root, env }
    }
    /// Run `cmd`, a command line it printed, as its user would in another shell: in PowerShell on
    /// Windows and sh elsewhere, with its home, profile and `PATH`, but without the
    /// `FILMCRAFT_DATA_DIR` or token it was started with. What it printed on stdout, trimmed.
    fn run_printed(&self, cmd: &str) -> String {
        let mut c = if cfg!(windows) {
            // a script file: no command-line quoting in between; UTF-8 with a byte-order mark
            let ps1 = self.root.path("printed.ps1");
            std::fs::write(&ps1, [&[0xEF, 0xBB, 0xBF][..], cmd.as_bytes()].concat()).unwrap();
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(ps1);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", cmd]);
            c
        };
        for (k, v) in &self.env {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c.env_remove("FILMCRAFT_DATA_DIR").env_remove("FILMCRAFT_MCP_TOKEN");
        let o = c.output().expect("run the printed command");
        assert!(o.status.success(), "{cmd}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    /// Its default data directory (when the test did not set `FILMCRAFT_DATA_DIR`).
    fn data_dir(&self) -> std::path::PathBuf {
        self.root.path("data")
    }
    /// The token it stored in its default data directory.
    fn stored_token(&self) -> String {
        std::fs::read_to_string(self.data_dir().join("mcp-token")).unwrap()
    }
    fn line(rx: &std::sync::mpsc::Receiver<String>) -> String {
        rx.recv_timeout(std::time::Duration::from_secs(60)).expect("a line in time")
    }
    /// Wait for the process to end; its status and the rest of stderr.
    fn wait(&mut self) -> (std::process::ExitStatus, String) {
        let t0 = std::time::Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(30), "still running");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let rest: Vec<String> = self.err.iter().collect();
        (status, rest.join("\n"))
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `mcp --http 0.0.0.0:PORT` (all interfaces, a token required) prints a URL a client can use
/// (loopback, not `http://0.0.0.0:PORT`), the Host values it accepts, and what LAN clients need.
#[test]
fn mcp_http_on_every_interface_prints_usable_addresses() {
    let token = "banner-test-token-0123456789-abcdefgh";
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "0.0.0.0:0"], &[("FILMCRAFT_MCP_TOKEN", token)]);
    let banner = Spawned::line(&p.err);
    let addr = url_addr(&banner);
    assert!(addr.starts_with("127.0.0.1:"), "{banner}");
    let hosts = Spawned::line(&p.err);
    assert!(hosts.contains("listening on every interface (0.0.0.0:") && hosts.contains(&format!("Host must be one of: {addr}")), "{hosts}");
    let lan = Spawned::line(&p.err);
    assert!(lan.contains("LAN clients get 403 until you add --allow-host "), "{lan}");
    assert!(!banner.contains(token) && !hosts.contains(token) && !lan.contains(token));
    // the printed URL works
    let reply = http_initialize(&addr, Some(token));
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
}

/// The stored token is long enough for an address the network reaches, so `0.0.0.0` works with
/// nothing else given; it is the same token as on loopback.
#[test]
fn mcp_http_on_every_interface_with_the_stored_token() {
    let p = Spawned::new(&["mcp", "--demo", "--http", "0.0.0.0:0"]);
    let lines = banner(&p);
    let addr = banner_addr(&lines);
    assert!(lines.iter().any(|l| l.contains("reachable from the network and HTTP is not encrypted")), "{lines:?}");
    let token = p.stored_token();
    assert!(lines.iter().all(|l| !l.contains(&token)));
    assert!(http_initialize(&addr, None).starts_with("HTTP/1.1 401"));
    assert!(http_initialize(&addr, Some(&token)).starts_with("HTTP/1.1 200"));
}

/// This machine's LAN address, if it has one (as the server finds it).
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// The banner knows an `--allow-host` without a port matches every port (LAN clients then get
/// in), and says that requests for a tunnel's or proxy's name come through it, from its address.
#[test]
fn mcp_http_banner_for_lan_and_relayed_hosts() {
    let token = "banner-test-token-0123456789-abcdefgh";
    if let Some(ip) = lan_ip() {
        let ip = ip.to_string();
        let p = Spawned::with_env(&["mcp", "--demo", "--http", "0.0.0.0:0", "--allow-host", &ip], &[("FILMCRAFT_MCP_TOKEN", token)]);
        let lines: Vec<String> = (0..3).map(|_| Spawned::line(&p.err)).collect();
        assert!(lines.iter().any(|l| l.contains(&format!("LAN clients: http://{ip}:"))), "{lines:?}");
    }
    let p = Spawned::with_env(&["mcp", "--demo", "--http", "127.0.0.1:0", "--allow-host", "proxy.example:443"], &[("FILMCRAFT_MCP_TOKEN", token)]);
    let banner = Spawned::line(&p.err);
    assert!(banner.contains("bearer token required"), "{banner}");
    let relayed = Spawned::line(&p.err);
    assert!(relayed.contains("requests for proxy.example:443 arrive through a tunnel or proxy"), "{relayed}");
}

/// `mcp` stops cleanly on SIGTERM and SIGHUP (kill, systemd, docker stop, a closed terminal) as
/// on Ctrl+C (SIGINT), instead of dying at once and leaving partial exports: it says so and exits
/// 0, over HTTP and on stdio.
#[cfg(unix)]
#[test]
fn mcp_stops_cleanly_on_sigterm_and_sighup() {
    let initialize =
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    for (signal, http) in [("TERM", true), ("HUP", true), ("INT", true), ("TERM", false)] {
        let mut args = vec!["mcp", "--demo"];
        if http {
            args.extend(["--http", "127.0.0.1:0"]);
        }
        let mut p = Spawned::new(&args);
        if http {
            let banner = Spawned::line(&p.err);
            assert!(banner.contains("http://127.0.0.1:"), "{banner}");
        } else {
            // up once it answers the handshake (stdin stays open: EOF would stop it too)
            writeln!(p.stdin.as_mut().unwrap(), "{initialize}").unwrap();
            let reply = Spawned::line(&p.out);
            assert!(reply.contains(r#""id":0"#), "{reply}");
            assert!(!p.data_dir().join("mcp-token").exists(), "stdio needs no token");
        }
        let kill = Command::new("kill").args([format!("-{signal}"), p.child.id().to_string()]).status().expect("run kill");
        assert!(kill.success());
        let (status, err) = p.wait();
        assert_eq!(status.code(), Some(0), "SIG{signal}, http {http}: {err}");
        let said = if signal == "INT" { "stopping (Ctrl+C)".to_string() } else { format!("stopping (SIG{signal})") };
        assert!(err.contains(&said), "{err}");
    }
}

/// Under `nohup` (SIGHUP ignored) the server keeps running when the terminal hangs up, as it
/// did before it watched SIGHUP; SIGTERM still stops it cleanly.
#[cfg(target_os = "linux")]
#[test]
fn mcp_under_nohup_ignores_sighup() {
    let mut cmd = Command::new("nohup");
    cmd.args([env!("CARGO_BIN_EXE_filmcraft-cli"), "mcp", "--demo", "--http", "127.0.0.1:0"]);
    let mut p = Spawned::spawn(cmd);
    let mut banner = Spawned::line(&p.err);
    while !banner.contains("http://127.0.0.1:") {
        banner = Spawned::line(&p.err); // nohup's own note about its input
    }
    let addr = url_addr(&banner);
    let pid = p.child.id().to_string(); // nohup execs the program: the same process
    assert!(Command::new("kill").args(["-HUP", &pid]).status().expect("run kill").success());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(p.child.try_wait().unwrap().is_none(), "still running");
    assert!(http_initialize(&addr, Some(&p.stored_token())).starts_with("HTTP/1.1 200"));
    assert!(Command::new("kill").args(["-TERM", &pid]).status().expect("run kill").success());
    let (status, err) = p.wait();
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(err.contains("stopping (SIGTERM)") && !err.contains("SIGHUP"), "{err}");
}

#[test]
fn version_after_global_options() {
    let want = format!("filmcraft-cli {}\n", env!("CARGO_PKG_VERSION"));
    for args in [&["--version"][..], &["-V"], &["version"], &["--demo", "--version"], &["--compact", "-V"], &["--demo", "version"]] {
        let o = cli(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(String::from_utf8_lossy(&o.stdout), want, "{args:?}");
    }
}

#[test]
fn long_help_matches_help_command() {
    let reference = cli(&["help"]);
    assert!(reference.status.success());
    let long_help = cli(&["--help"]);
    assert!(long_help.status.success(), "--help failed: {}", String::from_utf8_lossy(&long_help.stderr));
    assert_eq!(long_help.stdout, reference.stdout);
    assert!(long_help.stderr.is_empty());
    let short_help = cli(&["-h"]);
    assert!(short_help.status.success());
    assert_eq!(short_help.stdout, reference.stdout);
    let sub_help = cli(&["export", "--help"]);
    assert!(sub_help.status.success(), "export --help failed: {}", String::from_utf8_lossy(&sub_help.stderr));
    assert_eq!(sub_help.stdout, reference.stdout);
}

#[test]
fn probe_reports_mpeg_transport_and_program_streams() {
    let Some(ffmpeg) = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"].into_iter().find(|p| std::path::Path::new(p).exists()) else {
        eprintln!("SKIPPED (probe_reports_mpeg_transport_and_program_streams): ffmpeg not found");
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/cli-tests").join(format!("mpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, fmt, extra) in [("a.ts", "mpegts", &["-mpegts_m2ts_mode", "0"][..]), ("b.vob", "vob", &[][..])] {
        let out = dir.join(name);
        let st = Command::new(ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", "0.5", "-c:v", "mpeg2video", "-c:a", "mp2", "-ac", "2", "-f", fmt])
            .args(extra)
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        let v = json_out(&cli(&["probe", out.to_str().unwrap()]));
        assert!(v["video"]["codec"].as_str().unwrap().starts_with("MPEG-2 Video"), "{v}");
        assert_eq!(v["audio_streams"][0]["codec"], "MPEG Audio");
        let streams = v["mpeg"]["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2, "{v}");
        assert_eq!(streams[0]["codec"], "MPEG-2 Video");
        assert!(streams[0]["pictures"]["I"].as_u64().unwrap() >= 1);
        assert_eq!(v["mpeg"]["format"], if fmt == "mpegts" { "MPEG-2 TS" } else { "MPEG-2 PS" });
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exec_inspect_and_describe() {
    let seq = json_out(&cli(&["--demo", "inspect", "sequence", "--compact"]));
    assert!(seq["tracks"].is_array() || seq.is_object(), "{seq}");
    let d = json_out(&cli(&["describe", "timeline.razor"]));
    assert_eq!(d["id"], "timeline.razor");
    assert!(d["params"].is_string());
    let list = json_out(&cli(&["commands", "razor", "--json"]));
    assert!(list.as_array().unwrap().iter().any(|c| c["id"] == "timeline.razor"));
    // An unknown command fails with exit status 1.
    let bad = cli(&["--demo", "exec", "no.such.command"]);
    assert_eq!(bad.status.code(), Some(1));
}

#[test]
fn exec_save_as_then_reopen() {
    let dir = std::env::temp_dir().join(format!("fc-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let proj = dir.join("p.fcproj");
    let p = proj.to_str().unwrap();
    json_out(&cli(&["--demo", "exec", "file.newBin", "name=CLI Selects", "--save-as", p]));
    let tree = json_out(&cli(&["--project", p, "inspect", "project"]));
    assert!(tree.to_string().contains("CLI Selects"), "{tree}");
    // `run` from stdin, editing and saving back in place.
    let mut child = Command::new(env!("CARGO_BIN_EXE_filmcraft-cli"))
        .args(["--project", p, "--save", "run", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"# comment\n{\"id\":\"file.newBin\",\"params\":{\"name\":\"Second\"}}\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let line: Value = serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(line["ok"], true);
    assert!(json_out(&cli(&["--project", p, "inspect", "project"])).to_string().contains("Second"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_keep_going_reports_failures() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_filmcraft-cli"))
        .args(["--demo", "--keep-going", "run", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{\"id\":\"no.such\"}\n{\"id\":\"file.newBin\",\"params\":{\"name\":\"x\"}}\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ok"], false);
    assert_eq!(lines[1]["ok"], true);
}

#[test]
fn export_wav_waits_for_job() {
    let dir = std::env::temp_dir().join(format!("fc-cli-x-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("still.wav");
    let o = cli(&["--demo", "export", out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(std::fs::metadata(&out).map(|m| m.len() > 44).unwrap_or(false));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `export --preset`: a built-in preset by name (dash or en dash), a custom range, a user preset
/// from `--data-dir`, `--list-presets`, `--queue`, and a clean failure for an unknown preset.
#[test]
fn export_with_presets() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/export-tests").join(format!("cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");
    let data_s = data.to_str().unwrap();
    let out = dir.join("proxy.mov");
    let o = cli(&["--demo", "--data-dir", data_s, "export", out.to_str().unwrap(), "--preset", "Apple ProRes 422 Proxy", "--start", "0", "--end", "0.25"]);
    let v = json_out(&o);
    assert_eq!(v["preset"], "Apple ProRes 422 Proxy");
    assert!(std::fs::metadata(&out).map(|m| m.len() > 1000).unwrap_or(false));
    if let Some(ffprobe) = ["/opt/homebrew/bin/ffprobe", "/usr/local/bin/ffprobe", "/usr/bin/ffprobe"].into_iter().find(|p| std::path::Path::new(p).exists()) {
        let p = Command::new(ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,profile,width", "-of", "csv=p=0", out.to_str().unwrap()])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&p.stdout).trim(), "prores,Proxy,1920");
    }
    // the extension comes from the preset; a hyphen finds the en-dash name
    let base = dir.join("adaptive");
    json_out(&cli(&[
        "--demo",
        "--data-dir",
        data_s,
        "export",
        base.to_str().unwrap(),
        "--preset",
        "Match Source - Adaptive Low Bitrate",
        "--start",
        "0",
        "--end",
        "0.2",
    ]));
    assert!(dir.join("adaptive.mp4").exists());
    // a user preset saved into the data directory is found by later runs
    json_out(&cli(&[
        "--data-dir",
        data_s,
        "exec",
        "export.presets.save",
        "name=Tiny WAV",
        "from=Waveform Audio 48 kHz 16-bit",
        "settings={\"audio\":{\"channels\":1}}",
    ]));
    let list = json_out(&cli(&["--data-dir", data_s, "export", "--list-presets", "tiny"]));
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    let wav = dir.join("tiny.wav");
    json_out(&cli(&["--demo", "--data-dir", data_s, "export", wav.to_str().unwrap(), "--preset", "Tiny WAV", "--start", "0", "--end", "0.5"]));
    let b = std::fs::read(&wav).unwrap();
    assert_eq!(u16::from_le_bytes([b[22], b[23]]), 1, "mono from the user preset");
    // through the queue
    let q = dir.join("queued.wav");
    let v = json_out(&cli(&["--demo", "--data-dir", data_s, "export", q.to_str().unwrap(), "--preset", "Tiny WAV", "--queue", "--range", "entire"]));
    assert_eq!(v["items"][0]["status"], "done", "{v}");
    assert!(q.exists());
    // a format that changes the extension: the completion message names the file written (#341)
    let apv = dir.join("apv.mp4");
    let o =
        cli(&["--demo", "--data-dir", data_s, "export", apv.to_str().unwrap(), "--preset", "APV 422-10", "--start", "0", "--end", "0.1", "--scale", "0.125"]);
    let v = json_out(&o);
    let written = v["path"].as_str().unwrap().to_string();
    assert!(written.ends_with("apv.mp4.mov") && std::path::Path::new(&written).exists(), "{v}");
    let msg = String::from_utf8_lossy(&o.stderr);
    assert!(msg.contains(&format!("exported {written} in ")), "{msg}");
    let qa = dir.join("queued-apv.mp4");
    let o = cli(&[
        "--demo",
        "--data-dir",
        data_s,
        "export",
        qa.to_str().unwrap(),
        "--preset",
        "APV 422-10",
        "--queue",
        "--start",
        "0",
        "--end",
        "0.1",
        "--scale",
        "0.125",
    ]);
    let v = json_out(&o);
    let written = v["items"][0]["path"].as_str().unwrap().to_string();
    assert!(written.ends_with("queued-apv.mp4.mov"), "{v}");
    assert!(String::from_utf8_lossy(&o.stderr).contains(&format!("exported {written} in ")), "{}", String::from_utf8_lossy(&o.stderr));
    // unknown preset: exit status 1 and a message naming it
    let bad = cli(&["--demo", "--data-dir", data_s, "export", dir.join("x.mp4").to_str().unwrap(), "--preset", "No Such Preset"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("No Such Preset"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `filmcraft-cli commands | head -c 10` panicked with "failed printing to stdout: Broken pipe
/// (os error 32)" and exit status 101 (#285).
#[test]
fn closed_stdout_ends_quietly() {
    use std::io::Read;
    for args in [&["commands"][..], &["help"], &["--demo", "commands", "--json"]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_filmcraft-cli")).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut first = [0u8; 10];
        child.stdout.as_mut().unwrap().read_exact(&mut first).unwrap();
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && err.is_empty(), "{args:?}: {:?}: {err}", out.status);
    }
}

/// `run -` with stdout already closed: `run` reads its script to the end before it prints, so its
/// first write fails for sure. The rest of the script and the save still happen, and the exit status
/// still reports a failing command.
fn run_with_closed_stdout(args: &[&str], script: &[u8]) -> Output {
    let mut child =
        Command::new(env!("CARGO_BIN_EXE_filmcraft-cli")).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    drop(child.stdout.take());
    child.stdin.take().unwrap().write_all(script).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn closed_stdout_still_runs_the_script_and_saves() {
    let dir = std::env::temp_dir().join(format!("fc-cli-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let proj = dir.join("p.fcproj");
    let p = proj.to_str().unwrap();
    let script = b"{\"id\":\"file.newBin\",\"params\":{\"name\":\"Piped One\"}}\n{\"id\":\"file.newBin\",\"params\":{\"name\":\"Piped Two\"}}\n";
    let out = run_with_closed_stdout(&["--demo", "--save-as", p, "run", "-"], script);
    assert!(out.status.success(), "{:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    let tree = json_out(&cli(&["--project", p, "inspect", "project"])).to_string();
    assert!(tree.contains("Piped One") && tree.contains("Piped Two"), "{tree}");

    // a failing command is still a failure, whether or not anyone reads stdout
    let out = run_with_closed_stdout(&["--demo", "run", "-"], b"{\"id\":\"no.such.command\"}\n");
    assert_eq!(out.status.code(), Some(1));
    let out = run_with_closed_stdout(
        &["--demo", "--keep-going", "run", "-"],
        b"{\"id\":\"no.such.command\"}\n{\"id\":\"file.newBin\",\"params\":{\"name\":\"x\"}}\n",
    );
    assert_eq!(out.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// #1054: exporting or rendering a project whose media is missing fails (exit 1) and names the
/// offline items on stderr and in the result's `offlineMedia`, unless `--allow-offline` accepts the
/// placeholder frames; then it still warns.
#[test]
fn export_with_missing_media_fails_unless_allowed() {
    let dir = std::env::temp_dir().join(format!("fc-cli-1054-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let f = |n: &str| dir.join(n).to_str().unwrap().to_string();
    let (still, proj) = (f("still.png"), f("p.fcproj"));
    let o = cli(&["--demo", "render", "--seconds", "1", "--scale", "0.1", "--out", &still]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    json_out(&cli(&["exec", "file.newSequence", "name=s", "width=64", "height=36", "fps=24", "--save-as", &proj]));
    let paths = format!("paths=[\"{}\"]", still.replace('\\', "\\\\"));
    let item = json_out(&cli(&["--project", &proj, "--save", "exec", "file.import", &paths]))["items"][0].as_u64().unwrap();
    json_out(&cli(&["--project", &proj, "--save", "exec", "timeline.place", &format!("item={item}"), "track=V1", "frame=0", "audio=false"]));
    let export = |out: &str, extra: &[&str]| {
        let mut args = vec!["--project", proj.as_str(), "--compact", "export", out, "--format", "png", "--start", "0", "--end", "0.05"];
        args.extend_from_slice(extra);
        cli(&args)
    };

    let on = export(&f("on.png"), &[]);
    let v = json_out(&on);
    assert_eq!(v["offlineMedia"], serde_json::json!([]), "{v}");
    assert!(!String::from_utf8_lossy(&on.stderr).contains("warning"), "{}", String::from_utf8_lossy(&on.stderr));

    std::fs::remove_file(&still).unwrap();
    let off = export(&f("off.png"), &[]);
    let err = String::from_utf8_lossy(&off.stderr);
    assert_eq!(off.status.code(), Some(1), "{err}");
    assert!(err.contains("still.png") && err.contains("missing") && err.contains("--allow-offline"), "{err}");
    let v: Value = serde_json::from_slice(&off.stdout).expect("the result JSON is still printed");
    assert_eq!(v["offlineMedia"][0]["item"], item, "{v}");
    assert_eq!(v["offlineMedia"][0]["status"], "missing", "{v}");

    let allowed = export(&f("allowed.png"), &["--allow-offline"]);
    let err = String::from_utf8_lossy(&allowed.stderr);
    assert!(allowed.status.success(), "{err}");
    assert!(err.contains("still.png") && err.contains("missing"), "still warns: {err}");
    assert_eq!(json_out(&allowed)["offlineMedia"][0]["status"], "missing");

    let render = cli(&["--project", &proj, "render", "--seconds", "0", "--out", &f("r.png")]);
    let err = String::from_utf8_lossy(&render.stderr);
    assert_eq!(render.status.code(), Some(1), "{err}");
    assert!(err.contains("still.png") && err.contains("--allow-offline"), "{err}");
    let render = cli(&["--project", &proj, "render", "--seconds", "0", "--out", &f("r.png"), "--allow-offline"]);
    assert!(render.status.success(), "{}", String::from_utf8_lossy(&render.stderr));
    assert!(String::from_utf8_lossy(&render.stderr).contains("still.png"));
    let _ = std::fs::remove_dir_all(&dir);
}
