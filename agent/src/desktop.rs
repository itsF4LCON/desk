//! Small integrations with the Hyprland desktop: monitor layout, clipboard, notifications,
//! and the one-shot request file read by the `desk-picker` portal wrapper.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::input::Rect;

const CLIPBOARD_MAX: usize = 1024 * 1024;
const CMD_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Monitor {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            w: self.width,
            h: self.height,
        }
    }
}

pub async fn monitors() -> Result<Vec<Monitor>> {
    let out = tokio::time::timeout(
        CMD_TIMEOUT,
        Command::new("hyprctl").args(["monitors", "-j"]).output(),
    )
    .await
    .context("hyprctl timed out")??;
    if !out.status.success() {
        bail!("hyprctl monitors failed");
    }
    let mut mons: Vec<Monitor> =
        serde_json::from_slice(&out.stdout).context("unexpected hyprctl output")?;
    mons.sort_by_key(|m| (m.x, m.y));
    Ok(mons)
}

/// Bounding box of all monitors (the space the absolute pointer spans).
pub fn layout(mons: &[Monitor]) -> Rect {
    let x0 = mons.iter().map(|m| m.x).min().unwrap_or(0);
    let y0 = mons.iter().map(|m| m.y).min().unwrap_or(0);
    let x1 = mons.iter().map(|m| m.x + m.width).max().unwrap_or(1920);
    let y1 = mons.iter().map(|m| m.y + m.height).max().unwrap_or(1080);
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn pick_request_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(Into::into)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    dir.join("desk-agent").join("pick")
}

/// Tells `desk-picker` which monitor to answer the next portal request with.
/// Returns a guard that removes the request so no other app can consume it later.
pub fn request_pick(monitor: &str) -> Result<PickGuard> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = pick_request_path();
    let dir = path.parent().unwrap();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    std::io::Write::write_all(&mut f, monitor.as_bytes())?;
    Ok(PickGuard(path))
}

pub struct PickGuard(std::path::PathBuf);
impl Drop for PickGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn clipboard_get() -> Result<String> {
    let out = tokio::time::timeout(
        CMD_TIMEOUT,
        Command::new("wl-paste")
            .args(["--no-newline", "--type", "text"])
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("wl-paste timed out")??;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.truncate(text.floor_char_boundary(CLIPBOARD_MAX));
    Ok(text)
}

pub async fn clipboard_set(text: &str) -> Result<()> {
    if text.len() > CLIPBOARD_MAX {
        bail!("clipboard text too large");
    }
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .await?;
    // wl-copy forks to serve the selection; the parent exits once it has the data.
    tokio::time::timeout(CMD_TIMEOUT, child.wait())
        .await
        .context("wl-copy timed out")??;
    Ok(())
}

/// Visible on the PC itself, so a remote session is never silent.
pub fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=desk", "--urgency=critical", summary, body])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_of_two_side_by_side() {
        let m = |name: &str, x| Monitor {
            name: name.into(),
            x,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            layout(&[m("A", 0), m("B", 1920)]),
            Rect {
                x: 0,
                y: 0,
                w: 3840,
                h: 1080
            }
        );
    }
}
