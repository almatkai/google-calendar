//! Native Google Calendar / People adapter. Behavior attributed to the installed
//! google-calendar 1.4.4 package's .sc-build/{create-event,create-quick-event,
//! search-contacts,list-events,list-calendars}.js (read-only source).
//! OAuth uses a user-owned Google Desktop OAuth client, NOT Raycast's client ID.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::path::PathBuf;
use std::time::Duration as StdDuration;
use tezbar_extension_sdk as v2_sdk;
use tezbar_extension_sdk::{CoreResult, ExtensionContext};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[cfg(test)]
const SLUG: &str = "google-calendar";
const API: &str = "https://www.googleapis.com";
const PEOPLE_API: &str = "https://people.googleapis.com";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPES: &str = "https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/contacts.readonly https://www.googleapis.com/auth/contacts.other.readonly https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile openid";
const PAGE_LIMIT: usize = 100;

pub fn commands() -> &'static [&'static str] {
    &[
        "create-event",
        "create-quick-event",
        "search-contacts",
        "list-events",
        "list-calendars",
    ]
}
fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    s(&v[key])
}
fn node(kind: &str, props: Value, children: Vec<Value>) -> Value {
    json!({"type":kind,"props":props,"children":children})
}
fn action(id: &str, title: &str) -> Value {
    json!({"id":id,"title":title,"kind":"action"})
}
fn form_field(kind: &str, id: &str, title: &str, value: Value) -> Value {
    node(kind, json!({"id":id,"title":title,"value":value}), vec![])
}
fn list(title: &str, rows: Vec<Value>, actions: Vec<Value>, more: bool, search: bool) -> Value {
    let mut result = v2_sdk::view(
        title,
        node(
            "List",
            json!({"__hasMore":more,"__hasServerSearch":search,"searchBarPlaceholder":if search {"Search contacts..."} else {"Search events..."}}),
            rows,
        ),
        actions,
    );
    result["hasMore"] = json!(more);
    result
}
fn auth_view() -> Value {
    v2_sdk::view(
        "Connect Google Calendar",
        node(
            "Detail",
            json!({"markdown":"Connect your own Google Desktop OAuth client to access Calendar and Contacts. Set TEZBAR_GOOGLE_OAUTH_CLIENT_ID to your Desktop application's OAuth client ID, then select Connect. The installed extension's Raycast client ID is registered for Raycast redirects and cannot be assumed to accept Tezbar's loopback callback. Existing Raycast tokens are not imported."}),
            vec![],
        ),
        vec![action("google:connect", "Connect Google")],
    )
}
#[cfg(test)]
fn auth_file(ctx: &ExtensionContext) -> PathBuf {
    ctx.paths.data_dir.join("google-oauth").join("storage.json")
}
const STORE: &str = "google-oauth";
static AUTH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn validate_tokens(value: &Value) -> Result<(), String> {
    if !value.is_object() {
        return Err("Google OAuth credential file is not an object".into());
    }
    for key in ["access_token", "refresh_token", "client_id"] {
        if let Some(v) = value.get(key) {
            if !v.is_string() {
                return Err(format!("Invalid Google OAuth {key}"));
            }
        }
    }
    if value.get("expires_at").is_some_and(|v| !v.is_i64()) {
        return Err("Invalid Google OAuth expiry".into());
    }
    if value
        .get("migrationComplete")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("Invalid Google OAuth migration marker".into());
    }
    Ok(())
}
async fn read_tokens(ctx: &ExtensionContext) -> CoreResult {
    let value = v2_sdk::read_store(ctx, STORE).await?;
    validate_tokens(&value)?;
    Ok(value)
}
async fn save_tokens(ctx: &ExtensionContext, value: &Value) -> Result<(), String> {
    validate_tokens(value)?;
    let expected = read_tokens(ctx).await?;
    let mut replacement = expected.clone();
    for (key, value) in value.as_object().unwrap() {
        replacement[key] = value.clone();
    }
    replace_tokens(ctx, &expected, &replacement).await
}
async fn replace_tokens(
    ctx: &ExtensionContext,
    expected: &Value,
    value: &Value,
) -> Result<(), String> {
    validate_tokens(value)?;
    let dir = ctx.paths.data_dir.join(STORE);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let replacement = value.clone();
    let expected = expected.clone();
    v2_sdk::update_store(ctx, STORE, move |current| {
        validate_tokens(current)?;
        if *current != expected {
            return Err("Google credentials changed during update; retry the command".into());
        }
        *current = replacement;
        Ok(())
    })
    .await?;
    Ok(())
}
/// Host supplies one exact legacy native token file; never discover app config or Raycast grants.
pub async fn initialize(ctx: &ExtensionContext) -> CoreResult {
    let _guard = AUTH_LOCK.lock().await;
    let mut current = read_tokens(ctx).await?;
    if current["migrationComplete"] == true {
        return Ok(json!({"ready":true}));
    }
    if current.as_object().is_some_and(|v| v.is_empty()) {
        if let Some(path) = ctx.migration_paths.get("nativeGoogleOAuth") {
            if !path.is_absolute() {
                return Err("OAuth migration path must be absolute".into());
            }
            match std::fs::symlink_metadata(path) {
                Ok(meta) => {
                    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 * 1024
                    {
                        return Err("Invalid OAuth migration file".into());
                    }
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)
                        .map_err(|e| e.to_string())?
                        .take(1024 * 1024 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    if bytes.len() > 1024 * 1024 {
                        return Err("OAuth migration file exceeds 1 MiB".into());
                    }
                    let legacy: Value = serde_json::from_slice(&bytes)
                        .map_err(|_| "Malformed OAuth migration file")?;
                    validate_tokens(&legacy)?;
                    for key in ["access_token", "refresh_token", "expires_at", "client_id"] {
                        if let Some(value) = legacy.get(key) {
                            current[key] = value.clone();
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    current["migrationComplete"] = json!(true);
    save_tokens(ctx, &current).await?;
    Ok(json!({"ready":true}))
}
fn client_id() -> Result<String, String> {
    let id = std::env::var("TEZBAR_GOOGLE_OAUTH_CLIENT_ID").unwrap_or_default();
    if !id.ends_with(".apps.googleusercontent.com") || id.contains(char::is_whitespace) {
        return Err("Set TEZBAR_GOOGLE_OAUTH_CLIENT_ID to a Google Desktop OAuth client ID before connecting".into());
    }
    Ok(id)
}
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(StdDuration::from_secs(25))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}
async fn bounded_json(response: reqwest::Response) -> CoreResult {
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Cannot read Google response")?
    {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err("Google response exceeds 8 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "Google response was not JSON".into())
}
async fn response_json(response: reqwest::Response) -> CoreResult {
    let status = response.status();
    let value = bounded_json(response).await?;
    if !status.is_success() {
        return Err(format!(
            "Google API {}: {}",
            status.as_u16(),
            if field(&value["error"], "message").is_empty() {
                s(&value["error"]).to_string()
            } else {
                field(&value["error"], "message").to_string()
            }
        ));
    }
    Ok(value)
}
fn token_expiry(response: &Value) -> Result<i64, String> {
    let lifetime = match response.get("expires_in") {
        None => 3600,
        Some(value) => value.as_i64().ok_or("Invalid OAuth token lifetime")?,
    };
    if !(1..=31_536_000).contains(&lifetime) {
        return Err("Invalid OAuth token lifetime".into());
    }
    Utc::now()
        .timestamp()
        .checked_add(lifetime)
        .ok_or_else(|| "OAuth expiry overflow".into())
}
async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<Value, String> {
    response_json(
        http.post(endpoint)
            .form(params)
            .send()
            .await
            .map_err(|e| format!("Google OAuth request failed: {e}"))?,
    )
    .await
}
async fn access_token(
    ctx: &ExtensionContext,
    http: &reqwest::Client,
    token_url: &str,
) -> Result<Option<String>, String> {
    let _guard = AUTH_LOCK.lock().await;
    let stored = read_tokens(ctx).await?;
    let current = field(&stored, "access_token");
    if !current.is_empty()
        && stored["expires_at"].as_i64().unwrap_or(0) > Utc::now().timestamp() + 90
    {
        return Ok(Some(current.to_owned()));
    }
    let refresh = field(&stored, "refresh_token").to_owned();
    if refresh.is_empty() {
        return Ok(None);
    }
    let id = client_id()?;
    if stored.get("client_id").is_some_and(|v| v != &json!(id)) {
        return Err("Google Desktop OAuth client changed; reconnect Google".into());
    }
    refresh_access_token(ctx, http, token_url, &id, stored).await
}

async fn refresh_access_token(
    ctx: &ExtensionContext,
    http: &reqwest::Client,
    token_url: &str,
    id: &str,
    mut stored: Value,
) -> Result<Option<String>, String> {
    let snapshot = stored.clone();
    let refresh = field(&stored, "refresh_token").to_owned();
    if refresh.is_empty() {
        return Ok(None);
    }
    let response = http
        .post(token_url)
        .form(&[
            ("client_id", id),
            ("refresh_token", refresh.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| format!("Google OAuth refresh request failed: {e}"))?;
    let status = response.status();
    let value = bounded_json(response).await?;
    if status == reqwest::StatusCode::BAD_REQUEST && field(&value, "error") == "invalid_grant" {
        // A revoked/expired grant needs fresh consent, not an endless error loop.
        // Never discard newer credentials written by another session meanwhile.
        let current = read_tokens(ctx).await?;
        if field(&current, "refresh_token").is_empty() {
            return Ok(None);
        }
        if current["refresh_token"] != stored["refresh_token"]
            || current["access_token"] != stored["access_token"]
        {
            return Err("Google credentials changed during refresh; retry the command".into());
        }
        let mut cleared = current.clone();
        cleared.as_object_mut().unwrap().remove("access_token");
        cleared.as_object_mut().unwrap().remove("refresh_token");
        cleared.as_object_mut().unwrap().remove("expires_at");
        replace_tokens(ctx, &current, &cleared).await?;
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!(
            "Google OAuth refresh failed ({}): {}",
            status.as_u16(),
            value["error_description"]
                .as_str()
                .or(value["error"]["message"].as_str())
                .or(value["error"].as_str())
                .unwrap_or("Unknown OAuth error")
        ));
    }
    let response = value;
    let access = field(&response, "access_token");
    if access.is_empty() {
        return Err("Google OAuth refresh returned no access token".into());
    }
    stored["client_id"] = json!(id);
    stored["access_token"] = json!(access);
    stored["expires_at"] = json!(token_expiry(&response)?);
    if let Some(new_refresh) = response["refresh_token"].as_str() {
        stored["refresh_token"] = json!(new_refresh);
    }
    replace_tokens(ctx, &snapshot, &stored).await?;
    Ok(Some(access.to_owned()))
}

// OAuth 2.0 Authorization Code + PKCE loopback. The listener is bound BEFORE the
// browser opens; state and callback path are checked before the code is exchanged.
async fn connect_inner(
    ctx: &ExtensionContext,
    http: &reqwest::Client,
    token_url: &str,
) -> Result<(), String> {
    let _guard = AUTH_LOCK.lock().await;
    let id = client_id()?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Cannot open OAuth loopback listener: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}/oauth/google");
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().to_string();
    let auth = format!("https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256&state={}",
        v2_sdk::encode_component(&id), v2_sdk::encode_component(&redirect), v2_sdk::encode_component(SCOPES), challenge, state);
    v2_sdk::open(ctx, &auth).await?;
    let code = callback_code(&listener, &state).await?;
    let token = token_request(
        http,
        token_url,
        &[
            ("code", &code),
            ("client_id", &id),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
            ("grant_type", "authorization_code"),
        ],
    )
    .await?;
    if field(&token, "access_token").is_empty() || field(&token, "refresh_token").is_empty() {
        return Err(
            "Google OAuth did not grant offline access; reconnect and allow consent".into(),
        );
    }
    save_tokens(
        ctx,
        &json!({"access_token":token["access_token"],"refresh_token":token["refresh_token"],"client_id":id,"migrationComplete":true,"expires_at":token_expiry(&token)?}),
    ).await
}

async fn connect(
    ctx: &ExtensionContext,
    http: &reqwest::Client,
    endpoint: &str,
) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let owned = ctx.clone();
    let http = http.clone();
    let endpoint = endpoint.to_string();
    ctx.spawn(async move {
        let result = connect_inner(&owned, &http, &endpoint).await;
        let _ = tx.send(result);
    })?;
    rx.await
        .map_err(|_| "Google authorization cancelled".to_string())?
}

async fn callback_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    tokio::time::timeout(StdDuration::from_secs(180), async {
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let request = tokio::time::timeout(StdDuration::from_secs(5), async {
                let mut data = Vec::new();
                loop {
                    let mut chunk = [0u8; 1024];
                    let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
                    if n == 0 { return Err("Incomplete OAuth callback".to_string()); }
                    data.extend_from_slice(&chunk[..n]);
                    if data.len() > 8192 { return Err("OAuth callback too large".to_string()); }
                    if data.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                }
                String::from_utf8(data).map_err(|_| "Invalid OAuth callback encoding".to_string())
            }).await;
            let Ok(Ok(request)) = request else { continue; };
            let mut first = request.lines().next().unwrap_or("").split_whitespace();
            if first.next() != Some("GET") { continue; }
            let Some(path) = first.next().filter(|p| p.starts_with('/') && !p.starts_with("//")) else { continue; };
            let url = match reqwest::Url::parse(&format!("http://127.0.0.1{path}")) { Ok(url) => url, Err(_) => continue };
            let pairs: Vec<_> = url.query_pairs().collect();
            let unique = |key: &str| -> Option<String> {
                let values: Vec<_> = pairs.iter().filter(|(k,_)| k == key).collect();
                if values.len() == 1 { Some(values[0].1.to_string()) } else { None }
            };
            let valid = url.path() == "/oauth/google" && unique("state").as_deref() == Some(state);
            let reply = if valid { "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nAuthorization received. Return to Tezbar." } else { "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nInvalid OAuth callback." };
            let _ = tokio::time::timeout(StdDuration::from_secs(2), stream.write_all(reply.as_bytes())).await;
            if !valid { continue; }
            if let Some(error) = unique("error") { return Err(format!("Google authorization denied: {error}")); }
            return unique("code").filter(|s| !s.is_empty()).ok_or_else(|| "Google callback omitted authorization code".into());
        }
    }).await.map_err(|_| "Google authorization timed out".to_string())?
}

fn endpoints() -> Result<(String, String), String> {
    #[cfg(feature = "fixture-network")]
    if let Ok(base) = std::env::var("TEZBAR_GOOGLE_FIXTURE_ORIGIN") {
        let url = reqwest::Url::parse(&base).map_err(|_| "Invalid fixture origin")?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("Fixture origin must be an explicit IPv4 loopback HTTP port".into());
        }
        return Ok((
            base.trim_end_matches('/').into(),
            format!("{}/token", base.trim_end_matches('/')),
        ));
    }
    Ok((API.into(), TOKEN_URL.into()))
}

struct Google<'a> {
    http: &'a reqwest::Client,
    base: &'a str,
    token: &'a str,
}
impl Google<'_> {
    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, String> {
        // People has its own documented service origin; local fixtures retain their loopback base.
        let base = if self.base == API && path.starts_with("/v1/") {
            PEOPLE_API
        } else {
            self.base
        };
        response_json(
            self.http
                .get(format!("{base}{path}"))
                .bearer_auth(self.token)
                .query(params)
                .send()
                .await
                .map_err(|e| format!("Google API request failed: {e}"))?,
        )
        .await
    }
    async fn post(
        &self,
        path: &str,
        params: &[(&str, String)],
        body: &Value,
    ) -> Result<Value, String> {
        response_json(
            self.http
                .post(format!("{}{path}", self.base))
                .bearer_auth(self.token)
                .query(params)
                .json(body)
                .send()
                .await
                .map_err(|e| format!("Google API request failed: {e}"))?,
        )
        .await
    }
    async fn patch(&self, path: &str, body: &Value) -> Result<Value, String> {
        response_json(
            self.http
                .patch(format!("{}{path}", self.base))
                .bearer_auth(self.token)
                .json(body)
                .send()
                .await
                .map_err(|e| format!("Google API request failed: {e}"))?,
        )
        .await
    }
    async fn delete(&self, path: &str) -> Result<(), String> {
        let response = self
            .http
            .delete(format!("{}{path}", self.base))
            .bearer_auth(self.token)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("Google API delete failed: {}", response.status()));
        }
        Ok(())
    }
    async fn pages(&self, path: &str, params: &[(&str, String)]) -> Result<Vec<Value>, String> {
        let mut all = Vec::new();
        let mut cursor = String::new();
        for _ in 0..PAGE_LIMIT {
            let mut query = params.to_vec();
            if !cursor.is_empty() {
                query.push(("pageToken", cursor.clone()));
            }
            let response = self.get(path, &query).await?;
            all.extend(response["items"].as_array().cloned().unwrap_or_default());
            cursor = field(&response, "nextPageToken").to_string();
            if cursor.is_empty() {
                return Ok(all);
            }
        }
        Err("Google pagination exceeded 100 pages; results are not complete".into())
    }
}
fn cal_path(id: &str) -> String {
    format!(
        "/calendar/v3/calendars/{}/events",
        v2_sdk::encode_component(id)
    )
}
fn item(
    id: String,
    title: &str,
    subtitle: &str,
    action_ids: Vec<String>,
    accessories: Vec<Value>,
) -> Value {
    node(
        "List.Item",
        json!({"id":id,"title":title,"subtitle":subtitle,"actionIds":action_ids,"accessories":accessories}),
        vec![],
    )
}
fn set_items(state: &mut Value, items: Vec<Value>) {
    state["items"] = json!(items);
}
fn add_items(state: &mut Value, items: Vec<Value>) {
    if !state["items"].is_array() {
        state["items"] = json!([]);
    }
    state["items"].as_array_mut().unwrap().extend(items);
}
fn render(command: &str, state: &Value) -> Value {
    let command = if command == "list-calendars" && state["calendarView"] == true {
        "list-events"
    } else {
        command
    };
    let items = state["items"].as_array().cloned().unwrap_or_default();
    match command {
        "list-calendars" => {
            let mut rows = Vec::new();
            let mut actions = Vec::new();
            for (i, c) in items.iter().enumerate() {
                let id = field(c, "id");
                if id.is_empty() {
                    continue;
                }
                let name = c["summaryOverride"]
                    .as_str()
                    .or(c["summary"].as_str())
                    .unwrap_or("Untitled Calendar");
                let a = format!("google:calendar:{i}");
                rows.push(item(id.into(),name,field(c,"description"),vec![a.clone()],vec![json!({"tag":if c["primary"] == true {"Primary"} else if c["selected"] == true {"Visible"} else {"Hidden"}}),json!({"tag":field(c,"accessRole")})]));
                actions.push(action(&a, "Show Events"));
            }
            list("Google Calendars", rows, actions, false, false)
        }
        "list-events" => {
            let mut rows = Vec::new();
            let mut actions = Vec::new();
            for (i, e) in items.iter().enumerate() {
                let open = format!("google:event:open:{i}");
                let copy = format!("google:event:copy:{i}");
                let copy_title = format!("google:event:title:{i}");
                let copy_meeting = format!("google:event:copy-meeting:{i}");
                let del = format!("google:event:delete:{i}");
                let meet = format!("google:event:meet:{i}");
                let respond = format!("google:event:respond:{i}");
                let meeting = field(&e["conferenceData"]["entryPoints"][0], "uri");
                let primary = if state["openCommandJoinsMeeting"] == true && !meeting.is_empty() {
                    meet.clone()
                } else if !field(e, "htmlLink").is_empty() {
                    open.clone()
                } else if !meeting.is_empty() {
                    meet.clone()
                } else {
                    copy_title.clone()
                };
                let mut ids = vec![primary];
                let mut accessories = Vec::new();
                if state["showLocation"] == true && !field(e, "location").is_empty() {
                    accessories.push(json!({"text":e["location"],"icon":"Pin"}));
                }
                if e["recurringEventId"].is_string() || e["recurrence"].is_array() {
                    accessories.push(json!({"icon":"Repeat"}));
                }
                if let Some(attendees) = e["attendees"].as_array() {
                    accessories.push(json!({"text":attendees.len().to_string(),"icon":"Person"}));
                }
                if !meeting.is_empty() {
                    ids.push(meet.clone());
                    actions.push(action(&meet, "Join Meeting"));
                    ids.push(copy_meeting.clone());
                    actions.push(action(&copy_meeting, "Copy Meeting Link"));
                }
                if !field(e, "htmlLink").is_empty() {
                    ids.push(open.clone());
                    actions.push(action(&open, "Open in Google Calendar"));
                    ids.push(copy.clone());
                    actions.push(action(&copy, "Copy Event Link"));
                }
                ids.push(copy_title.clone());
                actions.push(action(&copy_title, "Copy Event Title"));
                if !field(e, "id").is_empty() {
                    ids.push(respond.clone());
                    actions.push(action(&respond, "Change Response (Yes / Maybe / No)"));
                    ids.push(del.clone());
                    actions.push(action(&del, "Delete Event"));
                }
                let start = e["start"]["dateTime"]
                    .as_str()
                    .or(e["start"]["date"].as_str())
                    .unwrap_or("");
                let end = e["end"]["dateTime"]
                    .as_str()
                    .or(e["end"]["date"].as_str())
                    .unwrap_or("");
                rows.push(item(
                    format!("{}:{i}", field(e, "id")),
                    e["summary"].as_str().unwrap_or("Untitled Event"),
                    &format!("{start} – {end}"),
                    ids,
                    accessories,
                ));
            }
            let more = !field(state, "nextPageToken").is_empty();
            let mut result = list("Upcoming Events", rows, actions, more, false);
            let mut options = vec![node(
                "List.Dropdown.Item",
                json!({"title":"Primary","value":"primary"}),
                vec![],
            )];
            if let Some(calendars) = state["calendars"].as_array() {
                for c in calendars {
                    let id = field(c, "id");
                    if !id.is_empty() && id != "primary" {
                        options.push(node("List.Dropdown.Item",json!({"title":c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or(id),"value":id}),vec![]));
                    }
                }
            }
            options.push(node(
                "List.Dropdown.Item",
                json!({"title":"Birthdays","value":"birthdays"}),
                vec![],
            ));
            let selected = field(state, "calendar");
            if let Some(position) = options
                .iter()
                .position(|c| field(&c["props"], "value") == selected)
            {
                options.swap(0, position);
            }
            result["root"]["props"]["searchBarAccessory"] = node(
                "List.Dropdown",
                json!({"actionId":"google:calendar-select"}),
                options,
            );
            result
        }
        "search-contacts" => {
            let mut rows = Vec::new();
            let mut actions = Vec::new();
            for (i, c) in items.iter().enumerate() {
                let email = field(&c["emailAddresses"][0], "value");
                if email.is_empty() {
                    continue;
                }
                let a = format!("google:contact:copy:{i}");
                rows.push(item(
                    field(c, "resourceName").into(),
                    email,
                    field(&c["names"][0], "displayName"),
                    vec![a.clone()],
                    vec![],
                ));
                actions.push(action(&a, "Copy Email"));
            }
            list("Search Contacts", rows, actions, false, true)
        }
        _ => v2_sdk::unchanged(),
    }
}
fn create_form(command: &str, calendars: &[Value], prefs: &Value, state: &Value) -> Value {
    let quick = command == "create-quick-event";
    let rounded = (Utc::now().timestamp() + 899).div_euclid(900) * 900;
    let suggested_start = DateTime::<Utc>::from_timestamp(rounded, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&chrono::Local)
        .to_rfc3339();
    let fields = if quick {
        vec![form_field(
            "Form.TextField",
            "input",
            "Title and Date/Time",
            json!(field(state, "input")),
        )]
    } else {
        let options:Vec<Value>=calendars.iter().filter(|c| matches!(field(c,"accessRole"),"owner"|"writer") && !field(c,"id").is_empty()).map(|c|json!({"title":c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or("Calendar"),"value":c["id"]})).collect();
        vec![
            node(
                "Form.Dropdown",
                json!({"id":"calendar","title":"Calendar","value":state["calendar"].as_str().unwrap_or("primary"),"data":options}),
                vec![],
            ),
            form_field("Form.TextField", "title", "Title", json!("")),
            form_field("Form.Checkbox", "allDay", "All-day", json!(false)),
            node(
                "Form.TextField",
                json!({"id":"startDate","title":"Start Date and Time","value":suggested_start,"placeholder":"2027-06-15T15:00:00+02:00 (or YYYY-MM-DD for all-day)","info":"Timed events require an ISO 8601 timezone offset (or Z)."}),
                vec![],
            ),
            form_field(
                "Form.DatePicker",
                "endDate",
                "End Date (all-day, inclusive)",
                Value::Null,
            ),
            form_field(
                "Form.TextField",
                "duration",
                "Duration (minutes or 1h30m)",
                json!(prefs["defaultEventDuration"].as_str().unwrap_or("30")),
            ),
            form_field(
                "Form.TextField",
                "attendees",
                "Guests (emails separated by commas)",
                json!(""),
            ),
            form_field(
                "Form.Dropdown",
                "conferencingProvider",
                "Conferencing",
                json!("none"),
            ),
            form_field(
                "Form.TextField",
                "conferenceUrl",
                "Other Meeting URL",
                json!(""),
            ),
            form_field("Form.TextArea", "description", "Description", json!("")),
            form_field(
                "Form.Dropdown",
                "sendInvitations",
                "Send Invitations",
                json!(prefs["sendInvitations"].as_str().unwrap_or("all")),
            ),
        ]
    };
    let fields=fields.into_iter().map(|mut f| { if f["props"]["id"]=="conferencingProvider" {f["props"]["data"]=json!([{"title":"None","value":"none"},{"title":"Google Meet","value":"hangoutsMeet"}]);}
        if f["props"]["id"]=="sendInvitations" {f["props"]["data"]=json!([{"title":"All guests","value":"all"},{"title":"External guests","value":"externalOnly"},{"title":"None","value":"none"}]);} f }).collect();
    let title = if quick {
        "Create Quick Event"
    } else {
        "Create Event"
    };
    v2_sdk::view(
        title,
        node("Form", json!({}), fields),
        vec![json!({"id":"google:submit","title":title,"kind":"submit-form"})],
    )
}
fn minutes(text: &str) -> Result<i64, String> {
    let input = text.trim().to_lowercase();
    if let Ok(number) = input.parse::<i64>() {
        return (1..=10080)
            .contains(&number)
            .then_some(number)
            .ok_or_else(|| "Duration must be between 1 and 10080 minutes".into());
    }
    let re = regex::Regex::new(r"(\d+)\s*(hours?|hrs?|h|minutes?|mins?|m)").unwrap();
    let mut sum = 0_i64;
    let mut end = 0;
    for cap in re.captures_iter(&input) {
        let matched = cap.get(0).unwrap();
        if !input[end..matched.start()].trim().is_empty() {
            return Err("Invalid duration; use 30, 45m or 1h30m".into());
        }
        let count = cap[1].parse::<i64>().map_err(|e| e.to_string())?;
        sum = sum
            .checked_add(
                count
                    .checked_mul(if cap[2].starts_with('h') { 60 } else { 1 })
                    .ok_or("Duration overflow")?,
            )
            .ok_or("Duration overflow")?;
        end = matched.end();
    }
    if !input[end..].trim().is_empty() || !(1..=10080).contains(&sum) {
        return Err("Invalid duration; use 30, 45m or 1h30m".into());
    }
    Ok(sum)
}
fn day(raw: &str) -> Result<NaiveDate, String> {
    if let Ok(d) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(d);
    }
    DateTime::parse_from_rfc3339(raw)
        .map(|d| d.date_naive())
        .map_err(|_| "Date must be YYYY-MM-DD or ISO 8601 with timezone".into())
}
fn date_time(raw: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(raw)
        .map(|d| d.to_utc())
        .map_err(|_| "Timed events require a timezone-aware ISO 8601 start date".into())
}
type EventRequest = (String, Value, Vec<(&'static str, String)>);
fn event_body(values: &Value, prefs: &Value) -> Result<EventRequest, String> {
    let title = field(values, "title").trim();
    if title.is_empty() {
        return Err("Event title is required".into());
    }
    let id = values["calendar"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("primary")
        .to_owned();
    let all_day = values["allDay"].as_bool().unwrap_or(false);
    let (start, end) = if all_day {
        let begin = day(field(values, "startDate"))?;
        let last = if field(values, "endDate").is_empty() {
            begin
        } else {
            day(field(values, "endDate"))?
        };
        if last < begin {
            return Err("All-day end cannot precede start".into());
        }
        (
            json!({"date":begin.to_string()}),
            json!({"date":last.succ_opt().ok_or("Date overflow")?.to_string()}),
        )
    } else {
        let start = date_time(field(values, "startDate"))?;
        let duration = minutes(
            values["duration"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(prefs["defaultEventDuration"].as_str().unwrap_or("30")),
        )?;
        (
            json!({"dateTime":start.to_rfc3339()}),
            json!({"dateTime":(start+Duration::minutes(duration)).to_rfc3339()}),
        )
    };
    let guests = field(values, "attendees")
        .split([',', ';', '\n'])
        .filter(|v| !v.trim().is_empty())
        .map(|v| {
            let address = v
                .trim()
                .rsplit_once('<')
                .and_then(|(_, rest)| rest.strip_suffix('>'))
                .unwrap_or(v.trim())
                .trim();
            let (local, domain) = address
                .split_once('@')
                .ok_or_else(|| format!("Invalid guest email: {address}"))?;
            if local.is_empty() || !domain.contains('.') || address.chars().any(char::is_whitespace)
            {
                return Err(format!("Invalid guest email: {address}"));
            }
            Ok(json!({"email":address}))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut description = field(values, "description").to_string();
    if prefs["addSignature"].as_bool().unwrap_or(true) {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str("Created with Tezbar");
    }
    let provider = field(values, "conferencingProvider");
    let mut body = json!({"summary":title,"start":start,"end":end,"description":description});
    if !guests.is_empty() {
        body["attendees"] = json!(guests);
    }
    let mut params = vec![];
    if provider == "hangoutsMeet" {
        body["conferenceData"] = json!({"createRequest":{"conferenceSolutionKey":{"type":"hangoutsMeet"},"requestId":uuid::Uuid::new_v4().to_string()}});
        params.push(("conferenceDataVersion", "1".into()));
    } else if !provider.is_empty() && provider != "none" {
        return Err("Unsupported conferencing provider".into());
    }
    let other_url = field(values, "conferenceUrl").trim();
    if !other_url.is_empty() {
        if provider == "hangoutsMeet" {
            return Err("Choose either Google Meet or another meeting URL".into());
        }
        let parsed =
            reqwest::Url::parse(other_url).map_err(|_| "Invalid meeting URL".to_string())?;
        if parsed.scheme() != "https" || parsed.host_str().is_none() {
            return Err("Meeting URL must be HTTPS".into());
        }
        body["location"] = json!(other_url);
    }
    let send = values["sendInvitations"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or(prefs["sendInvitations"].as_str().unwrap_or("all"));
    if !matches!(send, "all" | "externalOnly" | "none") {
        return Err("Invalid invitation preference".into());
    }
    params.push(("sendUpdates", send.into()));
    Ok((id, body, params))
}
fn https_link(url: &str) -> Result<&str, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid Google link".to_string())?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err("External links must use HTTPS".into());
    }
    Ok(url)
}
fn index(action: &str, prefix: &str) -> Option<usize> {
    action.strip_prefix(prefix)?.parse::<usize>().ok()
}
async fn load_calendars(api: &Google<'_>) -> Result<Vec<Value>, String> {
    api.pages("/calendar/v3/users/me/calendarList", &[]).await
}
fn event_types(cal: &str, birthdays: bool) -> Vec<(&'static str, String)> {
    let types: &[&str] = if cal == "birthdays"
        || cal.contains("addressbook#contacts")
        || cal.contains("#contacts@group")
    {
        &["birthday"]
    } else if birthdays {
        &[
            "default",
            "focusTime",
            "outOfOffice",
            "workingLocation",
            "fromGmail",
            "birthday",
        ]
    } else {
        &[
            "default",
            "focusTime",
            "outOfOffice",
            "workingLocation",
            "fromGmail",
        ]
    };
    types.iter().map(|t| ("eventTypes", (*t).into())).collect()
}
fn events_calendar(cal: &str) -> &str {
    if cal == "birthdays" {
        "primary"
    } else {
        cal
    }
}
async fn refresh(api: &Google<'_>, command: &str, state: &mut Value) -> Result<(), String> {
    match command {
        "list-calendars" => {
            set_items(state, load_calendars(api).await?);
        }
        "list-events" => {
            let cal = state["calendar"].as_str().unwrap_or("primary");
            let mut query = vec![
                ("timeMin", Utc::now().to_rfc3339()),
                ("maxResults", "50".into()),
                ("singleEvents", "true".into()),
                ("orderBy", "startTime".into()),
            ];
            query.extend(event_types(cal, state["showBirthdays"] == true));
            let page = api.get(&cal_path(events_calendar(cal)), &query).await?;
            set_items(state, page["items"].as_array().cloned().unwrap_or_default());
            state["nextPageToken"] = page["nextPageToken"].clone();
            state["timeMin"] = json!(query[0].1);
        }
        "search-contacts" => {
            let query = field(state, "query").trim();
            let contact_params = [
                ("query", query.into()),
                ("readMask", "names,emailAddresses,photos".into()),
                ("pageSize", "30".into()),
                ("sources", "READ_SOURCE_TYPE_CONTACT".into()),
                ("sources", "READ_SOURCE_TYPE_PROFILE".into()),
            ];
            let other_params = [
                ("query", query.into()),
                ("readMask", "names,emailAddresses".into()),
                ("pageSize", "30".into()),
            ];
            let (people, other) = tokio::try_join!(
                api.get("/v1/people:searchContacts", &contact_params),
                api.get("/v1/otherContacts:search", &other_params)
            )?;
            if query.is_empty() {
                // Google requires an empty-query warmup before searching each contact collection.
                set_items(state, vec![]);
                return Ok(());
            }
            let contacts = people["results"]
                .as_array()
                .into_iter()
                .chain(other["results"].as_array())
                .flatten()
                .filter_map(|r| r.get("person").cloned())
                .collect();
            set_items(state, contacts);
        }
        _ => {}
    }
    Ok(())
}
async fn invoke(
    ctx: &ExtensionContext,
    api: &Google<'_>,
    command: &str,
    action_id: &str,
    payload: &Value,
    state: &mut Value,
) -> CoreResult {
    let values = &payload["formValues"];
    if (action_id == "google:submit" || action_id.starts_with("google:created:"))
        && !matches!(command, "create-event" | "create-quick-event")
    {
        return Err("Action does not belong to this command".into());
    }
    let confirming = action_id == "google:confirm";
    let action_id = if confirming {
        let pending = field(state, "pending");
        if pending.is_empty() {
            return Err("No pending Google action".into());
        }
        pending.to_owned()
    } else {
        action_id.to_owned()
    };
    let action_id = action_id.as_str();
    if action_id == "google:submit" {
        let event = if command == "create-quick-event" {
            let text = field(values, "input").trim();
            if text.is_empty() {
                return Err("Enter an event title and date/time".into());
            }
            // Google's quickAdd parses natural-language expressions; no synthetic local result.
            api.post(
                &format!("{}/quickAdd", cal_path("primary")),
                &[("text", text.into())],
                &json!({}),
            )
            .await?
        } else {
            let prefs = &payload["preferences"];
            let (id, body, params) = event_body(values, prefs)?;
            state["calendar"] = json!(id);
            api.post(&cal_path(&id), &params, &body).await?
        };
        if field(&event, "id").is_empty() {
            return Err("Google did not return a created event ID".into());
        }
        state["createdEvent"] = event;
        return Ok(created_view(state));
    }
    if action_id == "google:created:open" {
        let url = field(&state["createdEvent"], "htmlLink");
        if url.is_empty() {
            return Err("Created event has no link".into());
        }
        v2_sdk::open(ctx, https_link(url)?).await?;
        return Ok(created_view(state));
    }
    if action_id == "google:created:delete" {
        if !confirming {
            state["pending"] = json!(action_id);
            return Ok(confirm_view("Delete Created Event", true));
        }
        if values["confirmed"] != true {
            return Err("Confirm deletion to continue".into());
        }
        let id = field(&state["createdEvent"], "id");
        let cal = state["calendar"].as_str().unwrap_or("primary");
        if id.is_empty() {
            return Err("Created event has no ID".into());
        }
        api.delete(&format!(
            "{}/{}",
            cal_path(cal),
            v2_sdk::encode_component(id)
        ))
        .await?;
        state["createdEvent"] = Value::Null;
        state["pending"] = Value::Null;
        return Ok(v2_sdk::no_view("Event deleted"));
    }
    if action_id == "google:calendar-select" {
        if command != "list-events"
            && !(command == "list-calendars" && state["calendarView"] == true)
        {
            return Err("Calendar selection unavailable".into());
        }
        let id = field(values, "value");
        if id != "primary"
            && id != "birthdays"
            && !state["calendars"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| field(c, "id") == id)
        {
            return Err("Unknown calendar".into());
        }
        state["calendar"] = json!(id);
        refresh(api, "list-events", state).await?;
        return Ok(render(command, state));
    }
    if let Some(i) = index(action_id, "google:calendar:") {
        if command != "list-calendars" {
            return Err("Action does not belong to this command".into());
        }
        let selected = state["items"]
            .as_array()
            .and_then(|v| v.get(i))
            .ok_or("Calendar not found")?;
        let id = field(selected, "id").to_owned();
        if id.is_empty() {
            return Err("Calendar has no ID".into());
        }
        state["calendar"] = json!(id);
        state["calendarView"] = json!(true);
        state["calendars"] = json!(load_calendars(api).await?);
        refresh(api, "list-events", state).await?;
        return Ok(render(command, state));
    }
    if let Some(i) = index(action_id, "google:contact:copy:") {
        if command != "search-contacts" {
            return Err("Action does not belong to this command".into());
        }
        let contact = state["items"]
            .as_array()
            .and_then(|v| v.get(i))
            .ok_or("Contact not found")?;
        let email = field(&contact["emailAddresses"][0], "value");
        if email.is_empty() {
            return Err("Contact has no email".into());
        }
        v2_sdk::copy(ctx, email).await?;
        return Ok(render(command, state));
    }
    for (kind, prefix) in [
        ("open", "google:event:open:"),
        ("copy", "google:event:copy:"),
        ("title", "google:event:title:"),
        ("copy-meeting", "google:event:copy-meeting:"),
        ("meet", "google:event:meet:"),
        ("delete", "google:event:delete:"),
        ("respond", "google:event:respond:"),
    ] {
        let Some(i) = index(action_id, prefix) else {
            continue;
        };
        if command != "list-events"
            && !(command == "list-calendars" && state["calendarView"] == true)
        {
            return Err("Action does not belong to this command".into());
        }
        let event = state["items"]
            .as_array()
            .and_then(|v| v.get(i))
            .ok_or("Event not found")?
            .clone();
        if matches!(kind, "delete" | "respond") && !confirming {
            state["pending"] = json!(action_id);
            return Ok(confirm_view(
                if kind == "delete" {
                    "Delete Event"
                } else {
                    "Respond to Event"
                },
                kind == "delete",
            ));
        }
        match kind {
            "open" | "meet" => {
                let target = if kind == "meet" {
                    field(&event["conferenceData"]["entryPoints"][0], "uri")
                } else {
                    field(&event, "htmlLink")
                };
                if target.is_empty() {
                    return Err("Event has no link".into());
                }
                v2_sdk::open(ctx, https_link(target)?).await?;
            }
            "copy" => {
                let url = field(&event, "htmlLink");
                if url.is_empty() {
                    return Err("Event has no link".into());
                }
                v2_sdk::copy(ctx, url).await?;
            }
            "title" => {
                let title = field(&event, "summary");
                if title.is_empty() {
                    return Err("Event has no title".into());
                }
                v2_sdk::copy(ctx, title).await?;
            }
            "copy-meeting" => {
                let uri = field(&event["conferenceData"]["entryPoints"][0], "uri");
                if uri.is_empty() {
                    return Err("Event has no meeting link".into());
                }
                v2_sdk::copy(ctx, uri).await?;
            }
            "delete" => {
                if payload["formValues"]["confirmed"] != true {
                    return Err("Deleting an event requires confirmed=true".into());
                }
                let id = field(&event, "id");
                if id.is_empty() {
                    return Err("Event has no ID".into());
                }
                let cal = state["calendar"].as_str().unwrap_or("primary");
                api.delete(&format!(
                    "{}/{}",
                    cal_path(cal),
                    v2_sdk::encode_component(id)
                ))
                .await?;
                refresh(api, "list-events", state).await?;
            }
            "respond" => {
                let status = field(values, "status");
                if !matches!(status, "accepted" | "tentative" | "declined") {
                    return Err("Specify response status accepted, tentative or declined".into());
                }
                let attendees = event["attendees"]
                    .as_array()
                    .ok_or("Event has no attendees")?;
                let mut attendees = attendees.clone();
                let self_attendee = attendees
                    .iter_mut()
                    .find(|v| v["self"] == true)
                    .ok_or("You are not an attendee")?;
                self_attendee["responseStatus"] = json!(status);
                let id = field(&event, "id");
                let cal = state["calendar"].as_str().unwrap_or("primary");
                api.patch(
                    &format!("{}/{}", cal_path(cal), v2_sdk::encode_component(id)),
                    &json!({"attendees":attendees}),
                )
                .await?;
                refresh(api, "list-events", state).await?;
            }
            _ => unreachable!(),
        }
        state["pending"] = Value::Null;
        return Ok(render(command, state));
    }
    Err(format!("Unknown Google action: {action_id}"))
}
fn confirm_view(title: &str, deleting: bool) -> Value {
    let fields = if deleting {
        vec![form_field(
            "Form.Checkbox",
            "confirmed",
            "Yes, delete this event",
            json!(false),
        )]
    } else {
        vec![node(
            "Form.Dropdown",
            json!({"id":"status","title":"Response","value":"accepted","data":[{"title":"Yes","value":"accepted"},{"title":"Maybe","value":"tentative"},{"title":"No","value":"declined"}]}),
            vec![],
        )]
    };
    v2_sdk::view(
        title,
        node("Form", json!({}), fields),
        vec![json!({"id":"google:confirm","title":title,"kind":"submit-form"})],
    )
}
fn created_view(state: &Value) -> Value {
    let event = &state["createdEvent"];
    let mut actions = vec![];
    if !field(event, "htmlLink").is_empty() {
        actions.push(action("google:created:open", "Open in Google Calendar"));
    }
    if !field(event, "id").is_empty() {
        actions.push(action("google:created:delete", "Delete Created Event"));
    }
    v2_sdk::view(
        "Created Event",
        node(
            "Detail",
            json!({"markdown":format!("# {}\n\n{}",field(event,"summary"),field(event,"htmlLink"))}),
            vec![],
        ),
        actions,
    )
}

pub async fn handle(
    ctx: &ExtensionContext,
    command: &str,
    operation: &str,
    payload: &Value,
    state: &mut Value,
) -> CoreResult {
    if !commands().contains(&command) {
        return Err(format!("Unknown Google Calendar command: {command}"));
    }
    if operation == "extension:dispose-session" {
        return Ok(v2_sdk::unchanged());
    }
    if operation == "extension:run-command" {
        state["launchContext"] = payload
            .get("launchContext")
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    let endpoints = endpoints()?;
    let http = client()?;
    let action_id = field(payload, "actionId");
    let operation = if operation == "extension:invoke-action" && action_id == "google:connect" {
        connect(ctx, &http, &endpoints.1).await?;
        "extension:run-command"
    } else {
        operation
    };
    let Some(token) = access_token(ctx, &http, &endpoints.1).await? else {
        return Ok(auth_view());
    };
    let api = Google {
        http: &http,
        base: &endpoints.0,
        token: &token,
    };
    match operation {
        "extension:run-command" => {
            if command == "create-event" {
                state["calendar"] = json!(state["launchContext"]["calendar"]
                    .as_str()
                    .unwrap_or("primary"));
            }
            if command == "list-events" {
                state["calendars"] = json!(load_calendars(&api).await?);
                state["calendar"] = json!(state["launchContext"]["calendarId"]
                    .as_str()
                    .unwrap_or("primary"));
                state["showBirthdays"] = json!(payload["preferences"]["showBirthdays"]
                    .as_bool()
                    .unwrap_or(false));
                state["showLocation"] = json!(payload["preferences"]["showLocation"]
                    .as_bool()
                    .unwrap_or(false));
                state["openCommandJoinsMeeting"] = json!(payload["preferences"]
                    ["openCommandJoinsMeeting"]
                    .as_bool()
                    .unwrap_or(false));
            }
            if command.starts_with("create-") {
                let calendars = if command == "create-event" {
                    load_calendars(&api).await?
                } else {
                    vec![]
                };
                return Ok(create_form(
                    command,
                    &calendars,
                    &payload["preferences"],
                    state,
                ));
            }
            refresh(&api, command, state).await?;
            Ok(render(command, state))
        }
        "extension:search-text-changed" => {
            if command == "search-contacts" {
                state["query"] = json!(field(payload, "searchText"));
                refresh(&api, command, state).await?;
            }
            Ok(render(command, state))
        }
        "extension:load-more" => {
            if (command == "list-events" || state["calendarView"] == true)
                && !field(state, "nextPageToken").is_empty()
            {
                let cal = state["calendar"].as_str().unwrap_or("primary");
                let mut params = vec![
                    ("pageToken", field(state, "nextPageToken").into()),
                    ("maxResults", "50".into()),
                    ("singleEvents", "true".into()),
                    ("orderBy", "startTime".into()),
                    ("timeMin", field(state, "timeMin").into()),
                ];
                params.extend(event_types(cal, state["showBirthdays"] == true));
                let page = api.get(&cal_path(events_calendar(cal)), &params).await?;
                add_items(state, page["items"].as_array().cloned().unwrap_or_default());
                state["nextPageToken"] = page["nextPageToken"].clone();
            }
            Ok(render(command, state))
        }
        "extension:refresh-session" => {
            if command.starts_with("create-") {
                if state["createdEvent"].is_object() {
                    Ok(created_view(state))
                } else {
                    let calendars = if command == "create-event" {
                        load_calendars(&api).await?
                    } else {
                        vec![]
                    };
                    Ok(create_form(
                        command,
                        &calendars,
                        &payload["preferences"],
                        state,
                    ))
                }
            } else {
                refresh(
                    &api,
                    if state["calendarView"] == true {
                        "list-events"
                    } else {
                        command
                    },
                    state,
                )
                .await?;
                Ok(render(command, state))
            }
        }
        "extension:invoke-action" => invoke(ctx, &api, command, action_id, payload, state).await,
        _ => Err(format!(
            "Unsupported Google Calendar operation: {operation}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_context(root: &std::path::Path) -> v2_sdk::TestContext {
        v2_sdk::TestContext::new(
            v2_sdk::ExtensionPaths {
                data_dir: root.into(),
                package_dir: root.join("package"),
                temp_dir: root.join("tmp"),
            },
            &["shell.open", "clipboard.write"],
        )
    }
    #[test]
    fn duration_and_date_guards() {
        assert_eq!(minutes("1h30m").unwrap(), 90);
        assert_eq!(minutes("45 min").unwrap(), 45);
        assert!(minutes("2h garbage").is_err());
        assert!(minutes("0").is_err());
        let prefs =
            json!({"addSignature":false,"defaultEventDuration":"30","sendInvitations":"none"});
        let (id,body,query)=event_body(&json!({"calendar":"primary","title":"Review","allDay":true,"startDate":"2026-12-31","endDate":"2027-01-01"}),&prefs).unwrap();
        assert_eq!(id, "primary");
        assert_eq!(body["end"]["date"], "2027-01-02");
        assert_eq!(query[0].1, "none");
        assert!(event_body(
            &json!({"title":"Meeting","startDate":"2027-01-01T10:00:00","attendees":"bad"}),
            &prefs
        )
        .is_err());
    }
    #[tokio::test]
    async fn fixture_records_bearer_pagination_and_event_write() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let recorder = tokio::spawn(async move {
            let mut seen = vec![];
            for step in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let size = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..size]).to_string();
                seen.push(request.clone());
                let body = match step {
                    0 => r#"{"items":[{"id":"first"}],"nextPageToken":"next-page"}"#,
                    1 => r#"{"items":[{"id":"second"}]}"#,
                    _ => r#"{"id":"created","summary":"Meeting"}"#,
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            seen
        });
        let http = client().unwrap();
        let api = Google {
            http: &http,
            base: &base,
            token: "fixture-token",
        };
        let items = api
            .pages("/calendar/v3/users/me/calendarList", &[])
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        let created = api
            .post(
                "/calendar/v3/calendars/primary/events",
                &[("sendUpdates", "none".into())],
                &json!({"summary":"Meeting"}),
            )
            .await
            .unwrap();
        assert_eq!(created["id"], "created");
        let seen = recorder.await.unwrap();
        assert!(seen.iter().all(
            |request| request.contains("Authorization: Bearer fixture-token")
                || request.contains("authorization: Bearer fixture-token")
        ));
        assert!(seen[1].contains("pageToken=next-page"));
        assert!(seen[2].contains("POST /calendar/v3/calendars/primary/events?sendUpdates=none"));
    }
    #[tokio::test]
    async fn fixture_refresh_request_sends_form_and_parses_token_without_real_grant() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let n = stream.read(&mut request).await.unwrap();
            let observed = String::from_utf8_lossy(&request[..n]).into_owned();
            let body = r#"{"access_token":"fixture-access","expires_in":3600}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            observed
        });
        let result = token_request(
            &client().unwrap(),
            &endpoint,
            &[
                ("client_id", "fixture-client"),
                ("refresh_token", "fixture-refresh"),
                ("grant_type", "refresh_token"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(result["access_token"], "fixture-access");
        let observed = server.await.unwrap();
        assert!(observed.contains("POST /token"));
        assert!(observed.contains("application/x-www-form-urlencoded"));
    }
    #[tokio::test]
    async fn revoked_refresh_returns_connect_view_but_transient_errors_preserve_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = test_context(temp.path());
        let ctx = fixture.context.clone();
        let expired = json!({"access_token":"fixture-expired","refresh_token":"fixture-revoked","expires_at":0});
        save_tokens(&ctx, &expired).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (status, body) in [
                (503, r#"{"error":"temporarily_unavailable"}"#),
                (
                    400,
                    r#"{"error":"invalid_grant","error_description":"Token revoked"}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let http = client().unwrap();
        assert!(refresh_access_token(
            &ctx,
            &http,
            &endpoint,
            "fixture-desktop-client",
            expired.clone()
        )
        .await
        .is_err());
        assert_eq!(
            read_tokens(&ctx).await.unwrap(),
            expired,
            "a transient service error must not erase a valid grant"
        );
        assert!(
            refresh_access_token(&ctx, &http, &endpoint, "fixture-desktop-client", expired)
                .await
                .unwrap()
                .is_none()
        );
        server.await.unwrap();
        assert_eq!(
            read_tokens(&ctx).await.unwrap(),
            json!({}),
            "invalid_grant must remove both access and refresh tokens"
        );
        assert!(access_token(&ctx, &http, &endpoint)
            .await
            .unwrap()
            .is_none());
        let view = auth_view();
        assert_eq!(view["mode"], "view");
        assert_eq!(view["actions"][0]["id"], "google:connect");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(auth_file(&ctx))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[tokio::test]
    async fn tokens_are_separate_from_sdk_store_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = test_context(temp.path());
        let ctx = fixture.context.clone();
        save_tokens(&ctx, &json!({"refresh_token":"fixture-only"}))
            .await
            .unwrap();
        assert_eq!(
            read_tokens(&ctx).await.unwrap()["refresh_token"],
            "fixture-only"
        );
        assert!(!ctx
            .paths
            .data_dir
            .join("native-extensions-v2")
            .join(SLUG)
            .exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(auth_file(&ctx))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[tokio::test]
    async fn malformed_private_tokens_are_rejected_and_never_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = test_context(temp.path());
        let path = auth_file(&fixture.context);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for bytes in [
            "not json",
            "[]",
            r#"{"refresh_token":false}"#,
            r#"{"expires_at":"tomorrow"}"#,
            r#"{"migrationComplete":"true"}"#,
            r#"{"migrationComplete":null}"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(initialize(&fixture.context).await.is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn event_request_preserves_timezone_guests_signature_conference_and_guards() {
        let prefs = json!({"addSignature":true,"defaultEventDuration":"45","sendInvitations":"externalOnly"});
        let values = json!({"title":"Planning","calendar":"team@example.test","startDate":"2027-01-02T10:30:00+06:00","attendees":"Ada <ada@example.test>;bob@example.test","conferencingProvider":"hangoutsMeet","description":"Agenda"});
        let (calendar, body, query) = event_body(&values, &prefs).unwrap();
        assert_eq!(calendar, "team@example.test");
        assert_eq!(body["start"]["dateTime"], "2027-01-02T04:30:00+00:00");
        assert_eq!(body["end"]["dateTime"], "2027-01-02T05:15:00+00:00");
        assert_eq!(body["attendees"][0]["email"], "ada@example.test");
        assert_eq!(body["description"], "Agenda\n\nCreated with Tezbar");
        assert!(query.contains(&("conferenceDataVersion", "1".into())));
        assert!(query.contains(&("sendUpdates", "externalOnly".into())));
        let mut invalid = values.clone();
        invalid["conferenceUrl"] = json!("https://example.test/meeting");
        assert!(event_body(&invalid, &prefs).is_err());
        invalid["conferencingProvider"] = json!("none");
        invalid["conferenceUrl"] = json!("file:///tmp/unsafe");
        assert!(event_body(&invalid, &prefs).is_err());
        assert!(minutes("9999999999999999999h").is_err());
        assert!(https_link("javascript:alert(1)").is_err());
        for value in [json!(-1), json!(i64::MAX), json!("3600")] {
            assert!(token_expiry(&json!({"expires_in":value})).is_err());
        }
    }
    #[tokio::test]
    async fn stale_token_commit_cannot_replace_new_credentials_or_unknown_keys() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = test_context(temp.path());
        let first = json!({"refresh_token":"first", "futureField":{"preserve":true}});
        save_tokens(&fixture.context, &first).await.unwrap();
        let newer = json!({"refresh_token":"newer", "futureField":{"preserve":true}});
        replace_tokens(&fixture.context, &first, &newer)
            .await
            .unwrap();
        assert!(
            replace_tokens(&fixture.context, &first, &json!({"refresh_token":"stale"}))
                .await
                .is_err()
        );
        assert_eq!(read_tokens(&fixture.context).await.unwrap(), newer);
    }
    #[tokio::test]
    async fn reconnect_token_update_preserves_unknown_destination_keys() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = test_context(temp.path());
        save_tokens(&fixture.context, &json!({"refresh_token":"old","access_token":"old-access","futureField":{"preserve":true},"migrationComplete":true})).await.unwrap();
        save_tokens(
            &fixture.context,
            &json!({"refresh_token":"new","access_token":"new-access","expires_at":1234}),
        )
        .await
        .unwrap();
        let updated = read_tokens(&fixture.context).await.unwrap();
        assert_eq!(updated["refresh_token"], "new");
        assert_eq!(updated["access_token"], "new-access");
        assert_eq!(updated["futureField"]["preserve"], true);
        assert_eq!(updated["migrationComplete"], true);
    }
}
