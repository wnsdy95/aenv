//! Shell init script generation. Each `script()` returns a string the user
//! should `eval` in their rc file. The job of these scripts is minimal:
//!   1. Prepend the aenv shims dir to PATH so `claude` resolves to our shim.
//!   2. (Optional) chpwd / PROMPT_COMMAND hook that calls `aenv current` to
//!      surface the current env in the prompt.
//!
//! All real work — env resolution, dispatch, supervisor — lives in the binary.

pub mod bash;
pub mod fish;
pub mod zsh;

/// Resolve the shims directory and validate that it's safe to embed in a
/// shell init script. We refuse to emit init code for paths containing shell
/// metacharacters that would need quoting we can't reliably do across shells.
/// The aenv default (`~/.aenv/shims`) and any sane override are safe.
///
/// Returns `Err(message)` if the path is unsafe — caller should print as a
/// shell echo so `eval` surfaces the error rather than silently breaking PATH.
pub fn shims_path_str() -> std::result::Result<String, String> {
    let raw = match crate::paths::shims_dir() {
        Ok(p) => p.display().to_string(),
        Err(_) => return Ok("$HOME/.aenv/shims".to_string()),
    };
    // On Windows, `aenv shell-init` is for Git Bash / MSYS / WSL —
    // none of which want native `C:\...` paths. Convert to the MSYS
    // POSIX form (`/c/...`) before validation; the safety guard then
    // sees no backslashes and the eval'd PATH= line works in those
    // shells. cmd.exe / PowerShell aren't supported targets — they
    // would need a separate generator anyway.
    let p = to_posix_path(&raw);
    // Disallow chars that have shell special meaning even inside double quotes:
    // " ` $ \ — and newlines, which break line-based init.
    // Single-quote is allowed *outside* quoted regions but our generators use
    // double-quoted PATH= so disallow it too. Spaces are allowed (paths often
    // have them on macOS) since we always wrap in double quotes.
    const FORBIDDEN: &[char] = &['"', '\'', '`', '$', '\\', '\n', '\r'];
    if let Some(c) = p.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(format!(
            "aenv: refusing to emit shell init: shims path contains '{c}' which \
             requires quoting we can't safely produce. Move ~/.aenv to a safe \
             location or set AENV_HOME=<safe-path>."
        ));
    }
    Ok(p)
}

/// Convert a Windows path to MSYS / Git-Bash POSIX form. No-op on
/// non-Windows builds.
///
/// `C:\Users\foo\bar` → `/c/Users/foo/bar`
/// `D:\develop\aenv`  → `/d/develop/aenv`
/// `relative\path`    → `relative/path`
///
/// Kept compiled (not `cfg(windows)`) so unit tests run on every host.
fn to_posix_path(p: &str) -> String {
    if !cfg!(windows) {
        return p.to_string();
    }
    let bytes = p.as_bytes();
    // Drive-letter prefix: `X:\...` or `X:/...` → `/x/...`.
    let mut out = if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let mut s = String::with_capacity(p.len() + 1);
        s.push('/');
        s.push((bytes[0] as char).to_ascii_lowercase());
        s.push_str(&p[2..]);
        s
    } else {
        p.to_string()
    };
    // Backslash → forward slash for the rest of the path. Safe in
    // POSIX shells; what Git Bash itself emits when you `cd C:\foo`.
    if out.contains('\\') {
        out = out.replace('\\', "/");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::to_posix_path;

    #[cfg(windows)]
    #[test]
    fn windows_drive_letter_becomes_msys_form() {
        assert_eq!(to_posix_path(r"C:\Users\foo"), "/c/Users/foo");
        assert_eq!(
            to_posix_path(r"D:\develop\aenv\shims"),
            "/d/develop/aenv/shims"
        );
        // already-forward-slash drive-prefixed path
        assert_eq!(to_posix_path("E:/x/y"), "/e/x/y");
    }

    #[cfg(windows)]
    #[test]
    fn windows_relative_path_just_swaps_slashes() {
        assert_eq!(to_posix_path(r"relative\path\here"), "relative/path/here");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_path_unchanged() {
        assert_eq!(
            to_posix_path("/Users/me/.aenv/shims"),
            "/Users/me/.aenv/shims"
        );
        // macOS allows backslashes in filenames; we leave them alone
        // so the safety check still fires (a real footgun for shells).
        assert_eq!(to_posix_path("/odd\\name"), "/odd\\name");
    }
}
