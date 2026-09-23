mod audio;
mod auth;
mod capture;
mod config;
mod desktop;
mod http;
mod input;
mod rtc;
mod stun;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};

const USAGE: &str = "\
desk-agent — browser remote desktop for Linux (Wayland)

USAGE:
    desk-agent serve  [--config PATH] [--insecure-local]
    desk-agent setup  [--config PATH] [--password-stdin] [--qr-png FILE]
    desk-agent doctor [--config PATH]

COMMANDS:
    serve    Run the agent (normally started by the systemd user service).
    setup    Set the login password and create a new TOTP secret (prints a QR code).
    doctor   Check encoder, screen capture, input and desktop integration.

--config defaults to $XDG_CONFIG_HOME/desk/config.toml (~/.config/desk/config.toml).
--insecure-local disables Cloudflare Access checks and only accepts http://localhost; for testing.";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "desk_agent=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let config_path = value("--config")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);

    match args.first().map(String::as_str) {
        Some("serve") => serve(&config_path, flag("--insecure-local")).await,
        Some("setup") => setup(&config_path, flag("--password-stdin"), value("--qr-png")),
        Some("doctor") => doctor(&config_path).await,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("desk").join("config.toml")
}

async fn serve(config_path: &Path, insecure_local: bool) -> Result<()> {
    let cfg = config::Config::load(config_path)?;
    let state_dir = config::StateDir::open(&cfg.state_dir)?;
    let auth_state: config::AuthState = state_dir
        .read("auth.json")?
        .ok_or_else(|| anyhow!("no credentials yet: run `desk-agent setup` first"))?;
    let portal_state: config::PortalState = state_dir.read("portal.json")?.unwrap_or_default();

    let encoder = capture::Encoder::select(cfg.encoder)?;
    tracing::info!(encoder = encoder.name(), "video encoder selected");

    // In --insecure-local mode Cloudflare Access is skipped, so only allow the plain loopback origin.
    let (access, origin) = if insecure_local {
        tracing::warn!(
            "--insecure-local: Cloudflare Access verification is DISABLED (loopback testing only)"
        );
        (None, format!("http://localhost:{}", cfg.listen.port()))
    } else {
        (
            Some(auth::AccessVerifier::new(cfg.access.clone())),
            cfg.public_origin.clone(),
        )
    };

    let state = Arc::new(http::AppState {
        access,
        origin,
        encoder,
        sessions: auth::Sessions::default(),
        auth_state: tokio::sync::Mutex::new(auth_state),
        portal: Arc::new(rtc::PortalStore {
            state: tokio::sync::Mutex::new(portal_state),
            dir: config::StateDir::open(&cfg.state_dir)?,
        }),
        state_dir,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?,
        active: tokio::sync::Mutex::new(None),
        next_session_id: Default::default(),
        cfg,
    });
    http::serve(state).await
}

/// Creates the password hash and a fresh TOTP secret (replacing any previous ones).
fn setup(config_path: &Path, password_stdin: bool, qr_png: Option<String>) -> Result<()> {
    let cfg = config::Config::load(config_path)?;
    let dir = config::StateDir::open(&cfg.state_dir)?;

    let password = if password_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_string()
    } else {
        let a = rpassword::prompt_password("New desk password: ")?;
        let b = rpassword::prompt_password("Repeat password: ")?;
        ensure!(a == b, "passwords do not match");
        a
    };
    ensure!(password.chars().count() >= 12, "use at least 12 characters");

    let secret = auth::new_totp_secret();
    let totp = auth::totp(&secret, cfg.public_host(), &cfg.access.allowed_email)?;
    let state = config::AuthState {
        password_hash: auth::hash_password(&password)?,
        totp_secret: secret,
        last_totp_step: 0,
    };

    let url = totp
        .to_url()
        .map_err(|e| anyhow!("TOTP URL failed: {e:?}"))?;
    if let Some(path) = qr_png {
        let png = totp
            .to_qr_png()
            .map_err(|e| anyhow!("QR generation failed: {e:?}"))?;
        std::fs::write(&path, png)?;
        println!("TOTP QR code written to {path}");
    } else {
        let code = qrcode::QrCode::new(url.as_bytes())?;
        println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build()
        );
    }
    dir.write("auth.json", &state)?;
    println!(
        "Scan the QR code with an authenticator app (issuer {}).",
        cfg.public_host()
    );
    println!("Credentials saved to {}/auth.json", cfg.state_dir.display());
    Ok(())
}

/// Checks everything the agent needs from the local system and prints a report.
async fn doctor(config_path: &Path) -> Result<()> {
    let mut failures = 0;
    let mut report = |ok: bool, what: &str, detail: String| {
        println!(
            "  {} {what}{}",
            if ok { "✓" } else { "✗" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        );
        if !ok {
            failures += 1;
        }
    };
    println!("desk doctor\n");

    let cfg = config::Config::load(config_path);
    match &cfg {
        Ok(c) => report(
            true,
            "config",
            format!("{} ({})", config_path.display(), c.public_origin),
        ),
        Err(e) => report(false, "config", format!("{e:#}")),
    }

    gstreamer::init().context("GStreamer failed to initialise")?;
    for el in [
        "pipewiresrc",
        "pulsesrc",
        "opusenc",
        "h264parse",
        "appsink",
        "videoconvert",
    ] {
        let ok = gstreamer::ElementFactory::find(el).is_some();
        report(
            ok,
            &format!("GStreamer element {el}"),
            if ok {
                String::new()
            } else {
                "missing plugin".into()
            },
        );
    }
    let choice = cfg.as_ref().map(|c| c.encoder).unwrap_or_default();
    match capture::Encoder::select(choice) {
        Ok(enc) => report(true, "H.264 encoder", enc.name().into()),
        Err(e) => report(false, "H.264 encoder", format!("{e:#}")),
    }

    let uinput = std::fs::OpenOptions::new().write(true).open("/dev/uinput");
    report(
        uinput.is_ok(),
        "write access to /dev/uinput",
        uinput.err().map(|e| e.to_string()).unwrap_or_default(),
    );

    match desktop::monitors().await {
        Ok(m) => report(
            true,
            "Hyprland monitors",
            m.iter()
                .map(|m| format!("{} {}x{}", m.name, m.width, m.height))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Err(e) => report(false, "Hyprland monitors (hyprctl)", format!("{e:#}")),
    }
    for bin in ["wl-copy", "wl-paste", "notify-send"] {
        let ok = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {bin}")])
            .output()
            .is_ok_and(|o| o.status.success());
        report(ok, bin, String::new());
    }
    let picker = std::fs::read_to_string(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".config/hypr/xdph.conf"),
    )
    .unwrap_or_default();
    report(
        picker.contains("desk-picker"),
        "headless monitor picker (xdph.conf custom_picker_binary)",
        if picker.contains("desk-picker") {
            String::new()
        } else {
            "run scripts/setup.sh".into()
        },
    );
    let portal = ashpd::desktop::screencast::Screencast::new().await;
    report(
        portal.is_ok(),
        "ScreenCast portal",
        portal.err().map(|e| e.to_string()).unwrap_or_default(),
    );

    println!();
    if failures == 0 {
        println!("All checks passed.");
        Ok(())
    } else {
        Err(anyhow!("{failures} check(s) failed"))
    }
}
