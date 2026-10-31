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

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::log_dbg;

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

    let tmp = dir.join(format!("{HOSTS_FILE}.tmp"));
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
}
