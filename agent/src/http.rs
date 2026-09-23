//! HTTP surface: static UI, login/logout, and the WebSocket used only for WebRTC signaling.
//! Every API/WS request must pass Cloudflare Access verification and an exact Origin check;
//! the WebSocket additionally requires a logged-in session.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tracing::{info, warn};

use crate::auth::{self, AccessVerifier, Sessions};
use crate::config::{AuthState, Config, StateDir, TurnConfig};
use crate::rtc::{self, PeerSettings, PortalStore};

const COOKIE: &str = "__Host-desk";
const MAX_WS_MESSAGE: usize = 256 * 1024;

pub struct AppState {
    pub cfg: Config,
    /// None only in `--insecure-local` mode (loopback testing without Cloudflare).
    pub access: Option<AccessVerifier>,
    pub sessions: Sessions,
    pub auth_state: Mutex<AuthState>,
    pub state_dir: StateDir,
    pub portal: Arc<PortalStore>,
    pub http: reqwest::Client,
    /// Stops the currently active viewer (one viewer at a time).
    pub active: Mutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>>,
    pub next_session_id: std::sync::atomic::AtomicU64,
    pub origin: String,
    pub encoder: crate::capture::Encoder,
}

type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/ws", get(ws))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_access,
        ));

    Router::new()
        .merge(api)
        .fallback_service(ServeDir::new(&state.cfg.web_dir).append_index_html_on_directories(true))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; \
             connect-src 'self'; media-src 'self' blob:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn deny(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Gate 1: Cloudflare Access JWT, plus exact Origin on anything state-changing.
async fn require_access(State(st): State<Shared>, req: Request, next: Next) -> Response {
    if let Some(verifier) = &st.access {
        let token = req
            .headers()
            .get("cf-access-jwt-assertion")
            .and_then(|v| v.to_str().ok());
        let Some(token) = token else {
            return deny(StatusCode::FORBIDDEN, "missing Access token");
        };
        if let Err(e) = verifier.verify(token).await {
            warn!("Access verification failed: {e:#}");
            return deny(StatusCode::FORBIDDEN, "Access verification failed");
        }
    }
    let needs_origin = req.method() != axum::http::Method::GET || req.uri().path() == "/ws";
    if needs_origin
        && req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            != Some(st.origin.as_str())
    {
        return deny(StatusCode::FORBIDDEN, "bad origin");
    }
    next.run(req).await
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            kv.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_string)
        })
}

fn logged_in(st: &AppState, headers: &HeaderMap) -> bool {
    session_cookie(headers).is_some_and(|t| st.sessions.is_valid(&t, Instant::now()))
}

async fn me(State(st): State<Shared>, headers: HeaderMap) -> Response {
    Json(json!({ "authenticated": logged_in(&st, &headers) })).into_response()
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
    code: String,
}

/// Gates 2 and 3: password + TOTP. Both are always evaluated; the response never says which failed.
async fn login(State(st): State<Shared>, Json(body): Json<LoginBody>) -> Response {
    let now = Instant::now();
    if let Some(left) = st.sessions.locked_for(now) {
        return deny(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("locked, try again in {} min", left.as_secs() / 60 + 1),
        );
    }
    if body.password.len() > 1024 || body.code.len() > 16 {
        return deny(StatusCode::BAD_REQUEST, "invalid input");
    }

    let mut auth_state = st.auth_state.lock().await;
    let hash = auth_state.password_hash.clone();
    let password_ok =
        tokio::task::spawn_blocking(move || auth::verify_password(&hash, &body.password))
            .await
            .unwrap_or(false);
    let step = auth::check_totp(&auth_state, &body.code, auth::unix_now());

    let (true, Some(step)) = (password_ok, step) else {
        st.sessions.record_failure(now);
        // Uniform slow-down on failure.
        tokio::time::sleep(Duration::from_millis(500)).await;
        return deny(StatusCode::UNAUTHORIZED, "invalid password or code");
    };
    auth_state.last_totp_step = step;
    if let Err(e) = st.state_dir.write("auth.json", &*auth_state) {
        warn!("could not persist TOTP step: {e:#}");
    }
    st.sessions.record_success();
    let token = st.sessions.create(now);
    info!("login succeeded");
    let cookie = format!(
        "{COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={}",
        auth::SESSION_TTL.as_secs()
    );
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn logout(State(st): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = session_cookie(&headers) {
        st.sessions.revoke(&t);
    }
    if let Some((_, stop)) = st.active.lock().await.take() {
        let _ = stop.send(());
    }
    let cookie = format!("{COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

async fn ws(State(st): State<Shared>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    if !logged_in(&st, &headers) {
        return deny(StatusCode::UNAUTHORIZED, "login required");
    }
    upgrade
        .max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| signaling(st, socket))
}

async fn ice_servers(st: &AppState) -> serde_json::Value {
    let stun = json!([{ "urls": ["stun:stun.cloudflare.com:3478"] }]);
    let Some(TurnConfig { key_id, api_token }) = &st.cfg.turn else {
        return stun;
    };
    let url = format!(
        "https://rtc.live.cloudflare.com/v1/turn/keys/{key_id}/credentials/generate-ice-servers"
    );
    let res = st
        .http
        .post(url)
        .bearer_auth(api_token)
        .json(&json!({ "ttl": 86400 }))
        .send()
        .await;
    match res.and_then(|r| r.error_for_status()) {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("iceServers").cloned())
            .unwrap_or(stun),
        Err(e) => {
            warn!("TURN credential request failed: {e}");
            stun
        }
    }
}

async fn signaling(st: Shared, mut socket: WebSocket) {
    let hello = json!({ "type": "hello", "iceServers": ice_servers(&st).await });
    if socket.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    let mut my_session: Option<u64> = None;

    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if v["type"] != "offer" {
            continue;
        }
        let Some(sdp) = v["sdp"].as_str() else {
            continue;
        };

        // One viewer at a time: stop whoever was connected before.
        if let Some((_, prev)) = st.active.lock().await.take() {
            let _ = prev.send(());
        }
        let settings = PeerSettings {
            udp_port: st.cfg.udp_port,
            portal: st.portal.clone(),
            encoder: st.encoder,
            host: st.cfg.public_host().to_string(),
        };
        let reply = match rtc::start(sdp, settings).await {
            Ok((answer, stop)) => {
                let id = st
                    .next_session_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                *st.active.lock().await = Some((id, stop));
                my_session = Some(id);
                json!({ "type": "answer", "sdp": answer })
            }
            Err(e) => {
                warn!("could not start session: {e:#}");
                json!({ "type": "error", "message": "could not start session" })
            }
        };
        if socket.send(Message::text(reply.to_string())).await.is_err() {
            break;
        }
    }
    // Signaling socket closed (tab closed / navigated away): end *our* media session, but not
    // a newer one that already replaced it.
    let mut active = st.active.lock().await;
    if my_session.is_some()
        && active.as_ref().map(|(id, _)| *id) == my_session
        && let Some((_, stop)) = active.take()
    {
        let _ = stop.send(());
    }
}

pub async fn serve(state: Shared) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(state.cfg.listen).await?;
    info!(addr = %state.cfg.listen, origin = %state.origin, "desk-agent listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
