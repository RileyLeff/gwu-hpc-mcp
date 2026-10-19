//! Structured create-only text transfer; never relax shell command policies.
use crate::{
    errors::{AppError, AppResult},
    ssh::ExecRequest,
    util::escape_for_shell,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};

const RECEIVER: &str = include_str!("upload_receiver.py");
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TextFile {
    /// Flat filename; no slashes, traversal or shell syntax.
    pub name: String,
    pub content: String,
    /// Required lowercase SHA-256 of the exact UTF-8 content.
    pub sha256: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadTextBundleParams {
    pub cluster: Option<String>,
    /// New directory relative to configured default_cwd. Parent must exist.
    pub directory: String,
    /// Up to 128 files and 8 MiB of serialized JSON. Created mode 0600.
    pub files: Vec<TextFile>,
}

fn component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

pub fn prepare(root: &str, params: &UploadTextBundleParams) -> AppResult<(ExecRequest, Vec<u8>)> {
    let invalid = |s: &str| AppError::InvalidInput(s.into());
    if !root.starts_with('/') || root.trim_matches('/').split('/').count() < 3 {
        return Err(invalid(
            "upload requires configured absolute bounded project default_cwd",
        ));
    }
    let parts: Vec<_> = params.directory.split('/').collect();
    if parts.len() > 12 || !parts.iter().all(|p| component(p)) {
        return Err(invalid(
            "upload directory must be a safe project-relative path",
        ));
    }
    if params.files.is_empty() || params.files.len() > 128 {
        return Err(invalid("upload requires 1..128 files"));
    }
    let mut names = std::collections::HashSet::new();
    for file in &params.files {
        if !component(&file.name) || !names.insert(&file.name) {
            return Err(invalid("invalid or duplicate flat filename"));
        }
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(invalid("invalid content SHA-256"));
        }
    }
    let input = serde_json::to_vec(&serde_json::json!({
        "root": root, "directory": params.directory, "files": params.files,
    }))
    .map_err(|e| invalid(&format!("upload serialization: {e}")))?;
    if input.len() > MAX_BYTES {
        return Err(invalid("upload exceeds 8 MiB"));
    }
    Ok((
        ExecRequest {
            // Command is fixed: NONE of the caller's paths, names or contents enter it.
            command: format!("python3 -I -c {}", escape_for_shell(RECEIVER)),
            cwd: None,
            env: HashMap::new(),
            pty: false,
            login_shell: false,
            timeout: Some(Duration::from_secs(30)),
        },
        input,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn params() -> UploadTextBundleParams {
        UploadTextBundleParams {
            cluster: None,
            directory: "experiments/safe-v1".into(),
            files: vec![TextFile {
                name: "auth.py".into(),
                content: "shutdown; $(reboot); `mkfs`; rm -rf /".into(),
                sha256: "a".repeat(64),
            }],
        }
    }
    #[test]
    fn contents_never_enter_remote_command() {
        let p = params();
        let (req, input) = prepare("/scratch/group/user/project", &p).unwrap();
        assert!(!req.command.contains(&p.files[0].content));
        assert!(String::from_utf8(input).unwrap().contains("shutdown"));
        assert!(!req.pty && !req.login_shell);
        assert_eq!(req.timeout, Some(Duration::from_secs(30)));
    }
    #[test]
    fn rejects_unsafe_paths_names_hashes_and_size() {
        for s in [
            "",
            "/tmp/new",
            "../new",
            "a/../new",
            "a//b",
            "a/./b",
            "x;reboot",
            "$(reboot)",
            "x\nnew",
            "-option",
        ] {
            let mut p = params();
            p.directory = s.into();
            assert!(prepare("/scratch/g/u/p", &p).is_err(), "{s}");
        }
        for s in [".", "..", "a/b", "x;reboot", ".ssh", "$(reboot)"] {
            let mut p = params();
            p.files[0].name = s.into();
            assert!(prepare("/scratch/g/u/p", &p).is_err());
        }
        let mut p = params();
        p.files[0].sha256 = "Z".repeat(64);
        assert!(prepare("/scratch/g/u/p", &p).is_err());
        let mut p = params();
        p.files[0].content = "a".repeat(MAX_BYTES);
        assert!(prepare("/scratch/g/u/p", &p).is_err());
        assert!(prepare("/", &params()).is_err());
    }
}
