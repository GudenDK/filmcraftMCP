//! The MCP bearer token (`mcp --http`), from `--token-file` or `FILMCRAFT_MCP_TOKEN`.
//!
//! Token files come from many editors and shells. Windows PowerShell 5.1 writes UTF-16 with a
//! byte-order mark (`'tok' > token.txt`) or UTF-8 with one (`-Encoding utf8`), cmd.exe adds a
//! space before the line break (`echo tok > token.txt`), and everything adds a line break. All of
//! those are read as the token they hold; a file that is no text at all gets a message saying how
//! to write one. Nothing here ever shows the token.

/// Largest token file read (a 4 KiB token in UTF-16).
const MAX_FILE: u64 = 8 << 10;

/// How to write a token file that reads back right, for error messages.
const HOW: &str =
    "write the token as plain ASCII text, e.g. `openssl rand -hex 32 > token.txt`, or in PowerShell `Set-Content -Encoding ascii token.txt <token>`";

/// The token in the file at `path` (its first line, trimmed), and a warning if other users can get
/// at the file.
pub fn read_token_file(path: &str) -> Result<(String, Option<String>), String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let warning = file.metadata().ok().and_then(|m| exposure(&m)).map(|why| format!("{path} {why}"));
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
        let p = path.to_str().unwrap();
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
        assert!(read_token_file(dir.join("missing").to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
