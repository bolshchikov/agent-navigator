use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cookie_store::CookieStore;
use reqwest_cookie_store::CookieStoreMutex;

use crate::config::ClientConfig;
use crate::error::{Error, Result};

pub struct SessionStore {
    dir: PathBuf,
    sessions: Mutex<HashMap<String, Session>>,
}

#[derive(Clone)]
pub struct Session {
    pub name: String,
    pub http: reqwest::Client,
    cookies: Arc<CookieStoreMutex>,
    persist_path: PathBuf,
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn get_or_create(&self, name: &str, cfg: &ClientConfig) -> Result<Session> {
        validate_session_name(name)?;
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| Error::Session("session map poisoned".into()))?;
        if let Some(existing) = guard.get(name) {
            return Ok(existing.clone());
        }
        let session = Session::open(name, &self.dir, cfg)?;
        guard.insert(name.to_string(), session.clone());
        Ok(session)
    }

    pub fn persist(&self, session: &Session) -> Result<()> {
        session.save()
    }
}

impl Session {
    fn open(name: &str, dir: &Path, cfg: &ClientConfig) -> Result<Self> {
        let persist_path = dir.join(format!("{name}.json"));
        let store = load_cookie_store(&persist_path);
        let cookies = Arc::new(CookieStoreMutex::new(store));
        let http = reqwest::Client::builder()
            .user_agent(&cfg.user_agent)
            .timeout(cfg.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .cookie_provider(cookies.clone())
            .build()?;
        Ok(Self {
            name: name.to_string(),
            http,
            cookies,
            persist_path,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.persist_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_cookie_file(&self.persist_path)?;
        let mut writer = BufWriter::new(file);
        let guard = self
            .cookies
            .lock()
            .map_err(|_| Error::Session("cookie store poisoned".into()))?;
        save_cookie_store(&guard, &mut writer)?;
        Ok(())
    }
}

fn load_cookie_store(path: &Path) -> CookieStore {
    let Ok(file) = fs::File::open(path) else {
        return CookieStore::default();
    };
    let reader = BufReader::new(file);
    cookie_store::serde::json::load(reader).unwrap_or_default()
}

fn save_cookie_store(store: &CookieStore, writer: &mut impl std::io::Write) -> Result<()> {
    cookie_store::serde::json::save(store, writer)
        .map_err(|e| Error::Session(format!("failed to persist cookies: {e}")))
}

pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(Error::Session(
            "session name must be 1–64 characters".into(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Session(
            "session name may only contain ASCII letters, digits, '-' and '_'".into(),
        ));
    }
    Ok(())
}

/// Cookie-jar name for a public HTTP MCP tenant. Keeps `[A-Za-z0-9_-]{1,64}`.
pub fn namespaced_session(mcp_session_id: &str, jar: &str) -> String {
    let ns = sanitize_session_fragment(mcp_session_id, 32);
    let jar = sanitize_session_fragment(jar, 31);
    format!("{ns}_{jar}")
}

fn sanitize_session_fragment(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(max)
        .collect();
    if cleaned.is_empty() {
        "anon".into()
    } else {
        cleaned
    }
}

fn open_cookie_file(path: &Path) -> Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?)
    }
    #[cfg(not(unix))]
    {
        Ok(fs::File::create(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_rejects_path_traversal() {
        assert!(validate_session_name("../evil").is_err());
        assert!(validate_session_name("foo/bar").is_err());
        assert!(validate_session_name("foo.json").is_err());
        assert!(validate_session_name("default").is_ok());
        assert!(validate_session_name("agent-1_prod").is_ok());
    }

    #[test]
    fn namespaced_session_stays_within_rules() {
        let name = namespaced_session("550e8400-e29b-41d4-a716-446655440000", "default");
        assert!(validate_session_name(&name).is_ok(), "{name}");
        assert_ne!(
            namespaced_session("session-a", "default"),
            namespaced_session("session-b", "default")
        );
        let long = namespaced_session(&"x".repeat(80), &"y".repeat(80));
        assert!(long.len() <= 64);
        assert!(validate_session_name(&long).is_ok());
    }
}
