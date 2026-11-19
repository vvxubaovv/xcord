//! Portable session import/export: Xcord YAML and Xshell `.xsh` files.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn default_port() -> u16 {
    22
}

fn default_auth() -> String {
    "password".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PortableProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default = "default_auth")]
    pub auth: String,
    #[serde(default)]
    pub key_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub login_script: String,
    #[serde(default)]
    pub startup_command: Option<String>,
    #[serde(default)]
    pub term: String,
    #[serde(default)]
    pub keepalive_secs: u64,
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum YamlDocument {
    List(Vec<PortableProfile>),
    Wrapped {
        #[serde(default)]
        version: u32,
        sessions: Vec<PortableProfile>,
    },
}

pub fn export_yaml(profiles: &[PortableProfile]) -> Result<String, String> {
    serde_yaml::to_string(profiles).map_err(|error| error.to_string())
}

pub fn import_yaml(text: &str) -> Result<Vec<PortableProfile>, String> {
    let document: YamlDocument =
        serde_yaml::from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    let profiles = match document {
        YamlDocument::List(profiles) => profiles,
        YamlDocument::Wrapped { version, sessions } => {
            let _ = version;
            sessions
        }
    };
    Ok(profiles)
}

pub struct XshellReport {
    pub profiles: Vec<PortableProfile>,
    pub warnings: Vec<String>,
}

pub fn import_xshell(path: &Path) -> Result<XshellReport, String> {
    let mut files = Vec::new();
    if path.is_file() {
        files.push(path.to_path_buf());
    } else if path.is_dir() {
        collect_xsh_files(path, &mut files)?;
    } else {
        return Err("Xshell path does not exist.".into());
    }
    if files.is_empty() {
        return Err("No .xsh session files found.".into());
    }
    let root = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    let sid = current_sid();
    let mut profiles = Vec::new();
    let mut warnings = Vec::new();
    for file in files {
        match parse_xsh_file(&file, &root, sid.as_deref()) {
            Ok(profile) => {
                if profile.host.trim().is_empty() {
                    warnings.push(format!("Skipped {}: no host.", file.display()));
                } else {
                    profiles.push(profile);
                }
            }
            Err(error) => warnings.push(format!("Skipped {}: {error}", file.display())),
        }
    }
    Ok(XshellReport { profiles, warnings })
}

fn collect_xsh_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            collect_xsh_files(&path, files)?;
        } else if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("xsh"))
        {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_xsh_file(path: &Path, root: &Path, sid: Option<&str>) -> Result<PortableProfile, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let ini = parse_ini(&text);
    let find = |keys: &[&str]| -> Option<String> {
        for section in ["connection", "connection:authentication", "connection:ssh"] {
            if let Some(values) = ini.get(section) {
                for key in keys {
                    if let Some(value) = values.get(*key) {
                        if !value.trim().is_empty() {
                            return Some(value.trim().to_string());
                        }
                    }
                }
            }
        }
        for values in ini.values() {
            for key in keys {
                if let Some(value) = values.get(*key) {
                    if !value.trim().is_empty() {
                        return Some(value.trim().to_string());
                    }
                }
            }
        }
        None
    };
    let protocol = find(&["protocol"]).unwrap_or_else(|| "SSH".into());
    if !protocol.eq_ignore_ascii_case("ssh") {
        return Err(format!("protocol {protocol} is not SSH"));
    }
    let host = find(&["host", "hostname"]).unwrap_or_default();
    let port = find(&["port"])
        .and_then(|value| value.parse().ok())
        .unwrap_or(22);
    let user = find(&["username", "user"]).unwrap_or_default();
    let auth_raw = find(&["userauthentication", "authentication", "method"])
        .unwrap_or_else(|| "password".into())
        .to_lowercase();
    let key_path = find(&["publickey", "publickeyfile", "userkey", "keyfile"]).unwrap_or_default();
    let auth = if auth_raw.contains("public") || auth_raw.contains("key") {
        "public_key".to_string()
    } else if auth_raw.contains("agent") {
        "agent".to_string()
    } else if auth_raw.contains("none") {
        "none".to_string()
    } else {
        "password".to_string()
    };
    let password = find(&["password"]).and_then(|encoded| {
        decrypt_xshell_password(&encoded, sid)
            .or_else(|| (!encoded.contains(' ')).then_some(encoded))
    });
    let passphrase = find(&["passphrase", "publickeypassphrase"]);
    let group = path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .map(|relative| {
            let value = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .filter(|component| !component.is_empty())
                .collect::<Vec<_>>()
                .join("/");
            if value.is_empty() {
                "Xshell".to_string()
            } else {
                format!("Xshell/{value}")
            }
        })
        .unwrap_or_else(|| "Xshell".to_string());
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Xshell session")
        .to_string();
    Ok(PortableProfile {
        name,
        group,
        host,
        port,
        user,
        auth,
        key_path,
        password,
        passphrase,
        login_script: String::new(),
        startup_command: None,
        term: "xterm-256color".into(),
        keepalive_secs: 30,
        logging: false,
        tags: Vec::new(),
    })
}

fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current = String::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].trim().to_lowercase();
            sections.entry(current.clone()).or_default();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            sections
                .entry(current.clone())
                .or_default()
                .insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }
    sections
}

fn current_sid() -> Option<String> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&output.stdout);
        let field = line.split(',').nth(1)?.trim().trim_matches('"');
        (!field.is_empty()).then(|| field.to_string())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn decrypt_xshell_password(encoded: &str, sid: Option<&str>) -> Option<String> {
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return None;
    }
    let sid = sid?;
    let ciphertext = decode_base64(encoded)?;
    let key = Sha256::digest(sid.as_bytes());
    let plain = rc4(&key, &ciphertext);
    let text = String::from_utf8(plain).ok()?;
    if text.is_empty() || text.chars().any(|ch| ch.is_control()) {
        return None;
    }
    Some(text)
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let engine = base64::engine::general_purpose::STANDARD;
    engine
        .decode(value)
        .or_else(|_| engine.decode(value.trim_end_matches('=')))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
        .ok()
}

fn rc4(key: &[u8], input: &[u8]) -> Vec<u8> {
    let mut state = [0u8; 256];
    for (index, value) in state.iter_mut().enumerate() {
        *value = index as u8;
    }
    let mut j = 0usize;
    for index in 0..256 {
        j = (j + state[index] as usize + key[index % key.len()] as usize) & 0xff;
        state.swap(index, j);
    }
    let mut i = 0usize;
    let mut j = 0usize;
    let mut output = Vec::with_capacity(input.len());
    for byte in input {
        i = (i + 1) & 0xff;
        j = (j + state[i] as usize) & 0xff;
        state.swap(i, j);
        let k = state[(state[i] as usize + state[j] as usize) & 0xff];
        output.push(byte ^ k);
    }
    output
}

fn encrypt_xshell_password(plain: &str, sid: &str) -> Option<String> {
    let key = Sha256::digest(sid.as_bytes());
    let cipher = rc4(&key, plain.as_bytes());
    Some(base64::engine::general_purpose::STANDARD.encode(cipher))
}

pub struct XshellExportReport {
    pub count: usize,
    pub warnings: Vec<String>,
}

pub fn export_xshell(
    directory: &Path,
    profiles: &[PortableProfile],
) -> Result<XshellExportReport, String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let sid = current_sid();
    let mut count = 0usize;
    let mut warnings = Vec::new();
    for profile in profiles {
        let group_parts = profile
            .group
            .split('/')
            .filter(|part| !part.trim().is_empty() && *part != "Sessions")
            .map(sanitize_component)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let mut target = directory.to_path_buf();
        for part in group_parts {
            target.push(part);
        }
        fs::create_dir_all(&target).map_err(|error| error.to_string())?;
        let stem = sanitize_component(&profile.name);
        let stem = if stem.is_empty() {
            "session".to_string()
        } else {
            stem
        };
        let mut path = target.join(format!("{stem}.xsh"));
        let mut suffix = 2;
        while path.exists() {
            path = target.join(format!("{stem} ({suffix}).xsh"));
            suffix += 1;
        }
        let mut lines = vec![
            "[CONNECTION]".to_string(),
            "Protocol=SSH".to_string(),
            format!("Host={}", profile.host),
            format!("Port={}", if profile.port == 0 { 22 } else { profile.port }),
            "Version=6.0".to_string(),
            "[CONNECTION:AUTHENTICATION]".to_string(),
            format!("UserName={}", profile.user),
        ];
        if profile.auth == "public_key" && !profile.key_path.is_empty() {
            lines.push("UserAuthentication=PublicKey".into());
            lines.push(format!("PublicKey={}", profile.key_path));
        } else {
            lines.push("UserAuthentication=Password".into());
        }
        if let Some(password) = &profile.password {
            if let Some(sid) = sid.as_deref() {
                if let Some(encrypted) = encrypt_xshell_password(password, sid) {
                    lines.push(format!("Password={encrypted}"));
                }
            } else {
                warnings.push(format!(
                    "{}: exported without password (could not read Windows SID).",
                    profile.name
                ));
            }
        }
        lines.push(String::new());
        fs::write(&path, lines.join("\r\n")).map_err(|error| error.to_string())?;
        count += 1;
    }
    Ok(XshellExportReport { count, warnings })
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_roundtrip() {
        let profiles = vec![PortableProfile {
            name: "Prod".into(),
            group: "Servers/Prod".into(),
            host: "example.com".into(),
            user: "root".into(),
            auth: "password".into(),
            password: Some("secret".into()),
            ..PortableProfile::default()
        }];
        let yaml = export_yaml(&profiles).unwrap();
        let parsed = import_yaml(&yaml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "Prod");
        assert_eq!(parsed[0].group, "Servers/Prod");
        assert_eq!(parsed[0].password.as_deref(), Some("secret"));
    }

    #[test]
    fn xshell_password_roundtrip() {
        let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        let encrypted = encrypt_xshell_password("p@ssw0rd", sid).unwrap();
        let decrypted = decrypt_xshell_password(&encrypted, Some(sid)).unwrap();
        assert_eq!(decrypted, "p@ssw0rd");
    }

    #[test]
    fn imports_plain_xsh() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("xcord-xsh-test-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("demo.xsh");
        std::fs::write(
            &file,
            "[CONNECTION]\nProtocol=SSH\nHost=127.0.0.1\nPort=2222\n\
             [CONNECTION:AUTHENTICATION]\nUserName=root\nPassword=plain-secret\n",
        )
        .unwrap();
        let report = import_xshell(&dir).unwrap();
        assert_eq!(report.profiles.len(), 1);
        assert_eq!(report.profiles[0].host, "127.0.0.1");
        assert_eq!(report.profiles[0].port, 2222);
        assert_eq!(report.profiles[0].user, "root");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
