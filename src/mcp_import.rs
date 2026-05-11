//! Bulk import MCP server configs from existing tools' configs.
//!
//! `aenv add mcp --from <source>` reads a config file or deeplink that
//! another tool (Claude Desktop, Claude Code, Cursor, VS Code) wrote and
//! returns a list of `(name, McpSpec)` entries the caller writes into
//! the aenv manifest. Saves users from typing `aenv add mcp ...` 5×
//! when their existing tool already has the right config.
//!
//! Supported sources (cited per agent's verified research):
//!   * `claude-desktop`     — `claude_desktop_config.json` shape, OS-specific path
//!   * `claude-code`        — `~/.claude.json` (mcpServers field)
//!   * `cursor`             — `~/.cursor/mcp.json` and `<cwd>/.cursor/mcp.json`
//!   * `cursor-deeplink:<url>` — `cursor://anysphere.cursor-deeplink/mcp/install?name=...&config=<base64>`
//!   * `vscode-deeplink:<url>` — `vscode:mcp/install?name=...&config=<urlencoded-json>`
//!   * `<path>`             — any file containing a `mcpServers`-shaped object
//!
//! Schema reference (verbatim quote from
//! <https://modelcontextprotocol.io/quickstart/user>):
//!   "{\n  \"mcpServers\": {\n    \"filesystem\": {\n      \"command\": \"npx\", ... } } }"

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::env::manifest::McpSpec;

pub fn import_from(source: &str) -> Result<Vec<(String, McpSpec)>> {
    if let Some(deeplink) = source.strip_prefix("cursor-deeplink:") {
        return import_cursor_deeplink(deeplink);
    }
    if let Some(deeplink) = source.strip_prefix("vscode-deeplink:") {
        return import_vscode_deeplink(deeplink);
    }
    match source {
        "claude-desktop" => import_path(&claude_desktop_config_path()?),
        "claude-code" => import_path(&claude_code_config_path()?),
        "cursor" => import_cursor_combined(),
        path => import_path(Path::new(path)),
    }
}

/// `~/Library/Application Support/Claude/claude_desktop_config.json`
/// on macOS; `%APPDATA%\Claude\claude_desktop_config.json` on Windows.
/// Linux path is best-effort (`$XDG_CONFIG_HOME/Claude/...`).
fn claude_desktop_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no $HOME"))?;
    #[cfg(target_os = "macos")]
    {
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("Claude")
            .join("claude_desktop_config.json"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| {
            home.join("AppData")
                .join("Roaming")
                .to_string_lossy()
                .into_owned()
        });
        Ok(PathBuf::from(appdata)
            .join("Claude")
            .join("claude_desktop_config.json"))
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"));
        Ok(xdg.join("Claude").join("claude_desktop_config.json"))
    }
}

fn claude_code_config_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("no $HOME"))?
        .join(".claude.json"))
}

/// Cursor reads `<project>/.cursor/mcp.json` first, then `~/.cursor/mcp.json`
/// — we merge both, project entries winning on collision.
fn import_cursor_combined() -> Result<Vec<(String, McpSpec)>> {
    let mut out: BTreeMap<String, McpSpec> = BTreeMap::new();
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no $HOME"))?;
    let global = home.join(".cursor").join("mcp.json");
    if global.is_file() {
        for (name, spec) in import_path(&global)? {
            out.insert(name, spec);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let project = cwd.join(".cursor").join("mcp.json");
        if project.is_file() {
            for (name, spec) in import_path(&project)? {
                out.insert(name, spec); // project wins
            }
        }
    }
    if out.is_empty() {
        bail!(
            "no cursor mcp config found at {} or <cwd>/.cursor/mcp.json",
            global.display()
        );
    }
    Ok(out.into_iter().collect())
}

/// Read any JSON file containing either a top-level `{ "mcpServers": { ... } }`
/// (Claude Desktop / Claude Code shape) or a bare `{ ... }` map
/// (Cursor / Cline shape).
fn import_path(path: &Path) -> Result<Vec<(String, McpSpec)>> {
    if !path.is_file() {
        bail!("config file not found: {}", path.display());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;

    // Detect shape by peeking at top-level keys. The wrapped form
    // (Claude Desktop / Claude Code) has a top-level `mcpServers`
    // object; the bare form (Cursor / Cline) is a map of name → spec
    // directly. A loose `from_str::<Wrapped>` would also succeed on a
    // bare-map JSON because every wrapped field is `default`-able,
    // returning empty entries — so explicit detection is required.
    let v: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("parse {} as JSON", path.display()))?;
    let map = v
        .get("mcpServers")
        .and_then(|m| m.as_object())
        .cloned()
        .or_else(|| v.as_object().cloned())
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let mut out: Vec<(String, McpSpec)> = Vec::new();
    for (k, raw_value) in map {
        let raw: RawSpec = serde_json::from_value(raw_value)
            .with_context(|| format!("parse server '{k}' in {} as MCP spec", path.display()))?;
        out.push((k, raw.into_spec()));
    }
    Ok(out)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawSpec {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
}

impl RawSpec {
    fn into_spec(self) -> McpSpec {
        McpSpec {
            transport: self.kind,
            command: self.command,
            args: self.args,
            env: self.env,
            url: self.url,
            headers: self.headers,
            version: None,
        }
    }
}

/// Cursor install link, verbatim from
/// <https://cursor.com/docs/context/mcp/install-links>:
/// `cursor://anysphere.cursor-deeplink/mcp/install?name=$NAME&config=$BASE64_ENCODED_CONFIG`
/// where `config` is base64(JSON.stringify(spec)).
fn import_cursor_deeplink(url: &str) -> Result<Vec<(String, McpSpec)>> {
    let (name, config_raw) = parse_deeplink_query(url)?;
    let json = decode_base64_url(&config_raw)
        .with_context(|| "decode cursor deeplink `config` (expected base64)")?;
    let spec: RawSpec =
        serde_json::from_slice(&json).context("parse cursor deeplink config JSON")?;
    Ok(vec![(name, spec.into_spec())])
}

/// VS Code install URL, e.g.
/// `vscode:mcp/install?name=...&config=<urlencoded-or-base64-json>`.
/// We accept both URL-encoded JSON (the spec since 2024-10) and base64
/// (matching Cursor) — try base64 first, fall back to plain URL-decoded
/// JSON.
fn import_vscode_deeplink(url: &str) -> Result<Vec<(String, McpSpec)>> {
    let (name, config_raw) = parse_deeplink_query(url)?;
    let bytes = decode_base64_url(&config_raw)
        .or_else(|_| Ok::<_, anyhow::Error>(url_decode(&config_raw).into_bytes()))?;
    let spec: RawSpec =
        serde_json::from_slice(&bytes).context("parse vscode deeplink config JSON")?;
    Ok(vec![(name, spec.into_spec())])
}

/// Pull `name=<NAME>` and `config=<...>` out of a deeplink query string.
fn parse_deeplink_query(url: &str) -> Result<(String, String)> {
    let q = url
        .split_once('?')
        .map(|(_, q)| q)
        .ok_or_else(|| anyhow!("deeplink missing query string: {url}"))?;
    let mut name: Option<String> = None;
    let mut config: Option<String> = None;
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("name=") {
            name = Some(url_decode(v));
        } else if let Some(v) = pair.strip_prefix("config=") {
            config = Some(v.to_string());
        }
    }
    Ok((
        name.ok_or_else(|| anyhow!("deeplink missing `name` parameter"))?,
        config.ok_or_else(|| anyhow!("deeplink missing `config` parameter"))?,
    ))
}

/// Decode URL-safe base64 (no padding, `-_` alphabet) AND classic base64
/// (`+/` with padding) — Cursor uses the URL-safe form, but some
/// hand-pasted strings have the classic form.
fn decode_base64_url(s: &str) -> Result<Vec<u8>> {
    // Tiny inline base64 decoder so we don't pull in a crate just for
    // this. Accepts both `-_` (URL-safe) and `+/` (classic) alphabets;
    // pad if needed.
    let s = s.replace('-', "+").replace('_', "/");
    let pad_len = (4 - s.len() % 4) % 4;
    let padded = format!("{}{}", s, "=".repeat(pad_len));
    decode_classic_base64(&padded)
}

fn decode_classic_base64(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        bail!("base64: length not multiple of 4");
    }
    let mut i = 0;
    while i < bytes.len() {
        let chunk = &bytes[i..i + 4];
        let mut buf = [0u32; 4];
        for (j, &b) in chunk.iter().enumerate() {
            buf[j] = match b {
                b'A'..=b'Z' => (b - b'A') as u32,
                b'a'..=b'z' => (b - b'a' + 26) as u32,
                b'0'..=b'9' => (b - b'0' + 52) as u32,
                b'+' => 62,
                b'/' => 63,
                b'=' => 0, // padding handled below
                other => bail!("base64: invalid byte 0x{other:02x}"),
            };
        }
        let n = (buf[0] << 18) | (buf[1] << 12) | (buf[2] << 6) | buf[3];
        out.push(((n >> 16) & 0xff) as u8);
        if chunk[2] != b'=' {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if chunk[3] != b'=' {
            out.push((n & 0xff) as u8);
        }
        i += 4;
    }
    Ok(out)
}

/// Minimal `application/x-www-form-urlencoded` decoder (no crate).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_nibble(bytes[i + 1]).unwrap_or(0);
                let lo = hex_nibble(bytes[i + 2]).unwrap_or(0);
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrapped_mcp_servers() {
        let body = r#"{
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": {"TOKEN": "abc"}
                }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        let entries = import_path(tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        let (name, spec) = &entries[0];
        assert_eq!(name, "github");
        assert_eq!(spec.command.as_deref(), Some("npx"));
        assert_eq!(spec.args, vec!["-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(spec.env.get("TOKEN").unwrap(), "abc");
    }

    #[test]
    fn parses_bare_map_shape() {
        let body = r#"{
            "filesystem": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem"]
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        let entries = import_path(tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "filesystem");
    }

    #[test]
    fn parses_http_transport_entry() {
        let body = r#"{
            "mcpServers": {
                "notion": {
                    "type": "http",
                    "url": "https://mcp.notion.com/mcp"
                }
            }
        }"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        let entries = import_path(tmp.path()).unwrap();
        let (name, spec) = &entries[0];
        assert_eq!(name, "notion");
        assert_eq!(spec.transport.as_deref(), Some("http"));
        assert_eq!(spec.url.as_deref(), Some("https://mcp.notion.com/mcp"));
    }

    #[test]
    fn cursor_deeplink_decodes_base64_config() {
        // {"command":"npx","args":["-y","@scope/X"]} base64-url-encoded
        let json = r#"{"command":"npx","args":["-y","@scope/X"]}"#;
        let b64 = base64_url_encode(json.as_bytes());
        let url = format!(
            "cursor://anysphere.cursor-deeplink/mcp/install?name=demo&config={}",
            b64
        );
        let entries =
            import_cursor_deeplink(url.strip_prefix("cursor://").unwrap_or(&url)).unwrap();
        assert_eq!(entries[0].0, "demo");
        assert_eq!(entries[0].1.command.as_deref(), Some("npx"));
    }

    #[test]
    fn deeplink_missing_name_or_config_errors() {
        let bad = "cursor://x/?config=abc";
        assert!(import_cursor_deeplink(bad).is_err());
    }

    #[test]
    fn url_decode_basic() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("a+b"), "a b");
        assert_eq!(url_decode("%7B%7D"), "{}");
    }

    fn base64_url_encode(bytes: &[u8]) -> String {
        const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let b0 = bytes[i] as u32;
            let b1 = if i + 1 < bytes.len() {
                bytes[i + 1] as u32
            } else {
                0
            };
            let b2 = if i + 2 < bytes.len() {
                bytes[i + 2] as u32
            } else {
                0
            };
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHA[((n >> 18) & 63) as usize] as char);
            out.push(ALPHA[((n >> 12) & 63) as usize] as char);
            if i + 1 < bytes.len() {
                out.push(ALPHA[((n >> 6) & 63) as usize] as char);
            }
            if i + 2 < bytes.len() {
                out.push(ALPHA[(n & 63) as usize] as char);
            }
            i += 3;
        }
        out
    }
}
