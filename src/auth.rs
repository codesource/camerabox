//! Authentication: persisted credentials + in-memory sessions.
//!
//! Credentials live in `/var/lib/camera-box/auth.toml` as a salted SHA-256
//! hash (default `admin` / `password` on first run). Login issues a random
//! session token held in memory and set as an HttpOnly cookie; the web layer
//! gates routes on a valid session. The password can be changed from the web
//! UI or reset from the CLI (`camera-box reset-password [user] [pass]`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

#[derive(Clone, Serialize, Deserialize)]
struct Credentials {
    username: String,
    salt: String,
    hash: String,
}

pub struct Auth {
    path: PathBuf,
    creds: RwLock<Credentials>,
    sessions: Mutex<HashMap<String, Instant>>,
}

impl Auth {
    /// Load credentials, creating the default `admin`/`password` on first run.
    pub fn load(path: PathBuf) -> Self {
        let creds = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<Credentials>(&s).ok())
            .unwrap_or_else(|| {
                let c = make_credentials("admin", "password");
                let _ = save(&path, &c);
                info!("initialised default credentials (admin / password)");
                c
            });
        Auth {
            path,
            creds: RwLock::new(creds),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn username(&self) -> String {
        self.creds.read().unwrap().username.clone()
    }

    fn verify(&self, user: &str, pass: &str) -> bool {
        let c = self.creds.read().unwrap();
        if c.username != user {
            return false;
        }
        match hex_decode(&c.salt) {
            Some(salt) => hash_password(&salt, pass) == c.hash,
            None => false,
        }
    }

    /// Verify credentials and start a session, returning its token.
    pub fn login(&self, user: &str, pass: &str) -> Option<String> {
        if !self.verify(user, pass) {
            return None;
        }
        let token = random_hex(16);
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Instant::now());
        Some(token)
    }

    /// Is this session token valid (and not expired)?
    pub fn validate(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, t| t.elapsed() < SESSION_TTL);
        s.contains_key(token)
    }

    pub fn logout(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    /// Change credentials (and invalidate all sessions).
    pub fn set_credentials(&self, user: &str, pass: &str) -> Result<(), String> {
        if user.is_empty() {
            return Err("username is required".into());
        }
        if pass.len() < 4 {
            return Err("password must be at least 4 characters".into());
        }
        let c = make_credentials(user, pass);
        save(&self.path, &c)?;
        *self.creds.write().unwrap() = c;
        self.sessions.lock().unwrap().clear();
        info!(username = user, "credentials updated");
        Ok(())
    }
}

fn make_credentials(user: &str, pass: &str) -> Credentials {
    let salt = random_bytes(16);
    Credentials {
        username: user.to_string(),
        salt: hex_encode(&salt),
        hash: hash_password(&salt, pass),
    }
}

fn save(path: &PathBuf, c: &Credentials) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = toml::to_string(c).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

fn hash_password(salt: &[u8], password: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password.as_bytes());
    hex_encode(&h.finalize())
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

fn random_hex(n: usize) -> String {
    hex_encode(&random_bytes(n))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
