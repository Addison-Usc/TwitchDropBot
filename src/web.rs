use std::{collections::HashMap, error::Error, path::{Path, PathBuf}, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};

use axum::{extract::State, http::{HeaderMap, StatusCode}, response::{Html, IntoResponse, Response}, routing::{get, post}, Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use twitch_gql_rs::{TwitchClient, client_type::ClientType};

use crate::{group_campaigns, run_session, SessionControl};

const SESSION_COOKIE: &str = "session";
const SESSION_TTL_SECS: u64 = 60 * 60;

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    pub ui_user: String,
    pub ui_password: String,
    pub client: Arc<Mutex<Option<Arc<TwitchClient>>>>,
    pub auth_info: Arc<Mutex<AuthInfo>>,
    pub running: Arc<Mutex<Option<SessionMeta>>>,
    pub sessions: Arc<Mutex<HashMap<String, std::time::Instant>>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum AuthStatus {
    Ready,
    Missing,
    Pending,
    Error,
}

#[derive(Clone, Debug, Serialize)]
struct AuthInfo {
    status: AuthStatus,
    verification_uri: Option<String>,
    user_code: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub control: SessionControl,
    pub started_at: SystemTime,
    pub run_until: Option<SystemTime>,
}

#[derive(Serialize)]
struct StatusResponse {
    auth: AuthInfo,
    running: bool,
    started_at: Option<u64>,
    run_until: Option<u64>,
}

#[derive(Serialize)]
struct CampaignInfo {
    game_id: String,
    display_name: String,
}

#[derive(Deserialize)]
struct StartRequest {
    game_id: String,
    duration_minutes: u64,
}

pub async fn serve(data_dir: PathBuf) -> Result<(), Box<dyn Error>> {
    let ui_user = std::env::var("UI_USER").map_err(|_| "UI_USER must be set")?;
    let ui_password = std::env::var("UI_PASSWORD").map_err(|_| "UI_PASSWORD must be set")?;

    let client = load_client_if_present(&data_dir).await;
    let auth_info = if client.is_some() {
        AuthInfo { status: AuthStatus::Ready, verification_uri: None, user_code: None, error: None }
    } else {
        AuthInfo { status: AuthStatus::Missing, verification_uri: None, user_code: None, error: None }
    };

    let state = AppState {
        data_dir,
        ui_user,
        ui_password,
        client: Arc::new(Mutex::new(client.map(Arc::new))),
        auth_info: Arc::new(Mutex::new(auth_info)),
        running: Arc::new(Mutex::new(None)),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/status", get(status))
        .route("/api/auth/start", post(auth_start))
        .route("/api/campaigns", get(campaigns))
        .route("/api/start", post(start_session))
        .route("/api/stop", post(stop_session))
        .with_state(state.clone());

    let host = std::env::var("UI_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8123);
    let addr = format!("{host}:{port}");
    tracing::info!("UI listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn load_client_if_present(data_dir: &Path) -> Option<TwitchClient> {
    let path = data_dir.join("save.json");
    if !path.exists() {
        return None;
    }
    TwitchClient::load_from_file(&path).await.ok()
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn login(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match verify_basic(&headers, &state).await {
        Ok(()) => {
            let token = Uuid::new_v4().to_string();
            let expiry = std::time::Instant::now() + Duration::from_secs(SESSION_TTL_SECS);
            let mut sessions = state.sessions.lock().await;
            sessions.insert(token.clone(), expiry);
            drop(sessions);
            let cookie = format!("{SESSION_COOKIE}={token}; Max-Age={SESSION_TTL_SECS}; Path=/; HttpOnly; SameSite=Strict");
            (StatusCode::NO_CONTENT, [(axum::http::header::SET_COOKIE, cookie)]).into_response()
        }
        Err(resp) => resp,
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let session = extract_session(&headers);
    if let Some(token) = session {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(&token);
    }
    let cookie = format!("{SESSION_COOKIE}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict");
    (StatusCode::NO_CONTENT, [(axum::http::header::SET_COOKIE, cookie)]).into_response()
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !check_session(&headers, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let auth = state.auth_info.lock().await.clone();
    let running = state.running.lock().await.clone();
    let (running_flag, started_at, run_until) = if let Some(meta) = running {
        (true, Some(to_epoch(meta.started_at)), meta.run_until.map(to_epoch))
    } else {
        (false, None, None)
    };

    Json(StatusResponse { auth, running: running_flag, started_at, run_until }).into_response()
}

async fn auth_start(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !check_session(&headers, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let mut auth_info = state.auth_info.lock().await;
    if matches!(auth_info.status, AuthStatus::Ready) {
        return Json(auth_info.clone()).into_response();
    }
    if matches!(auth_info.status, AuthStatus::Pending) {
        return Json(auth_info.clone()).into_response();
    }

    let client_type = ClientType::android_app();
    let mut client = match TwitchClient::new(&client_type).await {
        Ok(c) => c,
        Err(e) => {
            auth_info.status = AuthStatus::Error;
            auth_info.error = Some(format!("{e}"));
            return Json(auth_info.clone()).into_response();
        }
    };
    let device_auth = match client.request_device_auth().await {
        Ok(a) => a,
        Err(e) => {
            auth_info.status = AuthStatus::Error;
            auth_info.error = Some(format!("{e}"));
            return Json(auth_info.clone()).into_response();
        }
    };

    auth_info.status = AuthStatus::Pending;
    auth_info.verification_uri = Some(device_auth.verification_uri.clone());
    auth_info.user_code = Some(device_auth.user_code.clone());
    auth_info.error = None;

    let data_dir = state.data_dir.clone();
    let auth_state = state.auth_info.clone();
    let client_state = state.client.clone();
    tokio::spawn(async move {
        match client.auth(device_auth).await {
            Ok(()) => {}
            Err(e) => {
                let mut info = auth_state.lock().await;
                info.status = AuthStatus::Error;
                info.error = Some(format!("{e}"));
                return;
            }
        }
        let path = data_dir.join("save.json");
        let client = match client.save_file(&path).await {
            Ok(c) => c,
            Err(e) => {
                let mut info = auth_state.lock().await;
                info.status = AuthStatus::Error;
                info.error = Some(format!("{e}"));
                return;
            }
        };
        let mut client_lock = client_state.lock().await;
        *client_lock = Some(Arc::new(client));
        drop(client_lock);

        let mut info = auth_state.lock().await;
        info.status = AuthStatus::Ready;
        info.verification_uri = None;
        info.user_code = None;
        info.error = None;
    });

    Json(auth_info.clone()).into_response()
}

async fn campaigns(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !check_session(&headers, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let client = {
        let lock = state.client.lock().await;
        lock.clone()
    };
    let client = match client {
        Some(c) => c,
        None => return StatusCode::CONFLICT.into_response(),
    };

    let campaign = match client.get_campaign().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let grouped = group_campaigns(campaign.dropCampaigns);
    let mut items: Vec<CampaignInfo> = grouped
        .into_iter()
        .map(|(game_id, group)| CampaignInfo { game_id, display_name: group.display_name })
        .collect();
    items.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Json(items).into_response()
}

async fn start_session(State(state): State<AppState>, headers: HeaderMap, Json(req): Json<StartRequest>) -> Response {
    if !check_session(&headers, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let mut running_lock = state.running.lock().await;
    if running_lock.is_some() {
        return StatusCode::CONFLICT.into_response();
    }

    let client = {
        let lock = state.client.lock().await;
        lock.clone()
    };
    let client = match client {
        Some(c) => c,
        None => return StatusCode::CONFLICT.into_response(),
    };

    let campaign = match client.get_campaign().await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let grouped = group_campaigns(campaign.dropCampaigns);
    if !grouped.contains_key(&req.game_id) {
        return (StatusCode::BAD_REQUEST, "Unknown game_id").into_response();
    }

    let control = SessionControl::new();
    let run_until = if req.duration_minutes > 0 {
        Some(SystemTime::now() + Duration::from_secs(req.duration_minutes * 60))
    } else {
        None
    };

    *running_lock = Some(SessionMeta {
        control: control.clone(),
        started_at: SystemTime::now(),
        run_until,
    });
    drop(running_lock);

    let state_clone = state.clone();
    let data_dir = state.data_dir.clone();
    tokio::spawn(async move {
        let _ = run_session(client, grouped, &req.game_id, req.duration_minutes, &data_dir, control).await;
        let mut running = state_clone.running.lock().await;
        *running = None;
    });

    StatusCode::NO_CONTENT.into_response()
}

async fn stop_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !check_session(&headers, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let running = state.running.lock().await.clone();
    if let Some(meta) = running {
        meta.control.stop();
        return StatusCode::NO_CONTENT.into_response();
    }
    StatusCode::CONFLICT.into_response()
}

async fn verify_basic(headers: &HeaderMap, state: &AppState) -> Result<(), Response> {
    let auth = headers.get(axum::http::header::AUTHORIZATION).and_then(|h| h.to_str().ok());
    let auth = match auth {
        Some(a) => a,
        None => return Err(StatusCode::UNAUTHORIZED.into_response()),
    };
    let creds = match parse_basic(auth) {
        Some(c) => c,
        None => return Err(StatusCode::UNAUTHORIZED.into_response()),
    };
    if creds.0 == state.ui_user && creds.1 == state.ui_password {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED.into_response())
    }
}

async fn check_session(headers: &HeaderMap, state: &AppState) -> bool {
    let token = match extract_session(headers) {
        Some(t) => t,
        None => return false,
    };
    let mut sessions = state.sessions.lock().await;
    let expiry = match sessions.get(&token) {
        Some(e) => *e,
        None => return false,
    };
    if std::time::Instant::now() > expiry {
        sessions.remove(&token);
        return false;
    }
    true
}

fn extract_session(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return Some(value.to_string());
        }
    }
    None
}

fn parse_basic(header: &str) -> Option<(String, String)> {
    let header = header.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(header).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn to_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>TwitchDropBot</title>
  <style>
    :root {
      --bg: #0e1a20;
      --panel: #15232b;
      --accent: #62d6c6;
      --text: #e7f1f6;
      --muted: #9bb1be;
      --danger: #ff7b7b;
    }
    body {
      margin: 0;
      font-family: "IBM Plex Sans", "Segoe UI", sans-serif;
      background: radial-gradient(1200px 600px at 20% -10%, #183242, #0e1a20);
      color: var(--text);
    }
    .wrap {
      max-width: 860px;
      margin: 24px auto;
      padding: 20px;
    }
    .panel {
      background: var(--panel);
      border: 1px solid #1f333f;
      border-radius: 12px;
      padding: 16px;
      margin-bottom: 16px;
      box-shadow: 0 12px 30px rgba(0,0,0,0.25);
    }
    h1 { font-size: 28px; margin: 0 0 8px; }
    h2 { font-size: 18px; margin: 0 0 12px; color: var(--muted); }
    label { display: block; margin: 8px 0 4px; }
    input, select, button {
      font-size: 14px;
      padding: 10px 12px;
      border-radius: 8px;
      border: 1px solid #233a46;
      background: #0f1c23;
      color: var(--text);
    }
    button {
      background: var(--accent);
      color: #0c1418;
      border: none;
      cursor: pointer;
    }
    button.secondary {
      background: transparent;
      border: 1px solid #2d4757;
      color: var(--text);
    }
    .row { display: flex; gap: 12px; flex-wrap: wrap; }
    .row > * { flex: 1; }
    .mono { font-family: "IBM Plex Mono", "Consolas", monospace; }
    .muted { color: var(--muted); }
    .danger { color: var(--danger); }
    .hidden { display: none; }
    .status { display: flex; gap: 12px; align-items: center; }
  </style>
</head>
<body>
  <div class="wrap">
    <div class="panel">
      <h1>TwitchDropBot</h1>
      <div class="status">
        <div id="statusText" class="muted">Not logged in</div>
        <button class="secondary" id="refreshBtn">Refresh</button>
      </div>
    </div>

    <div class="panel" id="loginPanel">
      <h2>Login</h2>
      <div class="row">
        <div>
          <label>Username</label>
          <input id="loginUser" autocomplete="username">
        </div>
        <div>
          <label>Password</label>
          <input id="loginPass" type="password" autocomplete="current-password">
        </div>
      </div>
      <button id="loginBtn">Sign in</button>
      <div id="loginError" class="danger"></div>
    </div>

    <div class="panel hidden" id="authPanel">
      <h2>Device Authorization</h2>
      <div class="muted">Authorize the bot with your Twitch account.</div>
      <div id="authDetails" class="mono"></div>
      <button id="authStartBtn">Start Authorization</button>
    </div>

    <div class="panel hidden" id="controlPanel">
      <h2>Session Control</h2>
      <label>Campaign</label>
      <select id="campaignSelect"></select>
      <label>Run time (minutes, 0 = no limit)</label>
      <input id="durationInput" type="number" min="0" value="60">
      <div class="row">
        <button id="startBtn">Start</button>
        <button class="secondary" id="stopBtn">Stop</button>
      </div>
      <div id="runInfo" class="muted"></div>
    </div>
  </div>

  <script>
    const statusText = document.getElementById("statusText");
    const loginPanel = document.getElementById("loginPanel");
    const authPanel = document.getElementById("authPanel");
    const controlPanel = document.getElementById("controlPanel");
    const authDetails = document.getElementById("authDetails");
    const campaignSelect = document.getElementById("campaignSelect");
    const runInfo = document.getElementById("runInfo");
    const loginError = document.getElementById("loginError");

    async function api(path, options = {}) {
      const resp = await fetch(path, { credentials: "include", ...options });
      if (!resp.ok) {
        const text = await resp.text();
        throw new Error(text || resp.status);
      }
      if (resp.status === 204) return null;
      return resp.json();
    }

    async function refreshStatus() {
      loginError.textContent = "";
      try {
        const status = await api("/api/status");
        loginPanel.classList.add("hidden");
        statusText.textContent = status.running ? "Running" : "Idle";
        authPanel.classList.toggle("hidden", status.auth.status !== "missing" && status.auth.status !== "pending" && status.auth.status !== "error");
        controlPanel.classList.toggle("hidden", status.auth.status !== "ready");
        if (status.auth.status === "pending") {
          authDetails.textContent = `Go to ${status.auth.verification_uri} and enter code: ${status.auth.user_code}`;
        } else if (status.auth.status === "error") {
          authDetails.textContent = status.auth.error || "Auth error";
        } else {
          authDetails.textContent = "";
        }
        if (status.auth.status === "ready") {
          await loadCampaigns();
        }
        if (status.running) {
          runInfo.textContent = status.run_until ? `Runs until ${new Date(status.run_until * 1000).toLocaleString()}` : "Running with no time limit";
        } else {
          runInfo.textContent = "";
        }
      } catch {
        loginPanel.classList.remove("hidden");
        authPanel.classList.add("hidden");
        controlPanel.classList.add("hidden");
        statusText.textContent = "Not logged in";
      }
    }

    async function loadCampaigns() {
      const list = await api("/api/campaigns");
      campaignSelect.innerHTML = "";
      list.forEach(item => {
        const opt = document.createElement("option");
        opt.value = item.game_id;
        opt.textContent = item.display_name;
        campaignSelect.appendChild(opt);
      });
    }

    document.getElementById("loginBtn").addEventListener("click", async () => {
      const user = document.getElementById("loginUser").value;
      const pass = document.getElementById("loginPass").value;
      const token = btoa(`${user}:${pass}`);
      try {
        await fetch("/api/login", { method: "POST", headers: { Authorization: `Basic ${token}` }, credentials: "include" });
        await refreshStatus();
      } catch (err) {
        loginError.textContent = "Login failed";
      }
    });

    document.getElementById("authStartBtn").addEventListener("click", async () => {
      try {
        const status = await api("/api/auth/start", { method: "POST" });
        authDetails.textContent = `Go to ${status.verification_uri} and enter code: ${status.user_code}`;
      } catch (err) {
        authDetails.textContent = "Failed to start auth";
      }
    });

    document.getElementById("startBtn").addEventListener("click", async () => {
      const game_id = campaignSelect.value;
      const duration_minutes = Number(document.getElementById("durationInput").value || 0);
      await api("/api/start", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ game_id, duration_minutes }) });
      await refreshStatus();
    });

    document.getElementById("stopBtn").addEventListener("click", async () => {
      await api("/api/stop", { method: "POST" });
      await refreshStatus();
    });

    document.getElementById("refreshBtn").addEventListener("click", refreshStatus);
    refreshStatus();
  </script>
</body>
</html>
"#;
