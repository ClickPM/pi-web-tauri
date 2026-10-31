//! `%APPDATA%\Pi\hosts.json` — the optional list of remote pi-web hosts.
//!
//! The file is optional and hand-edited, so the rules here are about never
//! letting a typo take the app down:
//!
//!   * missing / empty / unparsable file -> "Windows 本地" only, no error;
//!   * one bad entry -> that entry is skipped with a warning, the rest load;
//!   * an unknown `active` -> falls back to local.
//!
//! The tunnel port range is the load-bearing rule. Every remote gets its own
//! `http://127.0.0.1:<localPort>` origin, which is what gives it its own
//! persistent `localStorage` (坑 1 again, per host). The range 45000..=48999 sits
//! above the embedded server's pool (30000..=44999, so `preload.js` can tell the
//! two apart from `location.port` alone) and below the Windows ephemeral range
//! (49152+, so the port is still free next launch).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::log_dbg;

/// Serialises every read-modify-write against `hosts.json` (`save_active` and
/// `save_config`) within this process so concurrent calls can never clobber a
/// newer file with an older snapshot or contend on the same temp file.
static FILE_IO_LOCK: Mutex<()> = Mutex::new(());
static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_tmp_path(dir: &Path) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{HOSTS_FILE}.{}.{seq}.tmp", std::process::id()))
}

pub const HOSTS_FILE: &str = "hosts.json";
pub const LOCAL_ID: &str = "local";
pub const LOCAL_LABEL: &str = "Windows 本地";

/// Tunnel ports. Must stay in step with the check in `inject/preload.js`.
pub const REMOTE_PORT_MIN: u16 = 45_000;
pub const REMOTE_PORT_MAX: u16 = 48_999;

const DEFAULT_REMOTE_HOST: &str = "127.0.0.1";
/// pi-web's documented default port, which is what the remote service
/// templates in `scripts/remote/` listen on.
const DEFAULT_REMOTE_PORT: u16 = 30_141;

/// One validated remote host.
#[derive(Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub id: String,
    pub label: String,
    /// A `~/.ssh/config` alias or `user@host`. Validated to never look like an
    /// option, because it is handed to ssh.exe as an argument.
    pub ssh_target: String,
    /// `None` = let `~/.ssh/config` decide.
    pub ssh_port: Option<u16>,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    /// Resolved: `passwordEnv` (when set and non-empty) wins over `password`.
    pub password: Option<String>,
}

// Hand-written so a password can never reach the debug log through `{:?}`.
impl std::fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostConfig")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("ssh_target", &self.ssh_target)
            .field("ssh_port", &self.ssh_port)
            .field("local_port", &self.local_port)
            .field("remote_host", &self.remote_host)
            .field("remote_port", &self.remote_port)
            .field("password", &self.password.as_ref().map(|_| "<set>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsFile {
    /// `LOCAL_ID` or the id of one of `hosts`.
    pub active: String,
    pub keep_local_running: bool,
    pub hosts: Vec<HostConfig>,
    /// Human-readable reasons for everything that was skipped or defaulted.
    pub warnings: Vec<String>,
    /// Whether a non-empty file was found. `active` is only ever written back
    /// into an existing file — the shell never creates one on its own.
    pub exists: bool,
}

impl Default for HostsFile {
    fn default() -> Self {
        Self {
            active: LOCAL_ID.into(),
            keep_local_running: true,
            hosts: Vec::new(),
            warnings: Vec::new(),
            exists: false,
        }
    }
}

impl HostsFile {
    pub fn find(&self, id: &str) -> Option<&HostConfig> {
        self.hosts.iter().find(|h| h.id == id)
    }
}

pub fn hosts_path(dir: &Path) -> PathBuf {
    dir.join(HOSTS_FILE)
}

/// Read `hosts.json` from `dir`. Never fails; problems end up in `warnings`
/// (and in the debug log).
pub fn load(dir: &Path) -> HostsFile {
    let path = hosts_path(dir);
    let file = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &env_lookup),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_dbg!("hosts: {} not present — local only", path.display());
            HostsFile::default()
        }
        Err(e) => HostsFile {
            warnings: vec![format!("无法读取 {}：{e}", path.display())],
            ..HostsFile::default()
        },
    };
    for w in &file.warnings {
        log_dbg!("hosts: {w}");
    }
    log_dbg!(
        "hosts: {} remote host(s) [{}], active={}, keepLocalRunning={}",
        file.hosts.len(),
        file.hosts
            .iter()
            .map(|h| format!("{}=127.0.0.1:{}", h.id, h.local_port))
            .collect::<Vec<_>>()
            .join(", "),
        file.active,
        file.keep_local_running
    );
    file
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Parse the file's text. `env` resolves `passwordEnv`; injected so tests do
/// not have to mutate the process environment.
pub fn parse(text: &str, env: &dyn Fn(&str) -> Option<String>) -> HostsFile {
    // Same BOM tolerance as port.json / theme-state.json: every PowerShell text
    // cmdlet writes one, and a JSON parser rejects it outright.
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return HostsFile::default();
    }

    let mut out = HostsFile {
        exists: true,
        ..HostsFile::default()
    };

    let root: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            out.warnings
                .push(format!("hosts.json 不是合法的 JSON（{e}），已忽略，仅显示 Windows 本地"));
            return out;
        }
    };
    let Some(obj) = root.as_object() else {
        out.warnings
            .push("hosts.json 顶层必须是一个对象，已忽略".into());
        return out;
    };

    match obj.get("keepLocalRunning") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => out.keep_local_running = *b,
        Some(_) => out
            .warnings
            .push("keepLocalRunning 必须是 true/false，按默认 true 处理".into()),
    }

    match obj.get("hosts") {
        None | Some(Value::Null) => {}
        Some(Value::Array(entries)) => {
            for (index, entry) in entries.iter().enumerate() {
                match validate_host(entry, env, &out.hosts) {
                    Ok(host) => out.hosts.push(host),
                    Err(reason) => out
                        .warnings
                        .push(format!("hosts[{index}] 已跳过：{reason}")),
                }
            }
        }
        Some(_) => out.warnings.push("hosts 必须是数组，已忽略".into()),
    }

    let requested = obj
        .get("active")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    out.active = if requested.is_empty() || requested.eq_ignore_ascii_case(LOCAL_ID) {
        LOCAL_ID.into()
    } else if out.find(requested).is_some() {
        requested.to_string()
    } else {
        out.warnings.push(format!(
            "active=\"{requested}\" 不是有效的主机 id，已回退到 Windows 本地"
        ));
        LOCAL_ID.into()
    };

    out
}

fn validate_host(
    entry: &Value,
    env: &dyn Fn(&str) -> Option<String>,
    accepted: &[HostConfig],
) -> Result<HostConfig, String> {
    let obj = entry.as_object().ok_or("条目必须是对象")?;

    let str_field = |key: &str| -> Result<Option<String>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let s = s.trim();
                Ok((!s.is_empty()).then(|| s.to_string()))
            }
            Some(_) => Err(format!("{key} 必须是字符串")),
        }
    };
    let port_field = |key: &str| -> Result<Option<u16>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or_else(|| format!("{key} 必须是 1-65535 之间的整数")),
        }
    };

    // --- id ------------------------------------------------------------------
    let id = str_field("id")?.ok_or("缺少 id")?;
    if id.eq_ignore_ascii_case(LOCAL_ID) {
        return Err("id 不能为 \"local\"（保留给 Windows 本地）".into());
    }
    if id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "id \"{id}\" 只能包含字母、数字、- _ .（最长 64）"
        ));
    }
    if accepted.iter().any(|h| h.id == id) {
        return Err(format!("id \"{id}\" 重复"));
    }

    // --- ssh -----------------------------------------------------------------
    let ssh_target = str_field("sshTarget")?.ok_or("缺少 sshTarget")?;
    // It is a positional argument to ssh.exe: anything starting with '-' would
    // be parsed as an option (`-oProxyCommand=...` runs arbitrary commands).
    if ssh_target.starts_with('-')
        || ssh_target
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "sshTarget \"{ssh_target}\" 不合法（不能以 - 开头，不能含空白）"
        ));
    }
    let ssh_port = port_field("sshPort")?;

    // --- tunnel --------------------------------------------------------------
    let local_port = port_field("localPort")?.ok_or("缺少 localPort")?;
    if !(REMOTE_PORT_MIN..=REMOTE_PORT_MAX).contains(&local_port) {
        return Err(format!(
            "localPort {local_port} 不在 {REMOTE_PORT_MIN}-{REMOTE_PORT_MAX} 之间\
             （30000-44999 留给本地内嵌服务，49152 起是 Windows 动态端口）"
        ));
    }
    if let Some(other) = accepted.iter().find(|h| h.local_port == local_port) {
        return Err(format!(
            "localPort {local_port} 与主机 \"{}\" 冲突",
            other.id
        ));
    }

    let remote_host = str_field("remoteHost")?.unwrap_or_else(|| DEFAULT_REMOTE_HOST.into());
    if remote_host.starts_with('-')
        || !remote_host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(format!("remoteHost \"{remote_host}\" 不合法"));
    }
    let remote_port = port_field("remotePort")?.unwrap_or(DEFAULT_REMOTE_PORT);

    // --- label / password ----------------------------------------------------
    let label = str_field("label")?
        .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| id.clone());

    let from_env = str_field("passwordEnv")?
        .and_then(|name| env(&name))
        .filter(|v| !v.is_empty());
    // Not trimmed: a password may legitimately start or end with a space.
    let from_file = match obj.get("password") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => (!s.is_empty()).then(|| s.clone()),
        Some(_) => return Err("password 必须是字符串".into()),
    };
    let password = from_env.or(from_file);

    Ok(HostConfig {
        id,
        label,
        ssh_target,
        ssh_port,
        local_port,
        remote_host,
        remote_port,
        password,
    })
}

/// Write `active` back into an existing `hosts.json`, keeping everything else
/// the user wrote byte-for-byte (key order, indentation, line endings, BOM):
/// only the value of the top-level `"active"` key is replaced, or the key is
/// inserted right after the opening brace. Atomic: written to a sibling temp
/// file and renamed over.
///
/// Returns Ok(false) when there was nothing to do (no file, or already set).
pub fn save_active(dir: &Path, id: &str) -> Result<bool, String> {
    let _io = FILE_IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = hosts_path(dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("无法读取 {}：{e}", path.display())),
    };
    let (bom, text) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", raw.as_str()),
    };
    if text.trim().is_empty() {
        return Ok(false);
    }
    let root: Value = serde_json::from_str(text)
        .map_err(|e| format!("{} 不是合法 JSON，未写回 active：{e}", path.display()))?;
    let obj = root
        .as_object()
        .ok_or_else(|| format!("{} 顶层不是对象，未写回 active", path.display()))?;
    if obj.get("active").and_then(Value::as_str) == Some(id) {
        return Ok(false);
    }

    let value = serde_json::to_string(id).map_err(|e| e.to_string())?;
    let edited = replace_top_level_value(text, "active", &value)
        .ok_or_else(|| format!("{} 结构异常，未写回 active", path.display()))?;
    // Never write something that no longer parses, or that lost the change.
    let check: Value = serde_json::from_str(&edited).map_err(|e| e.to_string())?;
    if check.get("active").and_then(Value::as_str) != Some(id) {
        return Err("写回 active 的自检失败，已放弃".into());
    }

    let tmp = unique_tmp_path(dir);
    std::fs::write(&tmp, format!("{bom}{edited}"))
        .map_err(|e| format!("无法写入 {}：{e}", tmp.display()))?;
    // std's rename replaces an existing target on Windows (MOVEFILE_REPLACE_EXISTING).
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("无法替换 {}：{e}", path.display())
    })?;
    Ok(true)
}

/// In a JSON text whose root is an object, replace the value of top-level key
/// `key` with the literal `new_value`, or insert `"key": new_value,` after the
/// opening brace when absent. A small scanner that tracks nesting and string
/// escapes — enough for valid JSON, which the caller has already checked.
fn replace_top_level_value(text: &str, key: &str, new_value: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let skip_ws = |mut j: usize| {
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        j
    };
    let mut i = 0;
    let mut depth = 0usize;
    let mut open = None;
    let mut expect_key = false;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                if depth == 1 {
                    open = Some(i);
                    expect_key = true;
                }
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            b',' if depth == 1 => {
                expect_key = true;
                i += 1;
            }
            b'"' => {
                let start = i;
                i = skip_string(bytes, i)?;
                if depth == 1 && expect_key {
                    expect_key = false;
                    let name: String = serde_json::from_str(&text[start..i]).ok()?;
                    let colon = skip_ws(i);
                    if bytes.get(colon) != Some(&b':') {
                        return None;
                    }
                    let value_start = skip_ws(colon + 1);
                    if name == key {
                        let end = value_end(bytes, value_start)?;
                        return Some(format!("{}{}{}", &text[..value_start], new_value, &text[end..]));
                    }
                    i = value_start;
                }
            }
            _ => i += 1,
        }
    }
    // Absent: insert after `{`, matching the first key's line break and indent.
    let open = open?;
    let rest = &text[open + 1..];
    let (newline, after) = if let Some(r) = rest.strip_prefix("\r\n") {
        ("\r\n", r)
    } else if let Some(r) = rest.strip_prefix('\n') {
        ("\n", r)
    } else {
        ("", rest)
    };
    let indent: String = after.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let insert = if rest.trim_start().starts_with('}') {
        format!("\"{key}\": {new_value}")
    } else if newline.is_empty() {
        format!("\"{key}\": {new_value},")
    } else {
        format!("{newline}{indent}\"{key}\": {new_value},")
    };
    Some(format!("{}{}{}", &text[..=open], insert, rest))
}

/// Index just past the closing quote of the string starting at `i`.
fn skip_string(bytes: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Index just past the JSON value starting at `i`.
fn value_end(bytes: &[u8], i: usize) -> Option<usize> {
    match bytes.get(i)? {
        b'"' => skip_string(bytes, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < bytes.len() {
                match bytes[j] {
                    b'"' => {
                        j = skip_string(bytes, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < bytes.len()
                && !matches!(bytes[j], b',' | b'}' | b']')
                && !bytes[j].is_ascii_whitespace()
            {
                j += 1;
            }
            Some(j)
        }
    }
}

// ---------------------------------------------------------------------------
// Host Manager UI types, SSH config alias helper & atomic config writer
// ---------------------------------------------------------------------------

/// Redacted view of one remote host handed to `hosts-manager`.
///
/// Carries neither the inline `password` nor the resolved value of
/// `passwordEnv` — only `has_password` and `password_from_env` booleans plus
/// the variable NAME so the form can edit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSummary {
    pub id: String,
    pub label: String,
    pub ssh_target: String,
    pub ssh_port: Option<u16>,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub password_env: Option<String>,
    pub has_password: bool,
    pub password_from_env: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostsUiSnapshot {
    pub exists: bool,
    pub active: String,
    pub keep_local_running: bool,
    pub hosts: Vec<HostSummary>,
    pub warnings: Vec<String>,
}

/// How an edit submission wants the inline `password` field updated on disk.
#[derive(Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum PasswordUpdate {
    /// Leave the existing inline `password` in `hosts.json` untouched.
    Keep {
        #[serde(default, rename = "fromId")]
        from_id: Option<String>,
    },
    /// Set a new inline `password` (an empty string clears it).
    Set { value: String },
    /// Explicitly remove any inline `password`.
    Clear,
}

impl std::fmt::Debug for PasswordUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keep { from_id } => f
                .debug_struct("Keep")
                .field("from_id", from_id)
                .finish(),
            Self::Set { value } => f
                .debug_struct("Set")
                .field("value", &if value.is_empty() { "<empty>" } else { "<redacted>" })
                .finish(),
            Self::Clear => f.write_str("Clear"),
        }
    }
}

/// One host submitted from the Host Manager form (for testing or saving).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDraft {
    pub id: String,
    #[serde(default)]
    pub original_id: Option<String>,
    pub label: String,
    pub ssh_target: String,
    #[serde(default)]
    pub ssh_port: Option<u16>,
    pub local_port: u16,
    #[serde(default)]
    pub remote_host: Option<String>,
    #[serde(default)]
    pub remote_port: Option<u16>,
    #[serde(default)]
    pub password_env: Option<String>,
    pub password: PasswordUpdate,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SerializedHost {
    id: String,
    label: String,
    ssh_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_port: Option<u16>,
    local_port: u16,
    remote_host: String,
    remote_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    password_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SerializedHostsFile {
    active: String,
    keep_local_running: bool,
    hosts: Vec<SerializedHost>,
}

/// Whether `name` is a safe environment-variable identifier (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn is_valid_env_name(name: &str) -> bool {
    let name = name.trim();
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 128
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check whether `name` is set and non-empty in the current process environment,
/// without ever returning its value.
pub fn is_env_var_set(name: &str) -> bool {
    let name = name.trim();
    if !is_valid_env_name(name) {
        return false;
    }
    env_lookup(name).filter(|v| !v.is_empty()).is_some()
}

/// Smallest port in `45000..=48999` that is neither listed in `used_ports` nor
/// bound on loopback right now.
pub fn next_available_local_port(
    used_ports: &[u16],
    is_free: impl Fn(u16) -> bool,
) -> Option<u16> {
    (REMOTE_PORT_MIN..=REMOTE_PORT_MAX)
        .find(|port| !used_ports.contains(port) && is_free(*port))
}

/// Extract plain `Host` aliases from `~/.ssh/config` text, skipping wildcard
/// patterns (`*`, `?`, `!`) and anything starting with `-`.
pub fn parse_ssh_config_hosts(text: &str) -> Vec<String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut aliases: Vec<String> = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let kw_end = line
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(line.len());
        if !line[..kw_end].eq_ignore_ascii_case("host") {
            continue;
        }
        let rest = line[kw_end..].trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        for token in rest.split_whitespace() {
            if token.starts_with('#') {
                break;
            }
            if token.is_empty()
                || token.len() > 256
                || token.starts_with('-')
                || token.chars().any(|c| {
                    matches!(c, '*' | '?' | '!' | '"' | '\'' | '\\') || c.is_control()
                })
            {
                continue;
            }
            if !aliases.iter().any(|existing| existing == token) {
                aliases.push(token.to_string());
            }
        }
    }
    aliases
}

/// Load non-wildcard `Host` aliases from `%USERPROFILE%\.ssh\config`.
/// Never fails: returns an empty list if the file is absent or unreadable.
pub fn load_ssh_config_hosts() -> Vec<String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let Some(home) = home else {
        return Vec::new();
    };
    let path = home.join(".ssh").join("config");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_ssh_config_hosts(&text),
        Err(_) => Vec::new(),
    }
}

/// Load a redacted snapshot of `hosts.json` for the Host Manager window.
pub fn load_ui_snapshot(dir: &Path) -> HostsUiSnapshot {
    let path = hosts_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_ui_snapshot(&text, &env_lookup),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HostsUiSnapshot {
            exists: false,
            active: LOCAL_ID.into(),
            keep_local_running: true,
            hosts: Vec::new(),
            warnings: Vec::new(),
        },
        Err(e) => HostsUiSnapshot {
            exists: false,
            active: LOCAL_ID.into(),
            keep_local_running: true,
            hosts: Vec::new(),
            warnings: vec![format!("无法读取 {}：{e}", path.display())],
        },
    }
}

pub fn parse_ui_snapshot(text: &str, env: &dyn Fn(&str) -> Option<String>) -> HostsUiSnapshot {
    let parsed = parse(text, env);
    let trimmed = text.trim_start_matches('\u{feff}');
    if trimmed.trim().is_empty() {
        return HostsUiSnapshot {
            exists: false,
            active: LOCAL_ID.into(),
            keep_local_running: true,
            hosts: Vec::new(),
            warnings: Vec::new(),
        };
    }

    // Pull password metadata per validated id from the raw JSON object without
    // ever copying the secret into `HostSummary`.
    let mut meta: HashMap<String, (Option<String>, bool)> = HashMap::new();
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) {
        if let Some(Value::Array(entries)) = obj.get("hosts") {
            for entry in entries {
                let Some(eobj) = entry.as_object() else { continue };
                let Some(id) = eobj.get("id").and_then(Value::as_str).map(str::trim) else {
                    continue;
                };
                if id.is_empty() || meta.contains_key(id) {
                    continue;
                }
                let pw_env = eobj
                    .get("passwordEnv")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let has_file_pw = eobj
                    .get("password")
                    .and_then(Value::as_str)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                meta.insert(id.to_owned(), (pw_env, has_file_pw));
            }
        }
    }

    let hosts = parsed
        .hosts
        .into_iter()
        .map(|h| {
            let (password_env, has_password) = meta.remove(&h.id).unwrap_or((None, false));
            let password_from_env = password_env
                .as_deref()
                .and_then(env)
                .filter(|v| !v.is_empty())
                .is_some();
            HostSummary {
                id: h.id,
                label: h.label,
                ssh_target: h.ssh_target,
                ssh_port: h.ssh_port,
                local_port: h.local_port,
                remote_host: h.remote_host,
                remote_port: h.remote_port,
                password_env,
                has_password,
                password_from_env,
            }
        })
        .collect();

    HostsUiSnapshot {
        exists: parsed.exists,
        active: parsed.active,
        keep_local_running: parsed.keep_local_running,
        hosts,
        warnings: parsed.warnings,
    }
}

fn read_existing_disk_entries(dir: &Path) -> (HashSet<String>, HashMap<String, String>) {
    let mut ids = HashSet::new();
    let mut passwords = HashMap::new();
    let Ok(raw) = std::fs::read_to_string(hosts_path(dir)) else {
        return (ids, passwords);
    };
    let trimmed = raw.trim_start_matches('\u{feff}');
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) else {
        return (ids, passwords);
    };
    let Some(Value::Array(entries)) = obj.get("hosts") else {
        return (ids, passwords);
    };
    for entry in entries {
        let Some(eobj) = entry.as_object() else { continue };
        let Some(id) = eobj.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() || !ids.insert(id.to_owned()) {
            continue;
        }
        if let Some(pw) = eobj
            .get("password")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            passwords.insert(id.to_owned(), pw.to_owned());
        }
    }
    (ids, passwords)
}

fn resolve_inline_password(
    draft: &HostDraft,
    validated_id: &str,
    existing_ids: &HashSet<String>,
    existing_passwords: &HashMap<String, String>,
) -> Option<String> {
    match &draft.password {
        PasswordUpdate::Keep { from_id } => {
            let explicit = from_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    draft
                        .original_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                });
            match explicit {
                Some(key) => existing_passwords.get(key).cloned(),
                // Brand-new entry (`original_id == None`, `from_id == None`): if
                // its id happens to reuse the id of a host that was just deleted
                // in the same edit session, it must NOT inherit the deleted
                // host's secret.
                None if existing_ids.contains(validated_id) => None,
                None => existing_passwords.get(validated_id).cloned(),
            }
        }
        PasswordUpdate::Set { value } => (!value.is_empty()).then(|| value.clone()),
        PasswordUpdate::Clear => None,
    }
}

fn draft_to_serialized(
    draft: &HostDraft,
    existing_ids: &HashSet<String>,
    existing_passwords: &HashMap<String, String>,
    accepted: &[SerializedHost],
) -> Result<SerializedHost, String> {
    let id = draft.id.trim();
    if id.is_empty() {
        return Err("主机 ID 不能为空".into());
    }
    if id.eq_ignore_ascii_case(LOCAL_ID) {
        return Err("主机 ID 不能为 \"local\"（保留给 Windows 本地）".into());
    }
    if id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "主机 ID \"{id}\" 只能包含字母、数字、- _ .（最长 64）"
        ));
    }
    if accepted.iter().any(|h| h.id == id) {
        return Err(format!("主机 ID \"{id}\" 重复"));
    }

    let ssh_target = draft.ssh_target.trim();
    if ssh_target.is_empty() {
        return Err(format!("主机「{id}」缺少 SSH 目标 (sshTarget)"));
    }
    if ssh_target.len() > 256
        || ssh_target.starts_with('-')
        || ssh_target
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "SSH 目标 \"{ssh_target}\" 不合法（不能以 - 开头，不能含空白或控制字符）"
        ));
    }

    if draft.ssh_port == Some(0) {
        return Err("SSH 端口必须是 1-65535 之间的整数".into());
    }

    let local_port = draft.local_port;
    if !(REMOTE_PORT_MIN..=REMOTE_PORT_MAX).contains(&local_port) {
        return Err(format!(
            "本机隧道端口 {local_port} 不在 {REMOTE_PORT_MIN}-{REMOTE_PORT_MAX} 之间"
        ));
    }
    if let Some(other) = accepted.iter().find(|h| h.local_port == local_port) {
        return Err(format!(
            "本机隧道端口 {local_port} 与主机「{}」冲突",
            other.label
        ));
    }

    let remote_host = draft
        .remote_host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REMOTE_HOST);
    if remote_host.len() > 256
        || remote_host.starts_with('-')
        || !remote_host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(format!("远端监听地址 \"{remote_host}\" 不合法"));
    }

    let remote_port = draft.remote_port.unwrap_or(DEFAULT_REMOTE_PORT);
    if remote_port == 0 {
        return Err("远端端口必须是 1-65535 之间的整数".into());
    }

    let label: String = draft
        .label
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned();
    let label = if label.is_empty() {
        id.to_owned()
    } else if label.chars().count() > 128 {
        label.chars().take(128).collect()
    } else {
        label
    };

    let password_env = draft
        .password_env
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(ref env_name) = password_env {
        if !is_valid_env_name(env_name) {
            return Err(format!(
                "环境变量名 \"{env_name}\" 不合法（仅允许字母、数字与下划线，且不能以数字开头）"
            ));
        }
    }

    let password = resolve_inline_password(draft, id, existing_ids, existing_passwords);

    Ok(SerializedHost {
        id: id.to_owned(),
        label,
        ssh_target: ssh_target.to_owned(),
        ssh_port: draft.ssh_port,
        local_port,
        remote_host: remote_host.to_owned(),
        remote_port,
        password_env,
        password,
    })
}

/// Validate a single draft from the Host Manager form for "测试连接" without
/// touching disk. Also returns `Some(env_name)` when `passwordEnv` was filled
/// in, is currently unset/empty in the OS environment, and no fallback inline
/// password applies.
pub fn validate_draft_for_probe(
    dir: &Path,
    draft: &HostDraft,
) -> Result<(HostConfig, Option<String>), String> {
    let (existing_ids, existing_passwords) = read_existing_disk_entries(dir);
    // Allow an empty id during a quick probe before the user named the host by
    // substituting a harmless placeholder for validation.
    let mut probe_draft = draft.clone();
    if probe_draft.id.trim().is_empty() {
        probe_draft.id = "probe-host".into();
    }
    let ser = draft_to_serialized(&probe_draft, &existing_ids, &existing_passwords, &[])?;

    let from_env = ser
        .password_env
        .as_deref()
        .and_then(env_lookup)
        .filter(|v| !v.is_empty());
    let unset_env = match (&ser.password_env, &from_env, &ser.password) {
        (Some(name), None, None) => Some(name.clone()),
        _ => None,
    };
    let password = from_env.or(ser.password);

    Ok((
        HostConfig {
            id: ser.id,
            label: ser.label,
            ssh_target: ser.ssh_target,
            ssh_port: ser.ssh_port,
            local_port: ser.local_port,
            remote_host: ser.remote_host,
            remote_port: ser.remote_port,
            password,
        },
        unset_env,
    ))
}

/// Validate and atomically write `%APPDATA%\Pi\hosts.json` as standard UTF-8
/// JSON (no BOM), preserving untouched inline passwords from any prior file.
///
/// Returns the resolved `"active"` id written to disk (which follows an id
/// rename when `preferred_active` matched `original_id`, or falls back to
/// `LOCAL_ID` if the active host was deleted).
pub fn save_config(
    dir: &Path,
    keep_local_running: bool,
    drafts: &[HostDraft],
    preferred_active: &str,
) -> Result<String, String> {
    let _io = FILE_IO_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (existing_ids, existing_passwords) = read_existing_disk_entries(dir);
    let mut serialized: Vec<SerializedHost> = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let host = draft_to_serialized(draft, &existing_ids, &existing_passwords, &serialized)?;
        serialized.push(host);
    }

    let pref = preferred_active.trim();
    let active = if pref.is_empty() || pref.eq_ignore_ascii_case(LOCAL_ID) {
        LOCAL_ID.to_owned()
    } else if let Some((_, tracked)) = drafts.iter().zip(serialized.iter()).find(|(d, _)| {
        d.original_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            == Some(pref)
    }) {
        // Priority 1: follow the exact entry that originated as `pref`, even
        // when its id was renamed AND another entry took over the old `pref` id.
        tracked.id.clone()
    } else if existing_ids.contains(pref) {
        // `pref` existed on disk before this save, yet no surviving entry has
        // `original_id == Some(pref)`: the active host was deleted. Even if the
        // user added a brand-new host reusing the same id string, fall back to
        // local rather than silently treating the new host as the old session.
        LOCAL_ID.to_owned()
    } else if serialized.iter().any(|h| h.id == pref) {
        // Fresh file creation where `pref` never existed on disk yet.
        pref.to_owned()
    } else {
        LOCAL_ID.to_owned()
    };

    let doc = SerializedHostsFile {
        active: active.clone(),
        keep_local_running,
        hosts: serialized,
    };
    let mut text =
        serde_json::to_string_pretty(&doc).map_err(|e| format!("序列化 hosts.json 失败：{e}"))?;
    text.push('\n');

    // Round-trip self-check through `parse()` before touching disk: guarantees
    // the writer and reader can never disagree on validation rules.
    let check = parse(&text, &|_| None);
    if !check.warnings.is_empty() || check.hosts.len() != doc.hosts.len() {
        return Err(format!(
            "hosts.json 写前自检失败：{}",
            check.warnings.join("；")
        ));
    }

    std::fs::create_dir_all(dir)
        .map_err(|e| format!("无法创建配置目录 {}：{e}", dir.display()))?;
    let path = hosts_path(dir);
    let tmp = unique_tmp_path(dir);
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| format!("无法写入临时文件 {}：{e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("无法原子替换 {}：{e}", path.display())
    })?;

    log_dbg!(
        "hosts: saved {} host(s) to {} (active={}, keepLocalRunning={})",
        doc.hosts.len(),
        path.display(),
        active,
        keep_local_running
    );
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn host(id: &str, port: u16) -> String {
        format!(
            r#"{{"id":"{id}","label":"L {id}","sshTarget":"{id}","localPort":{port}}}"#
        )
    }

    fn file(hosts: &[String], extra: &str) -> String {
        format!(r#"{{ {extra} "hosts": [{}] }}"#, hosts.join(","))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "piwd-hosts-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn missing_file_is_local_only() {
        let dir = temp_dir("missing");
        let f = load(&dir);
        assert_eq!(f, HostsFile::default());
        assert_eq!(f.active, LOCAL_ID);
        assert!(f.hosts.is_empty());
        assert!(!f.exists);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_and_whitespace_files_are_local_only() {
        for text in ["", "   \r\n", "\u{feff}", "\u{feff}  \n"] {
            let f = parse(text, &no_env);
            assert_eq!(f, HostsFile::default(), "text={text:?}");
        }
    }

    #[test]
    fn invalid_json_is_local_only_with_warning() {
        let f = parse("{ not json", &no_env);
        assert!(f.hosts.is_empty());
        assert_eq!(f.active, LOCAL_ID);
        assert_eq!(f.warnings.len(), 1);
        assert!(f.exists);
    }

    #[test]
    fn utf8_bom_is_tolerated() {
        let text = format!(
            "\u{feff}{}",
            file(&[host("mac-mini", 45001)], r#""active":"mac-mini","#)
        );
        let f = parse(&text, &no_env);
        assert!(f.warnings.is_empty(), "{:?}", f.warnings);
        assert_eq!(f.active, "mac-mini");
        assert_eq!(f.hosts.len(), 1);
    }

    #[test]
    fn bom_file_loads_from_disk() {
        let dir = temp_dir("bom");
        let text = format!("\u{feff}{}", file(&[host("arch-130", 45002)], ""));
        std::fs::write(hosts_path(&dir), text).unwrap();
        let f = load(&dir);
        assert_eq!(f.hosts.len(), 1);
        assert_eq!(f.hosts[0].id, "arch-130");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn full_example_parses_with_defaults() {
        let text = r#"{
          "active": "local",
          "keepLocalRunning": false,
          "hosts": [
            { "id": "mac-mini", "label": "Mac mini", "sshTarget": "mac", "sshPort": 22,
              "localPort": 45001, "remoteHost": "127.0.0.1", "remotePort": 30141,
              "passwordEnv": "PI_WEB_REMOTE_PASSWORD_MAC", "password": "" },
            { "id": "arch-130", "sshTarget": "chenkun@192.168.100.130", "localPort": 45002 }
          ]
        }"#;
        let f = parse(text, &no_env);
        assert!(f.warnings.is_empty(), "{:?}", f.warnings);
        assert!(!f.keep_local_running);
        let mac = f.find("mac-mini").unwrap();
        assert_eq!(mac.label, "Mac mini");
        assert_eq!(mac.ssh_port, Some(22));
        assert_eq!(mac.password, None, "empty password + unset env = no password");
        let arch = f.find("arch-130").unwrap();
        assert_eq!(arch.label, "arch-130", "label defaults to id");
        assert_eq!(arch.ssh_port, None, "absent sshPort defers to ~/.ssh/config");
        assert_eq!(arch.remote_host, "127.0.0.1");
        assert_eq!(arch.remote_port, 30141);
    }

    #[test]
    fn keep_local_running_defaults_to_true() {
        let f = parse(&file(&[], ""), &no_env);
        assert!(f.keep_local_running);
    }

    #[test]
    fn ports_outside_the_tunnel_range_are_rejected() {
        // Local server pool, the gap above 48999, the Windows ephemeral range,
        // and junk values.
        for bad in ["30000", "35566", "44999", "49000", "49152", "60000", "0", "70000", "-1", "\"45001\""] {
            let text = format!(
                r#"{{"hosts":[{{"id":"x","sshTarget":"x","localPort":{bad}}}]}}"#
            );
            let f = parse(&text, &no_env);
            assert!(f.hosts.is_empty(), "localPort {bad} must be rejected");
            assert_eq!(f.warnings.len(), 1, "localPort {bad}: {:?}", f.warnings);
        }
        for good in [45000u16, 45001, 48999] {
            let f = parse(&file(&[host("x", good)], ""), &no_env);
            assert_eq!(f.hosts.len(), 1, "localPort {good} must be accepted");
        }
    }

    #[test]
    fn one_bad_entry_does_not_sink_the_rest() {
        let f = parse(
            &file(&[host("a", 45001), host("b", 35000), host("c", 45003)], ""),
            &no_env,
        );
        assert_eq!(
            f.hosts.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(f.warnings.len(), 1);
        assert!(f.warnings[0].contains("hosts[1]"));
    }

    #[test]
    fn duplicate_ids_and_ports_keep_the_first() {
        let f = parse(
            &file(&[host("mac", 45001), host("mac", 45002), host("arch", 45001)], ""),
            &no_env,
        );
        assert_eq!(f.hosts.len(), 1);
        assert_eq!(f.hosts[0].local_port, 45001);
        assert_eq!(f.warnings.len(), 2);
        assert!(f.warnings[0].contains("重复"));
        assert!(f.warnings[1].contains("冲突"));
    }

    #[test]
    fn reserved_and_malformed_ids_are_rejected() {
        for bad in ["local", "LOCAL", "has space", "", "a/b"] {
            let text = format!(
                r#"{{"hosts":[{{"id":"{bad}","sshTarget":"x","localPort":45001}}]}}"#
            );
            let f = parse(&text, &no_env);
            assert!(f.hosts.is_empty(), "id {bad:?} must be rejected");
        }
    }

    #[test]
    fn option_like_ssh_targets_are_rejected() {
        for bad in ["-oProxyCommand=calc", "-v", "mac mini", ""] {
            let text = format!(
                r#"{{"hosts":[{{"id":"x","sshTarget":"{bad}","localPort":45001}}]}}"#
            );
            let f = parse(&text, &no_env);
            assert!(f.hosts.is_empty(), "sshTarget {bad:?} must be rejected");
        }
        let text = r#"{"hosts":[{"id":"x","sshTarget":"x","localPort":45001,"remoteHost":"-bad"}]}"#;
        assert!(parse(text, &no_env).hosts.is_empty());
    }

    #[test]
    fn env_password_overrides_file_password() {
        let text = r#"{"hosts":[{"id":"x","sshTarget":"x","localPort":45001,
                       "passwordEnv":"PW_X","password":"from-file"}]}"#;
        let env = |name: &str| (name == "PW_X").then(|| "from-env".to_string());
        assert_eq!(parse(text, &env).hosts[0].password.as_deref(), Some("from-env"));

        // Unset or EMPTY env var falls back to the file.
        assert_eq!(parse(text, &no_env).hosts[0].password.as_deref(), Some("from-file"));
        let empty = |_: &str| Some(String::new());
        assert_eq!(parse(text, &empty).hosts[0].password.as_deref(), Some("from-file"));

        // Neither -> no password.
        let none = r#"{"hosts":[{"id":"x","sshTarget":"x","localPort":45001,"passwordEnv":"PW_X"}]}"#;
        assert_eq!(parse(none, &no_env).hosts[0].password, None);
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let text = r#"{"hosts":[{"id":"x","sshTarget":"x","localPort":45001,"password":"hunter2"}]}"#;
        let f = parse(text, &no_env);
        let dbg = format!("{:?}", f);
        assert!(!dbg.contains("hunter2"), "{dbg}");
    }

    #[test]
    fn unknown_active_falls_back_to_local() {
        let f = parse(&file(&[host("a", 45001)], r#""active":"gone","#), &no_env);
        assert_eq!(f.active, LOCAL_ID);
        assert_eq!(f.warnings.len(), 1);
        // An active pointing at a REJECTED entry also falls back.
        let f = parse(&file(&[host("a", 30001)], r#""active":"a","#), &no_env);
        assert_eq!(f.active, LOCAL_ID);
    }

    #[test]
    fn save_active_preserves_the_rest_of_the_file() {
        let dir = temp_dir("save");
        let original = "\u{feff}{\r\n  \"zFirst\": 1,\r\n  \"active\": \"local\",\r\n  \"keepLocalRunning\": true,\r\n  \"hosts\": [{\"id\":\"mac\",\"sshTarget\":\"mac\",\"localPort\":45001,\"active\":\"nested\"}]\r\n}\r\n";
        std::fs::write(hosts_path(&dir), original).unwrap();

        assert_eq!(save_active(&dir, "mac"), Ok(true));
        let written = std::fs::read_to_string(hosts_path(&dir)).unwrap();
        // Byte-for-byte identical apart from the one top-level value.
        assert_eq!(
            written,
            original.replace("\"active\": \"local\"", "\"active\": \"mac\"")
        );
        assert!(!dir.join("hosts.json.tmp").exists());

        // Re-parses into the same active host.
        assert_eq!(load(&dir).active, "mac");
        // Idempotent.
        assert_eq!(save_active(&dir, "mac"), Ok(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_active_inserts_when_absent() {
        let dir = temp_dir("insert");
        std::fs::write(
            hosts_path(&dir),
            "{\n  \"hosts\": [{\"id\":\"mac\",\"sshTarget\":\"mac\",\"localPort\":45001}]\n}\n",
        )
        .unwrap();
        assert_eq!(save_active(&dir, "mac"), Ok(true));
        let written = std::fs::read_to_string(hosts_path(&dir)).unwrap();
        assert!(written.starts_with("{\n  \"active\": \"mac\",\n  \"hosts\""), "{written}");
        assert_eq!(load(&dir).active, "mac");
        // Odd but valid layouts still round-trip.
        for text in [r#"{"hosts":[]}"#, "{}", r#"{ "active" : null , "x":{"active":1}}"#] {
            let out = replace_top_level_value(text, "active", "\"a\"").unwrap();
            let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{out}: {e}"));
            assert_eq!(v["active"], "a", "{out}");
            assert_eq!(v.get("x").map(|x| x["active"].clone()), v.get("x").map(|_| Value::from(1)));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_active_never_creates_or_clobbers() {
        let dir = temp_dir("noclobber");
        assert_eq!(save_active(&dir, "local"), Ok(false));
        assert!(!hosts_path(&dir).exists(), "must not create hosts.json");

        std::fs::write(hosts_path(&dir), "{ broken").unwrap();
        assert!(save_active(&dir, "local").is_err());
        assert_eq!(std::fs::read_to_string(hosts_path(&dir)).unwrap(), "{ broken");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ssh_config_extracts_only_plain_host_aliases() {
        let cfg = "\u{feff}\
            # Global defaults\n\
            Host *\n\
              ServerAliveInterval 15\n\
            \n\
            Host mac mac-mini.local *.lan !mac-old -oProxyCommand=calc\n\
              HostName 192.168.100.10\n\
              User agegr\n\
            host = 130   arch-box?  dev_01 # inline comment\n\
            HOST\tprod-1\tmac\n\
            Match host something\n\
              User root\n";
        assert_eq!(
            parse_ssh_config_hosts(cfg),
            vec!["mac", "mac-mini.local", "130", "dev_01", "prod-1"]
        );
    }

    #[test]
    fn next_available_port_skips_configured_and_bound_ports() {
        let used = [45000u16, 45001, 45003];
        // 45002 is not in `used`, but pretend the OS reports it busy; 45004 wins.
        let picked = next_available_local_port(&used, |p| p != 45002);
        assert_eq!(picked, Some(45004));

        // Nothing used, all free -> starts at REMOTE_PORT_MIN (45000).
        assert_eq!(next_available_local_port(&[], |_| true), Some(45000));
        // When 45000 is used, 45001 is next.
        assert_eq!(next_available_local_port(&[45000], |_| true), Some(45001));
        // Exhausted pool -> None.
        assert_eq!(next_available_local_port(&[], |_| false), None);
    }

    #[test]
    fn ui_snapshot_never_exposes_passwords_or_env_values() {
        let text = r#"{
          "active": "arch-130",
          "keepLocalRunning": false,
          "hosts": [
            {
              "id": "mac-mini",
              "label": "Mac mini",
              "sshTarget": "mac",
              "localPort": 45001,
              "passwordEnv": "PW_MAC",
              "password": "file-secret-mac"
            },
            {
              "id": "arch-130",
              "label": "Arch",
              "sshTarget": "130",
              "localPort": 45002,
              "password": "file-secret-130"
            },
            {
              "id": "dev-box",
              "sshTarget": "dev",
              "localPort": 45003,
              "passwordEnv": "PW_UNSET"
            }
          ]
        }"#;
        let env = |name: &str| (name == "PW_MAC").then(|| "env-secret-mac".to_string());
        let snap = parse_ui_snapshot(text, &env);
        assert!(!snap.keep_local_running);
        assert_eq!(snap.hosts.len(), 3);

        assert!(snap.hosts[0].has_password);
        assert!(snap.hosts[0].password_from_env);
        assert_eq!(snap.hosts[0].password_env.as_deref(), Some("PW_MAC"));

        assert!(snap.hosts[1].has_password);
        assert!(!snap.hosts[1].password_from_env);
        assert_eq!(snap.hosts[1].password_env, None);

        assert!(!snap.hosts[2].has_password);
        assert!(!snap.hosts[2].password_from_env);
        assert_eq!(snap.hosts[2].password_env.as_deref(), Some("PW_UNSET"));

        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("file-secret-mac"), "{json}");
        assert!(!json.contains("file-secret-130"), "{json}");
        assert!(!json.contains("env-secret-mac"), "{json}");
    }

    #[test]
    fn save_config_creates_utf8_no_bom_and_handles_password_modes() {
        let dir = temp_dir("save-cfg");
        assert!(!hosts_path(&dir).exists());

        // 1. Fresh creation when hosts.json does not exist yet.
        let initial = vec![
            HostDraft {
                id: "mac-mini".into(),
                original_id: None,
                label: "Mac mini".into(),
                ssh_target: "mac".into(),
                ssh_port: Some(22),
                local_port: 45001,
                remote_host: None,
                remote_port: None,
                password_env: Some("PIWD_TEST_ONLY_ENV_MAC".into()),
                password: PasswordUpdate::Set {
                    value: "alpha-secret".into(),
                },
            },
            HostDraft {
                id: "arch-130".into(),
                original_id: None,
                label: "Arch 130".into(),
                ssh_target: "130".into(),
                ssh_port: None,
                local_port: 45002,
                remote_host: Some("127.0.0.1".into()),
                remote_port: Some(30141),
                password_env: None,
                password: PasswordUpdate::Set {
                    value: "  spaced-secret  ".into(),
                },
            },
        ];
        save_config(&dir, false, &initial, "arch-130").unwrap();

        let bytes = std::fs::read(hosts_path(&dir)).unwrap();
        assert!(
            !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            "saved hosts.json must be UTF-8 without BOM"
        );
        let loaded = parse(&std::fs::read_to_string(hosts_path(&dir)).unwrap(), &no_env);
        assert_eq!(loaded.active, "arch-130");
        assert!(!loaded.keep_local_running);
        assert_eq!(loaded.hosts[0].password.as_deref(), Some("alpha-secret"));
        assert_eq!(loaded.hosts[1].password.as_deref(), Some("  spaced-secret  "));

        // 2. Edit: keep password on renamed host, clear password on second host, add third.
        let edited = vec![
            HostDraft {
                id: "mac-studio".into(),
                original_id: Some("mac-mini".into()),
                label: "Mac Studio".into(),
                ssh_target: "mac".into(),
                ssh_port: None,
                local_port: 45001,
                remote_host: Some("127.0.0.1".into()),
                remote_port: Some(30141),
                password_env: None,
                password: PasswordUpdate::Keep { from_id: None },
            },
            HostDraft {
                id: "arch-130".into(),
                original_id: Some("arch-130".into()),
                label: "Arch 130".into(),
                ssh_target: "130".into(),
                ssh_port: None,
                local_port: 45002,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Clear,
            },
            HostDraft {
                id: "box-3".into(),
                original_id: None,
                label: "Box 3".into(),
                ssh_target: "user@10.0.0.3".into(),
                ssh_port: Some(2222),
                local_port: 45003,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Keep { from_id: None },
            },
        ];
        save_config(&dir, true, &edited, "mac-mini").unwrap();

        let loaded2 = parse(&std::fs::read_to_string(hosts_path(&dir)).unwrap(), &no_env);
        assert!(loaded2.keep_local_running);
        // Active followed the renamed original_id mac-mini -> mac-studio.
        assert_eq!(loaded2.active, "mac-studio");
        assert_eq!(
            loaded2.find("mac-studio").unwrap().password.as_deref(),
            Some("alpha-secret"),
            "untouched password must survive edits and id renames"
        );
        assert_eq!(
            loaded2.find("arch-130").unwrap().password,
            None,
            "cleared password must be removed"
        );
        assert_eq!(loaded2.find("box-3").unwrap().password, None);

        // 3. Reject injection / duplicate ports without clobbering existing file.
        let bad = vec![HostDraft {
            id: "evil".into(),
            original_id: None,
            label: "Evil".into(),
            ssh_target: "-oProxyCommand=calc.exe".into(),
            ssh_port: None,
            local_port: 45005,
            remote_host: None,
            remote_port: None,
            password_env: None,
            password: PasswordUpdate::Clear,
        }];
        assert!(save_config(&dir, true, &bad, "local").is_err());
        assert_eq!(load(&dir).hosts.len(), 3, "failed save must leave disk untouched");

        // 4. Probe validation resolves kept file password on backend and rejects bad options.
        let (probe_cfg, unset_env) = validate_draft_for_probe(
            &dir,
            &HostDraft {
                id: "mac-studio".into(),
                original_id: Some("mac-studio".into()),
                label: "Mac Studio".into(),
                ssh_target: "mac".into(),
                ssh_port: None,
                local_port: 45001,
                remote_host: None,
                remote_port: None,
                password_env: Some("PIWD_DEFINITELY_UNSET_ENV_999".into()),
                password: PasswordUpdate::Keep { from_id: None },
            },
        )
        .unwrap();
        assert_eq!(probe_cfg.password.as_deref(), Some("alpha-secret"));
        // Fallback file password is present, so `unset_env` is None.
        assert_eq!(unset_env, None);

        let (probe_no_pw, unset_env2) = validate_draft_for_probe(
            &dir,
            &HostDraft {
                id: "new-box".into(),
                original_id: None,
                label: "New".into(),
                ssh_target: "10.0.0.9".into(),
                ssh_port: None,
                local_port: 45010,
                remote_host: None,
                remote_port: None,
                password_env: Some("PIWD_DEFINITELY_UNSET_ENV_999".into()),
                password: PasswordUpdate::Clear,
            },
        )
        .unwrap();
        assert_eq!(probe_no_pw.password, None);
        assert_eq!(unset_env2.as_deref(), Some("PIWD_DEFINITELY_UNSET_ENV_999"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn next_available_port_skips_real_bound_loopback_socket() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        // Bind a specific port in the tunnel range if free, to verify real OS check.
        if let Ok(bound) = std::net::TcpListener::bind(("127.0.0.1", REMOTE_PORT_MIN)) {
            let chosen = next_available_local_port(&[], crate::server::is_free).unwrap();
            assert_ne!(chosen, REMOTE_PORT_MIN);
            drop(bound);
        }
        drop(listener);
    }

    #[test]
    fn save_config_tracks_identity_across_id_swap_and_delete_then_reuse() {
        let dir = temp_dir("id-swap");
        let initial = vec![
            HostDraft {
                id: "a".into(),
                original_id: None,
                label: "Host A".into(),
                ssh_target: "host-a".into(),
                ssh_port: None,
                local_port: 45001,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Set { value: "pw-a".into() },
            },
            HostDraft {
                id: "b".into(),
                original_id: None,
                label: "Host B".into(),
                ssh_target: "host-b".into(),
                ssh_port: None,
                local_port: 45002,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Set { value: "pw-b".into() },
            },
        ];
        save_config(&dir, true, &initial, "a").unwrap();

        // 1. Rename active "a" -> "c" while simultaneously renaming "b" -> "a".
        let swapped = vec![
            HostDraft {
                id: "c".into(),
                original_id: Some("a".into()),
                label: "Host A->C".into(),
                ssh_target: "host-a".into(),
                ssh_port: None,
                local_port: 45001,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Keep { from_id: None },
            },
            HostDraft {
                id: "a".into(),
                original_id: Some("b".into()),
                label: "Host B->A".into(),
                ssh_target: "host-b".into(),
                ssh_port: None,
                local_port: 45002,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Keep { from_id: None },
            },
        ];
        let resolved = save_config(&dir, true, &swapped, "a").unwrap();
        assert_eq!(resolved, "c", "active must follow original_id a -> c, not new a");
        let loaded = parse(&std::fs::read_to_string(hosts_path(&dir)).unwrap(), &no_env);
        assert_eq!(loaded.active, "c");
        assert_eq!(loaded.find("c").unwrap().password.as_deref(), Some("pw-a"));
        assert_eq!(loaded.find("a").unwrap().password.as_deref(), Some("pw-b"));

        // 2. Delete active "c", and in the same save add a brand-new host reusing id "c".
        let deleted_and_reused = vec![
            HostDraft {
                id: "a".into(),
                original_id: Some("a".into()),
                label: "Host B->A".into(),
                ssh_target: "host-b".into(),
                ssh_port: None,
                local_port: 45002,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Keep { from_id: None },
            },
            HostDraft {
                id: "c".into(),
                original_id: None,
                label: "Brand New C".into(),
                ssh_target: "new-c".into(),
                ssh_port: None,
                local_port: 45003,
                remote_host: None,
                remote_port: None,
                password_env: None,
                password: PasswordUpdate::Keep { from_id: None },
            },
        ];
        let resolved2 = save_config(&dir, true, &deleted_and_reused, "c").unwrap();
        assert_eq!(
            resolved2, LOCAL_ID,
            "deleting active c and adding a fresh c must fall back to local"
        );
        let loaded2 = parse(&std::fs::read_to_string(hosts_path(&dir)).unwrap(), &no_env);
        assert_eq!(loaded2.active, LOCAL_ID);
        assert_eq!(
            loaded2.find("c").unwrap().password,
            None,
            "brand-new host reusing deleted id must not inherit deleted host's password"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
