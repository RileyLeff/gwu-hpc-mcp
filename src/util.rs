use std::borrow::Cow;
use std::collections::VecDeque;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::errors::{AppError, AppResult};

/// The local user's home directory ($HOME on Unix, the user profile on Windows).
pub fn home_dir() -> AppResult<PathBuf> {
    env::home_dir()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| AppError::Io("cannot determine home directory".into()))
}

pub fn expand_path(input: &str) -> AppResult<PathBuf> {
    if input == "~" {
        return home_dir();
    }
    match input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        Some(rest) => Ok(home_dir()?.join(Path::new(rest))),
        None => Ok(PathBuf::from(input)),
    }
}

pub fn join_user_host(user: &str, host: &str) -> String {
    format!("{user}@{host}")
}

/// Quote for the cluster's POSIX shell. Always use POSIX rules:
/// `shell_escape::escape` switches to cmd.exe quoting on Windows, which the
/// remote shell would misread.
pub fn escape_for_shell(value: &str) -> String {
    shell_escape::unix::escape(Cow::Borrowed(value)).to_string()
}

pub fn format_duration(duration: Duration) -> String {
    humantime::format_duration(duration).to_string()
}

pub struct TailBuffer {
    data: VecDeque<u8>,
    capacity: usize,
}

impl TailBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.data.len() == self.capacity {
                self.data.pop_front();
            }
            self.data.push_back(b);
        }
    }

    pub fn to_lossy_string(&self) -> String {
        let (first, second) = self.data.as_slices();
        let mut out = String::with_capacity(first.len() + second.len());
        out.push_str(&String::from_utf8_lossy(first));
        out.push_str(&String::from_utf8_lossy(second));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{TailBuffer, escape_for_shell, expand_path, validate_env};

    #[test]
    fn quotes_for_the_remote_posix_shell_on_every_platform() {
        assert_eq!(escape_for_shell("plain"), "plain");
        assert_eq!(escape_for_shell("a b"), "'a b'");
        assert_eq!(escape_for_shell("it's $HOME"), r"'it'\''s $HOME'");
    }

    #[test]
    fn validate_environment_names_and_bare_home() {
        assert!(validate_env(&[("GOOD_VAR".into(), "a b".into())].into()).is_ok());
        for key in ["", "1BAD", "BAD;echo", "A=B"] {
            assert!(validate_env(&[(key.into(), "x".into())].into()).is_err());
        }
        let home = std::env::home_dir().unwrap();
        assert_eq!(expand_path("~").unwrap(), home);
        assert_eq!(expand_path("~/a/b").unwrap(), home.join("a").join("b"));
        assert_eq!(
            expand_path("/abs").unwrap(),
            std::path::PathBuf::from("/abs")
        );
    }

    #[test]
    fn tail_buffer_keeps_recent_bytes() {
        let mut buf = TailBuffer::new(4);
        buf.push(b"abcd");
        buf.push(b"ef");
        assert_eq!(buf.to_lossy_string(), "cdef");
    }
}

pub fn validate_env(env: &std::collections::HashMap<String, String>) -> AppResult<()> {
    for key in env.keys() {
        let mut chars = key.chars();
        if !chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(AppError::InvalidInput(format!(
                "Invalid environment variable name: {key}"
            )));
        }
    }
    Ok(())
}
