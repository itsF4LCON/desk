//! `config.toml` (non-secret settings) and the private `state/` directory.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Must be loopback: the only way in from outside is the Cloudflare Tunnel.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Exact browser origin, e.g. `https://desk.example.com`. Checked on every POST and WebSocket.
    pub public_origin: String,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_web_dir")]
    pub web_dir: PathBuf,
    /// H.264 encoder: `auto` (probe NVENC → VA-API → x264), `nvenc`, `vaapi` or `x264`.
    #[serde(default)]
    pub encoder: crate::capture::EncoderChoice,
    /// Local UDP port for media. 0 = random (fine for most NATs); set a fixed port if you want
    /// to forward it on your router for guaranteed direct connections.
    #[serde(default)]
    pub udp_port: u16,
    pub access: AccessConfig,
    pub turn: Option<TurnConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccessConfig {
    /// Zero Trust team domain, e.g. `yourteam.cloudflareaccess.com`.
    pub team_domain: String,
    /// "Application Audience (AUD) Tag" of the Access application.
    pub aud: String,
    /// The only identity allowed through.
    pub allowed_email: String,
}

/// Cloudflare Realtime TURN key (optional; used by the browser when a direct path fails).
#[derive(Debug, Clone, Deserialize)]
pub struct TurnConfig {
    pub key_id: String,
    pub api_token: String,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8750".parse().unwrap()
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("state")
}
fn default_web_dir() -> PathBuf {
    PathBuf::from("../web/dist")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        if !cfg.listen.ip().is_loopback() {
            bail!("listen must be a loopback address; expose it through cloudflared instead");
        }
        if !cfg.public_origin.starts_with("https://")
            || cfg.public_origin.trim_end_matches('/').matches('/').count() != 2
        {
            bail!("public_origin must be a bare https:// origin, e.g. https://desk.example.com");
        }
        Ok(cfg)
    }

    /// Host part of `public_origin`, e.g. `desk.example.com`.
    pub fn public_host(&self) -> &str {
        self.public_origin
            .trim_start_matches("https://")
            .trim_end_matches('/')
    }
}

/// Secrets written by `desk-agent setup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthState {
    pub password_hash: String,
    pub totp_secret: String,
    /// Last accepted TOTP time step; codes at or before it are rejected (replay protection).
    #[serde(default)]
    pub last_totp_step: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortalState {
    /// Monitor name → portal restore token.
    pub tokens: HashMap<String, String>,
}

pub struct StateDir(PathBuf);

impl StateDir {
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path.to_path_buf()))
    }

    pub fn read<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Result<Option<T>> {
        let path = self.0.join(name);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("corrupt {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic write with mode 0600.
    pub fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let tmp = self.0.join(format!(".{name}.tmp"));
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, self.0.join(name))?;
        Ok(())
    }
}
