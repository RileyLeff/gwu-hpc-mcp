//! OpenSSH-compatible known_hosts checks for the built-in SSH client.
//!
//! Shares ~/.ssh/known_hosts with OpenSSH (including Windows OpenSSH), so a
//! host already trusted by `ssh` is trusted here and vice versa.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{HashAlg, PublicKey};
use sha1::Sha1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Trusted,
    Unknown,
    /// A different key of the same type is recorded: possible man-in-the-middle.
    Changed {
        path: PathBuf,
        line: usize,
    },
    Revoked {
        path: PathBuf,
        line: usize,
    },
}

/// The host token OpenSSH writes: `host` for port 22, `[host]:port` otherwise.
pub fn host_token(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

pub fn check(path: &Path, host: &str, port: u16, key: &PublicKey) -> io::Result<HostKeyStatus> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HostKeyStatus::Unknown),
        Err(e) => return Err(e),
    };
    let token = host_token(host, port);
    let mut trusted = false;
    let mut changed = None;
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let mut fields = raw.split_whitespace();
        let Some(mut first) = fields.next() else {
            continue;
        };
        if first.starts_with('#') {
            continue;
        }
        let marker = if first.starts_with('@') {
            let marker = first;
            match fields.next() {
                Some(next) => first = next,
                None => continue,
            }
            Some(marker)
        } else {
            None
        };
        let (Some(kind), Some(data)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !hosts_match(first, &token) {
            continue;
        }
        // Unsupported or malformed entries are skipped, as OpenSSH does.
        let Ok(recorded) = PublicKey::from_openssh(&format!("{kind} {data}")) else {
            continue;
        };
        let same_key = recorded.key_data() == key.key_data();
        match marker {
            Some("@revoked") if same_key => {
                return Ok(HostKeyStatus::Revoked {
                    path: path.to_path_buf(),
                    line,
                });
            }
            Some(_) => {}
            None if same_key => trusted = true,
            None if recorded.algorithm() == key.algorithm() => {
                changed.get_or_insert(line);
            }
            None => {}
        }
    }
    Ok(match (trusted, changed) {
        (true, _) => HostKeyStatus::Trusted,
        (false, Some(line)) => HostKeyStatus::Changed {
            path: path.to_path_buf(),
            line,
        },
        (false, None) => HostKeyStatus::Unknown,
    })
}

pub fn learn(path: &Path, host: &str, port: u16, key: &PublicKey) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut key = key.clone();
    key.set_comment("");
    let encoded = key
        .to_openssh()
        .map_err(|e| io::Error::other(format!("cannot encode host key: {e}")))?;
    let needs_newline = fs::read(path)
        .map(|bytes| bytes.last().is_some_and(|b| *b != b'\n'))
        .unwrap_or(false);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if needs_newline {
        file.write_all(b"\n")?;
    }
    writeln!(file, "{} {}", host_token(host, port), encoded.trim_end())
}

fn hosts_match(field: &str, token: &str) -> bool {
    if let Some(hashed) = field.strip_prefix("|1|") {
        return hashed_match(hashed, token);
    }
    let mut matched = false;
    for pattern in field.split(',') {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_match(negated, token) {
                return false;
            }
        } else if glob_match(pattern, token) {
            matched = true;
        }
    }
    matched
}

fn hashed_match(hashed: &str, token: &str) -> bool {
    let Some((salt, hash)) = hashed.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (BASE64.decode(salt), BASE64.decode(hash)) else {
        return false;
    };
    let Ok(mut mac) = <Hmac<Sha1> as KeyInit>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(token.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// OpenSSH host pattern matching with `*` and `?` wildcards (case-insensitive).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    const ED_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB7VaVL5ZbgYlx5HSmtpVNNyVJDH5mCFJ86oE1wDXYzN";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn file(contents: &str) -> (tempdir::Dir, PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("known_hosts");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn trusted_unknown_changed_and_revoked() {
        let (_d, path) = file(&format!(
            "# comment\n\nother.example {ED_B}\npegasus.arc.gwu.edu,1.2.3.4 {ED_A}\n"
        ));
        let check = |h: &str, k: &str| check(&path, h, 22, &key(k)).unwrap();
        assert_eq!(check("pegasus.arc.gwu.edu", ED_A), HostKeyStatus::Trusted);
        assert_eq!(check("1.2.3.4", ED_A), HostKeyStatus::Trusted);
        assert_eq!(check("new.example", ED_A), HostKeyStatus::Unknown);
        assert!(matches!(
            check("pegasus.arc.gwu.edu", ED_B),
            HostKeyStatus::Changed { line: 4, .. }
        ));

        let (_d, revoked) = file(&format!("@revoked * {ED_A}\nhost {ED_A}\n"));
        assert!(matches!(
            super::check(&revoked, "host", 22, &key(ED_A)).unwrap(),
            HostKeyStatus::Revoked { line: 1, .. }
        ));
    }

    #[test]
    fn missing_file_is_unknown_and_learn_round_trips() {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("sub").join("known_hosts");
        assert_eq!(
            check(&path, "h", 2222, &key(ED_A)).unwrap(),
            HostKeyStatus::Unknown
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x.example ssh-ed25519 garbage").unwrap();
        learn(&path, "h", 2222, &key(ED_A)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\n[h]:2222 ssh-ed25519 "), "{text}");
        assert_eq!(
            check(&path, "h", 2222, &key(ED_A)).unwrap(),
            HostKeyStatus::Trusted
        );
        assert_eq!(
            check(&path, "h", 22, &key(ED_A)).unwrap(),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn hashed_wildcard_and_negated_hosts() {
        // Generated with `ssh-keygen -H` semantics: HMAC-SHA1(salt, host).
        let salt = [7u8; 20];
        let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&salt).unwrap();
        mac.update(b"pegasus.arc.gwu.edu");
        let hash = mac.finalize().into_bytes();
        let hashed = format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(hash));
        assert!(hosts_match(&hashed, "pegasus.arc.gwu.edu"));
        assert!(!hosts_match(&hashed, "other.gwu.edu"));
        assert!(hosts_match("*.gwu.edu", "pegasus.arc.gwu.edu"));
        assert!(hosts_match("PEGASUS.arc.gwu.edu", "pegasus.arc.gwu.edu"));
        assert!(!hosts_match("*.gwu.edu,!pegasus*", "pegasus.arc.gwu.edu"));
        assert!(hosts_match("p?gasus", "pegasus"));
        assert!(!hosts_match("pegasus", "pegasus2"));
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!("ghm-kh-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
