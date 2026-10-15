//! The MCP bearer token (`mcp --http`), which is on by default. It comes from, first match wins:
//! `FILMCRAFT_MCP_TOKEN`, `--token-file`, or the token stored in FilmCraft's data directory
//! (`mcp-token`), which is made on first use from the operating system's random number generator.
//! `--no-token` turns it off, on loopback only.
//!
//! Token files come from many editors and shells. Windows PowerShell 5.1 writes UTF-16 with a
//! byte-order mark (`'tok' > token.txt`) or UTF-8 with one (`-Encoding utf8`), cmd.exe adds a
//! space before the line break (`echo tok > token.txt`), and everything adds a line break. All of
//! those are read as the token they hold; a file that is no text at all gets a message saying how
//! to write one. Nothing here ever shows the token.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use filmcraft_automation::http_transport::TOKEN_ENV;

/// The stored token's file name, in the data directory.
pub const STORED_FILE: &str = "mcp-token";
/// Random bytes in a generated token (64 hex digits).
const GENERATED_BYTES: usize = 32;

/// Largest token file read (a 4 KiB token in UTF-16).
const MAX_FILE: u64 = 8 << 10;

/// How to write a token file that reads back right, for error messages.
const HOW: &str =
    "write the token as plain ASCII text, e.g. `openssl rand -hex 32 > token.txt`, or in PowerShell `Set-Content -Encoding ascii token.txt <token>`";

/// The token in the file at `path` (its first line, trimmed), and a warning if other users can get
/// at the file.
pub fn read_token_file(path: &Path) -> Result<(String, Option<String>), String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let warning = file.metadata().ok().and_then(|m| exposure(&m)).map(|why| format!("{} {why}", path.display()));
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("larger than 8 KiB: not a token file".into());
    }
    Ok((token_from_bytes(&bytes)?, warning))
}

/// The token from `FILMCRAFT_MCP_TOKEN`'s value: trimmed, `None` when empty.
pub fn token_from_env(value: &str) -> Option<String> {
    Some(first_line(value)).filter(|t| !t.is_empty())
}

/// The first line of a token file's text, trimmed: UTF-8 (with or without a byte-order mark) or
/// UTF-16 with a byte-order mark.
pub fn token_from_bytes(bytes: &[u8]) -> Result<String, String> {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        utf8(rest)?
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, u16::from_le_bytes)?
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, u16::from_be_bytes)?
    } else if bytes.contains(&0) {
        return Err(format!("the file holds NUL bytes (UTF-16 without a byte-order mark?): {HOW}"));
    } else {
        utf8(bytes)?
    };
    let token = first_line(&text);
    if token.is_empty() {
        return Err(format!("the first line of the file is empty: {HOW}"));
    }
    Ok(token)
}

/// The first line of `text`, without a byte-order mark or surrounding white space.
fn first_line(text: &str) -> String {
    text.trim_start_matches('\u{FEFF}').lines().next().unwrap_or_default().trim().to_string()
}

fn utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| format!("the file is not UTF-8 or UTF-16 text: {HOW}"))
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, String> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(format!("the file is cut off in the middle of a UTF-16 character: {HOW}"));
    }
    char::decode_utf16(pairs.iter().map(|p| unit(*p))).collect::<Result<String, _>>().map_err(|_| format!("the file is not valid UTF-16 text: {HOW}"))
}

/// Why other users can get at a token file (read it, or put in their own token), if they can:
/// group or other permission bits on a regular file. Only checked on unix; a Windows profile
/// folder is private by default.
#[cfg(unix)]
fn exposure(meta: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt as _;
    // a pipe (`--token-file <(pass show filmcraft)`) or a device is not a file others can open later
    if !meta.is_file() {
        return None;
    }
    let mode = meta.permissions().mode() & 0o777;
    let what = match (mode & 0o044 != 0, mode & 0o022 != 0) {
        (true, true) => "can be read and changed by other users",
        (true, false) => "can be read by other users",
        (false, true) => "can be changed by other users",
        (false, false) if mode & 0o011 != 0 => "is open to other users",
        (false, false) => return None,
    };
    Some(format!("{what} (mode {mode:03o}); make it private with `chmod 600`"))
}

#[cfg(not(unix))]
fn exposure(_meta: &std::fs::Metadata) -> Option<String> {
    None
}

/// Where `mcp --http` got its token, for the banner and the command it prints.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// `FILMCRAFT_MCP_TOKEN`.
    Env,
    /// `--token-file`, made absolute.
    File(PathBuf),
    /// The stored token; `created` when it was made just now.
    Stored { path: PathBuf, created: bool },
    /// `--no-token`.
    Off,
}

/// The token `mcp --http` requires, where it came from, and warnings to show (never the token).
#[derive(Debug)]
pub struct Resolved {
    pub token: Option<String>,
    pub source: Source,
    pub warnings: Vec<String>,
}

/// Why no token could be had: a usage error (exit 2) or a failure (exit 1).
#[derive(Debug, PartialEq)]
pub enum Problem {
    Usage(String),
    Failure(String),
}

/// The token `mcp --http` requires, first match wins: `env` (`FILMCRAFT_MCP_TOKEN`'s value; empty
/// counts as unset), then `token_file`, then the stored token at `stored()` (made when there is
/// none, see [`load_or_create`]). `no_token` turns it off, and contradicts a token given either
/// way.
pub fn resolve(no_token: bool, env: Option<&str>, token_file: Option<&str>, stored: impl FnOnce() -> Result<PathBuf, String>) -> Result<Resolved, Problem> {
    let env = env.and_then(token_from_env);
    if no_token {
        if env.is_some() {
            return Err(Problem::Usage(format!("--no-token contradicts {TOKEN_ENV}, which is set: unset it, or leave --no-token off")));
        }
        if token_file.is_some() {
            return Err(Problem::Usage("--no-token contradicts --token-file: give one or the other".into()));
        }
        return Ok(Resolved { token: None, source: Source::Off, warnings: Vec::new() });
    }
    let mut warnings = Vec::new();
    if let Some(token) = env {
        if let Some(f) = token_file {
            warnings.push(format!("{TOKEN_ENV} is set, so it is the token and --token-file {f} is not read (unset the variable to use the file)"));
        }
        return Ok(Resolved { token: Some(token), source: Source::Env, warnings });
    }
    if let Some(f) = token_file {
        let path = Path::new(f);
        let (token, warning) = read_token_file(path).map_err(|e| Problem::Usage(format!("--token-file {f}: {e}")))?;
        warnings.extend(warning.map(|w| format!("the token file {w}")));
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        return Ok(Resolved { token: Some(token), source: Source::File(path), warnings });
    }
    let instead = format!("pass --token-file FILE or set {TOKEN_ENV} instead");
    let path = stored().map_err(|e| Problem::Failure(format!("cannot locate the stored MCP token: {e}; {instead}")))?;
    let stored = load_or_create(&path)
        .map_err(|e| Problem::Failure(format!("the stored MCP token: {e}; replace it with `filmcraft-cli mcp-token --new`, or {instead}")))?;
    warnings.extend(stored.warning.map(|w| format!("the stored token file {w}")));
    Ok(Resolved { token: Some(stored.token), source: Source::Stored { path, created: stored.created }, warnings })
}

/// `--no-token` is for loopback only: not with an address the network reaches, nor with an
/// `--allow-host` (the names other machines, containers, tunnels and proxies reach the server
/// by), nor with `--allow-origin null` (any web page).
pub fn check_no_token(addr: SocketAddr, allowed_hosts: &[String], allowed_origins: &[String]) -> Result<(), String> {
    if !addr.ip().is_loopback() {
        return Err(format!(
            "--no-token is only for loopback addresses (127.0.0.1, [::1], localhost): {addr} can be reached from the network, so it needs the token"
        ));
    }
    if let Some(h) = allowed_hosts.first() {
        return Err(format!(
            "--no-token can't be used with --allow-host {h}: the names it adds are how containers, tunnels, proxies or other machines reach the server, so it needs the token"
        ));
    }
    if allowed_origins.iter().any(|o| o.eq_ignore_ascii_case("null")) {
        return Err("--no-token can't be used with --allow-origin null: every web page can send that origin, so it needs the token".into());
    }
    Ok(())
}

/// Where the stored token lives: `mcp-token` in `data_dir` (`--data-dir`), else in FilmCraft's
/// data directory (`FILMCRAFT_DATA_DIR`, a portable install's `data` folder, or the per-user one:
/// `%APPDATA%\FilmCraft`, `~/Library/Application Support/FilmCraft`,
/// `$XDG_DATA_HOME/filmcraft` or `~/.local/share/filmcraft`). Absolute, so the command the server
/// prints to read it works from any folder.
pub fn stored_token_path(data_dir: Option<&str>) -> Result<PathBuf, String> {
    let dir = match data_dir {
        Some(d) => PathBuf::from(d),
        None => filmcraft_engine::autosave::default_data_dir().ok_or("FilmCraft found no per-user data directory (HOME, or APPDATA on Windows, is not set)")?,
    };
    let dir = std::path::absolute(&dir).map_err(|e| format!("data directory `{}`: {e}", dir.display()))?;
    Ok(dir.join(STORED_FILE))
}

/// The stored token at a path.
#[derive(Debug)]
pub struct Stored {
    pub token: String,
    /// Made just now: there was none.
    pub created: bool,
    /// Other users can get at the file (unix).
    pub warning: Option<String>,
}

/// The token stored at `path`, read as a `--token-file` is. When nothing is there, a new random
/// one ([`generate_token`]) is stored first, without ever replacing or following what is at
/// `path`: a file another process (another server, `mcp-token`) stored meanwhile is read instead,
/// and a dangling link is an error. On unix the file is private (0600), and so is a folder made
/// for it (0700).
pub fn load_or_create(path: &Path) -> Result<Stored, String> {
    let read = || read_token_file(path).map(|(token, warning)| Stored { token, created: false, warning }).map_err(|e| format!("{}: {e}", path.display()));
    match std::fs::symlink_metadata(path) {
        Ok(_) => return read(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let token = generate_token()?;
    match create_token_file(path, &token) {
        Ok(()) => Ok(Stored { token, created: true, warning: None }),
        // another process stored one first: that is the token
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(),
        Err(e) => Err(format!("cannot create {}: {e}", path.display())),
    }
}

/// Replace the token stored at `path` with a new random one, in one step: a reader sees the old
/// file or the new one, never part of one. The new file is private (0600 on unix) whatever the old
/// one was, and a link at `path` is replaced, not followed.
pub fn replace_stored(path: &Path) -> Result<String, String> {
    let token = generate_token()?;
    let replaced = (|| {
        let dir = folder(path)?;
        create_private_dir(dir)?;
        let tmp = write_temp(dir, &token)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        sync_dir(dir);
        Ok::<_, std::io::Error>(())
    })();
    replaced.map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(token)
}

/// A new random token: 32 bytes from the operating system's random number generator, as 64
/// lowercase hex digits.
pub fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; GENERATED_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| format!("the system's random number generator failed: {e}"))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len().saturating_mul(2)), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Store `token` at `path` unless something is there (`AlreadyExists`). It is written to a
/// private temporary file first and hard-linked into place, so that a reader never finds the file
/// empty or half-written; where the file system has no hard links (FAT, some network shares), it
/// is written in place, still never over another file.
fn create_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    let dir = folder(path)?;
    create_private_dir(dir)?;
    let tmp = write_temp(dir, token)?;
    let linked = match std::fs::hard_link(&tmp, path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => write_private(path, token),
        r => r,
    };
    let _ = std::fs::remove_file(&tmp);
    if linked.is_ok() {
        sync_dir(dir);
    }
    linked
}

/// The folder a token file goes in.
fn folder(path: &Path) -> std::io::Result<&Path> {
    path.parent().filter(|d| !d.as_os_str().is_empty()).ok_or_else(|| std::io::Error::other("the path names no folder"))
}

/// Make `dir` (and its missing parents) if it is not there, private to this user on unix (0700).
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    // whatever the umask (one that takes the owner's bits away would leave it unusable)
    #[cfg(unix)]
    let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    Ok(())
}

/// `token` in a new private file in `dir`, under a random name no other process uses.
fn write_temp(dir: &Path, token: &str) -> std::io::Result<PathBuf> {
    let mut taken = None;
    for _ in 0..8 {
        let mut salt = [0u8; 8];
        getrandom::fill(&mut salt).map_err(|e| std::io::Error::other(format!("the system's random number generator failed: {e}")))?;
        let tmp = dir.join(format!(".{STORED_FILE}.{}.tmp", hex(&salt)));
        match write_private(&tmp, token) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => taken = Some(e),
            r => return r.map(|()| tmp),
        }
    }
    Err(taken.unwrap_or_else(|| std::io::ErrorKind::AlreadyExists.into()))
}

/// A new file at `path` holding `token` without a line break, private on unix (0600), on disk
/// when this returns. Never opens an existing file; removes its own on failure.
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(path)?;
    let written = (|| {
        // whatever the umask
        #[cfg(unix)]
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        file.write_all(token.as_bytes())?;
        file.sync_all()
    })();
    if written.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    written
}

/// Best effort: make a new name in `dir` durable (POSIX; Windows cannot open a folder this way).
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// The `--header` argument of the `claude mcp add` command the server prints: it reads the token
/// where the server got it, so the command never shows it. PowerShell syntax with `powershell`
/// (the server runs on Windows), else POSIX shell syntax (bash, zsh, fish 3.4+). `None` without a
/// token, or when the path is no text a shell can carry.
pub fn header_arg(source: &Source, powershell: bool) -> Option<String> {
    let read = match (source, powershell) {
        (Source::Off, _) => return None,
        (Source::Env, true) => format!("$env:{TOKEN_ENV}"),
        (Source::Env, false) => format!("${TOKEN_ENV}"),
        // the stored file has no line break; Get-Content -Raw takes the file as it is
        (Source::Stored { path, .. }, true) => format!("$(Get-Content -Raw {})", powershell_quote(path.to_str()?)),
        (Source::Stored { path, .. }, false) => format!("$(cat {})", sh_quote(path.to_str()?)),
        // a token file may hold more than its first line, and a line break: read it as the server does
        (Source::File(path), true) => format!("$((Get-Content -TotalCount 1 {}).Trim())", powershell_quote(path.to_str()?)),
        (Source::File(path), false) => format!("$(head -n 1 {})", sh_quote(path.to_str()?)),
    };
    Some(format!("--header \"Authorization: Bearer {read}\""))
}

/// `s` as a POSIX shell single-quoted word: `'` becomes `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as a PowerShell single-quoted string. PowerShell takes the typographic single quotes
/// (‘ ’ ‚ ‛) for `'` too, and each is escaped by doubling it.
pub fn powershell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len().saturating_add(2));
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut b = if bom { vec![0xFF, 0xFE] } else { Vec::new() };
        b.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        b
    }

    fn utf16be(s: &str) -> Vec<u8> {
        let mut b = vec![0xFE, 0xFF];
        b.extend(s.encode_utf16().flat_map(u16::to_be_bytes));
        b
    }

    /// What PowerShell 5.1, PowerShell 7, cmd.exe, Notepad and unix tools write all read back as
    /// the token.
    #[test]
    fn token_files_in_every_common_encoding() {
        let line = format!("{TOKEN}\r\n");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("ASCII, unix line break", format!("{TOKEN}\n").into_bytes()),
            ("no line break", TOKEN.as_bytes().to_vec()),
            ("CRLF (Set-Content -Encoding ascii)", line.clone().into_bytes()),
            ("cmd.exe `echo tok > file`", format!("{TOKEN} \r\n").into_bytes()),
            ("UTF-8 BOM (PowerShell 5.1 -Encoding utf8, Notepad)", [&[0xEF, 0xBB, 0xBF][..], line.as_bytes()].concat()),
            ("UTF-16 LE BOM (PowerShell 5.1 `>`, Out-File)", utf16le(&line, true)),
            ("UTF-16 BE BOM (-Encoding BigEndianUnicode)", utf16be(&line)),
            ("second line ignored", format!("{TOKEN}\nnot this\n").into_bytes()),
            ("surrounding spaces", format!("  {TOKEN}\t\n").into_bytes()),
        ];
        for (what, bytes) in cases {
            assert_eq!(token_from_bytes(&bytes).as_deref(), Ok(TOKEN), "{what}");
        }
    }

    /// Files that hold no usable text are refused with a message saying how to write one, never
    /// with the file's content.
    #[test]
    fn unusable_token_files_say_how_to_write_one() {
        let secret = "s3cret-s3cret-s3cret-s3cret-s3cret";
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("UTF-16 without a BOM", utf16le(secret, false)),
            ("Latin-1", [secret.as_bytes(), &[0xE9]].concat()),
            ("UTF-16 cut in half", [utf16le(secret, true), vec![0x41]].concat()),
            ("lone surrogate", [vec![0xFF, 0xFE], 0xD800u16.to_le_bytes().to_vec()].concat()),
            ("empty", Vec::new()),
            ("only a BOM", vec![0xEF, 0xBB, 0xBF]),
            ("empty first line", format!("\n{secret}\n").into_bytes()),
            ("blank", b"   \r\n".to_vec()),
        ];
        for (what, bytes) in cases {
            let e = token_from_bytes(&bytes).expect_err(what);
            assert!(e.contains("Set-Content -Encoding ascii") && !e.contains(secret), "{what}: {e}");
        }
    }

    #[test]
    fn token_from_the_environment() {
        assert_eq!(token_from_env(&format!(" {TOKEN}\r\n")).as_deref(), Some(TOKEN));
        assert_eq!(token_from_env(&format!("\u{FEFF}{TOKEN}")).as_deref(), Some(TOKEN));
        assert_eq!(token_from_env("  "), None);
        assert_eq!(token_from_env(""), None);
    }

    #[test]
    fn token_file_reading() {
        let dir = std::env::temp_dir().join(format!("filmcraft-cli-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.txt");
        let p = path.as_path();
        std::fs::write(&path, utf16le(&format!("{TOKEN}\r\n"), true)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |m: u32| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).unwrap();
            mode(0o600);
            assert_eq!(read_token_file(p), Ok((TOKEN.to_string(), None)));
            mode(0o400);
            assert_eq!(read_token_file(p), Ok((TOKEN.to_string(), None)));
            for (m, says) in
                [(0o644, "can be read by other users"), (0o640, "can be read by"), (0o666, "read and changed"), (0o620, "can be changed"), (0o601, "open to")]
            {
                mode(m);
                let (token, warning) = read_token_file(p).unwrap();
                assert_eq!(token, TOKEN);
                let w = warning.unwrap_or_else(|| panic!("no warning for mode {m:o}"));
                assert!(w.contains(says) && w.contains("chmod 600") && w.contains(&format!("{m:03o}")) && !w.contains(TOKEN), "{w}");
            }
            mode(0o600);
        }
        #[cfg(not(unix))]
        assert_eq!(read_token_file(p).unwrap().0, TOKEN);
        // a file far larger than any token is refused without reading it all
        std::fs::write(&path, vec![b'a'; 64 << 10]).unwrap();
        assert!(read_token_file(p).unwrap_err().contains("larger than 8 KiB"));
        assert!(read_token_file(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A test folder under the system's temporary folder, removed when the test ends (also when
    /// it fails).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d = std::env::temp_dir().join(format!("filmcraft-cli-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        /// The names in `dir` (to see that no temporary file is left behind).
        fn names(dir: &Path) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn is_token(t: &str) -> bool {
        t.len() == 64 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn generated_tokens_are_64_random_hex_digits() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert!(is_token(&a) && is_token(&b), "{a} {b}");
        assert_ne!(a, b);
        assert!(a.len() >= filmcraft_automation::http_transport::MIN_REMOTE_TOKEN_LEN);
        assert!(filmcraft_automation::http_transport::check_token(&a).is_ok());
        assert_eq!(hex(&[0, 1, 0xab, 0xff]), "0001abff");
    }

    /// The first use stores a new token in a private file (and a private folder made for it),
    /// with no line break and no temporary file left; later ones read the same token back.
    #[test]
    fn the_stored_token_is_made_once_privately_and_reused() {
        let tmp = TempDir::new("stored");
        let dir = tmp.0.join("data").join("filmcraft");
        let path = dir.join(STORED_FILE);
        let first = load_or_create(&path).unwrap();
        assert!(first.created && first.warning.is_none() && is_token(&first.token), "{first:?}");
        assert_eq!(std::fs::read(&path).unwrap(), first.token.as_bytes(), "the file holds the token and no line break");
        assert_eq!(TempDir::names(&dir), [STORED_FILE]);
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&tmp.0.join("data")), 0o700);
        }
        for _ in 0..3 {
            let again = load_or_create(&path).unwrap();
            assert!(!again.created);
            assert_eq!(again.token, first.token);
        }
        // a folder that is already there keeps its permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let shared = tmp.0.join("shared");
            std::fs::create_dir(&shared).unwrap();
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
            let p = shared.join(STORED_FILE);
            assert!(load_or_create(&p).unwrap().created);
            assert_eq!((mode(&shared), mode(&p)), (0o755, 0o600));
        }
    }

    /// Whatever is at the path already is never replaced: a token file is read as it is (with a
    /// warning when others can read it), and a dangling link, a folder or an unusable file is an
    /// error, with nothing written through or over it.
    #[test]
    fn an_existing_path_is_never_replaced_or_followed() {
        let tmp = TempDir::new("existing");
        let path = tmp.0.join(STORED_FILE);
        std::fs::write(&path, format!("{TOKEN}\r\n")).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let s = load_or_create(&path).unwrap();
        assert_eq!((s.token.as_str(), s.created), (TOKEN, false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{TOKEN}\r\n"));
        #[cfg(unix)]
        assert!(s.warning.is_some_and(|w| w.contains("can be read by other users") && !w.contains(TOKEN)));
        std::fs::write(&path, "\n").unwrap();
        assert!(load_or_create(&path).unwrap_err().contains("first line of the file is empty"));
        assert_eq!(std::fs::read(&path).unwrap(), b"\n");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(load_or_create(&path).is_err());
        std::fs::remove_dir(&path).unwrap();
        #[cfg(unix)]
        {
            let target = tmp.0.join("elsewhere");
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(load_or_create(&path).is_err());
            assert!(!target.exists(), "nothing written through the link");
            assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
            // a link to a token file is read
            std::fs::write(&target, TOKEN).unwrap();
            assert_eq!(load_or_create(&path).unwrap().token, TOKEN);
        }
        assert!(TempDir::names(&tmp.0).iter().all(|n| !n.ends_with(".tmp")));
    }

    /// Servers and `mcp-token` starting at once on a fresh data directory all end up with the
    /// one token one of them stored, never an empty or half-written file.
    #[test]
    fn racing_first_uses_agree_on_one_token() {
        for _ in 0..20 {
            let tmp = TempDir::new("race");
            let path = tmp.0.join("d").join(STORED_FILE);
            let results: Vec<Stored> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..8).map(|_| s.spawn(|| load_or_create(&path).unwrap())).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert_eq!(results.iter().filter(|r| r.created).count(), 1);
            assert!(results.iter().all(|r| r.token == results[0].token && is_token(&r.token)));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), results[0].token);
            assert_eq!(TempDir::names(&tmp.0.join("d")), [STORED_FILE]);
        }
    }

    /// `mcp-token --new` puts a new private file in place in one step, also over a file others
    /// could read, and replaces a link instead of writing through it.
    #[test]
    fn replacing_the_stored_token() {
        let tmp = TempDir::new("replace");
        let path = tmp.0.join("new").join(STORED_FILE);
        let a = replace_stored(&path).unwrap();
        assert!(is_token(&a));
        assert_eq!(load_or_create(&path).unwrap().token, a);
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let b = replace_stored(&path).unwrap();
        assert!(is_token(&b) && a != b);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), b);
        assert_eq!(TempDir::names(&tmp.0.join("new")), [STORED_FILE]);
        #[cfg(unix)]
        {
            assert_eq!(mode(&path), 0o600);
            let target = tmp.0.join("target");
            std::fs::write(&target, "keep").unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            let c = replace_stored(&path).unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
            assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), c);
        }
        // a folder in the way is an error, and nothing is left behind
        let blocked = tmp.0.join("blocked");
        std::fs::create_dir_all(blocked.join(STORED_FILE).join("x")).unwrap();
        assert!(replace_stored(&blocked.join(STORED_FILE)).is_err());
        assert_eq!(TempDir::names(&blocked), [STORED_FILE]);
    }

    /// `FILMCRAFT_MCP_TOKEN` comes first, then `--token-file`, then the stored token, which is only
    /// made when it is the one used; `--no-token` contradicts the first two.
    #[test]
    fn token_precedence() {
        let tmp = TempDir::new("precedence");
        let stored = tmp.0.join("data").join(STORED_FILE);
        let file = tmp.0.join("token.txt");
        let file_token = "file-token-0123456789-0123456789-abc";
        std::fs::write(&file, format!("{file_token}\n")).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let f = file.to_str().unwrap();
        let at = || Ok(stored.clone());

        let r = resolve(false, Some(&format!("{TOKEN}\n")), Some(f), at).unwrap();
        assert_eq!((r.token.as_deref(), &r.source), (Some(TOKEN), &Source::Env));
        assert!(r.warnings.len() == 1 && r.warnings[0].contains("--token-file") && !r.warnings[0].contains(TOKEN), "{:?}", r.warnings);
        let r = resolve(false, Some(TOKEN), None, at).unwrap();
        assert!(r.warnings.is_empty() && r.source == Source::Env);

        let r = resolve(false, None, Some(f), at).unwrap();
        assert_eq!((r.token.as_deref(), &r.source), (Some(file_token), &Source::File(file.clone())));
        assert!(r.warnings.is_empty());
        // an empty variable counts as unset
        let r = resolve(false, Some(" \r\n"), Some(f), at).unwrap();
        assert_eq!(r.token.as_deref(), Some(file_token));
        assert!(!stored.exists(), "the stored token is only made when it is used");
        assert!(matches!(resolve(false, None, Some(tmp.0.join("missing").to_str().unwrap()), at), Err(Problem::Usage(_))));

        let r = resolve(false, Some(""), None, at).unwrap();
        assert_eq!(r.source, Source::Stored { path: stored.clone(), created: true });
        let made = r.token.unwrap();
        assert!(is_token(&made));
        let r = resolve(false, None, None, at).unwrap();
        assert_eq!((r.token.as_deref(), &r.source), (Some(made.as_str()), &Source::Stored { path: stored.clone(), created: false }));

        // --no-token
        assert!(matches!(resolve(true, Some(TOKEN), None, at), Err(Problem::Usage(m)) if m.contains("FILMCRAFT_MCP_TOKEN") && !m.contains(TOKEN)));
        assert!(matches!(resolve(true, None, Some(f), at), Err(Problem::Usage(m)) if m.contains("--token-file")));
        let off = resolve(true, Some("  "), None, || panic!("not looked up")).unwrap();
        assert_eq!((off.token, off.source), (None, Source::Off));

        // no data directory, or an unusable stored file: a failure (exit 1) that says what to do
        let none = resolve(false, None, None, || Err("no HOME".into())).unwrap_err();
        assert!(matches!(&none, Problem::Failure(m) if m.contains("no HOME") && m.contains("--token-file") && m.contains("FILMCRAFT_MCP_TOKEN")), "{none:?}");
        std::fs::write(&stored, [0xFF, 0xFE, 0x41]).unwrap();
        let bad = resolve(false, None, None, at).unwrap_err();
        assert!(matches!(&bad, Problem::Failure(m) if m.contains("mcp-token --new") && m.contains("--token-file")), "{bad:?}");
    }

    #[test]
    fn no_token_is_for_loopback_only() {
        let lo: SocketAddr = "127.0.0.1:9877".parse().unwrap();
        let v6: SocketAddr = "[::1]:9877".parse().unwrap();
        assert!(check_no_token(lo, &[], &[]).is_ok());
        assert!(check_no_token(v6, &[], &["http://localhost:6274".into()]).is_ok());
        for addr in ["0.0.0.0:9877", "[::]:9877", "192.168.1.5:9877", "[::ffff:127.0.0.1]:9877"] {
            let e = check_no_token(addr.parse().unwrap(), &[], &[]).unwrap_err();
            assert!(e.contains("loopback"), "{addr}: {e}");
        }
        for host in ["host.docker.internal:9877", "my-tunnel.example", "localhost"] {
            assert!(check_no_token(lo, &[host.to_string()], &[]).unwrap_err().contains("--allow-host"), "{host}");
        }
        assert!(check_no_token(lo, &[], &["null".into()]).unwrap_err().contains("--allow-origin null"));
    }

    /// The printed `claude mcp add` header reads the token where the server got it, quoted for
    /// the shell it is printed for, and never holds the token itself.
    #[test]
    fn the_header_to_paste_reads_the_token_without_showing_it() {
        let stored = |p: &str| Source::Stored { path: PathBuf::from(p), created: false };
        assert_eq!(header_arg(&Source::Off, false), None);
        assert_eq!(header_arg(&Source::Off, true), None);
        assert_eq!(header_arg(&Source::Env, false).unwrap(), r#"--header "Authorization: Bearer $FILMCRAFT_MCP_TOKEN""#);
        assert_eq!(header_arg(&Source::Env, true).unwrap(), r#"--header "Authorization: Bearer $env:FILMCRAFT_MCP_TOKEN""#);
        assert_eq!(
            header_arg(&stored("/home/me/.local/share/filmcraft/mcp-token"), false).unwrap(),
            r#"--header "Authorization: Bearer $(cat '/home/me/.local/share/filmcraft/mcp-token')""#
        );
        assert_eq!(
            header_arg(&stored(r"C:\Users\Ann Lee\AppData\Roaming\FilmCraft\mcp-token"), true).unwrap(),
            r#"--header "Authorization: Bearer $(Get-Content -Raw 'C:\Users\Ann Lee\AppData\Roaming\FilmCraft\mcp-token')""#
        );
        assert_eq!(
            header_arg(&Source::File(PathBuf::from(r"C:\tok\t.txt")), true).unwrap(),
            r#"--header "Authorization: Bearer $((Get-Content -TotalCount 1 'C:\tok\t.txt').Trim())""#
        );
        assert_eq!(header_arg(&Source::File(PathBuf::from("/a b/t.txt")), false).unwrap(), r#"--header "Authorization: Bearer $(head -n 1 '/a b/t.txt')""#);
        // quotes in paths
        assert_eq!(sh_quote("/home/o'brien/x"), r"'/home/o'\''brien/x'");
        assert_eq!(powershell_quote(r"C:\Users\O'Brien"), r"'C:\Users\O''Brien'");
        assert_eq!(powershell_quote("C:\\Users\\O\u{2019}Brien"), "'C:\\Users\\O\u{2019}\u{2019}Brien'");
        assert_eq!(powershell_quote("$env:x `n"), "'$env:x `n'");
    }

    /// The header the server prints, run by a real POSIX shell, is `Authorization: Bearer
    /// <token>`, for a path with spaces, quotes and `$` in it.
    #[cfg(unix)]
    #[test]
    fn the_header_to_paste_works_in_a_posix_shell() {
        let tmp = TempDir::new("shell");
        let dir = tmp.0.join("it's a $HOME `dir`");
        let path = dir.join(STORED_FILE);
        let token = load_or_create(&path).unwrap().token;
        let file = dir.join("token.txt");
        std::fs::write(&file, format!("{TOKEN}\nsecond line\n")).unwrap();
        for (source, want) in [(Source::Stored { path, created: false }, token.as_str()), (Source::File(file), TOKEN), (Source::Env, "from-env")] {
            let arg = header_arg(&source, false).unwrap();
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("set -- {arg}; printf '%s|%s' \"$1\" \"$2\""))
                .env(TOKEN_ENV, "from-env")
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), format!("--header|Authorization: Bearer {want}"), "{arg}");
        }
    }
}
