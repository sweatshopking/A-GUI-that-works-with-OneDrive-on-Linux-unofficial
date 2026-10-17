//! Read-only Microsoft Graph calls for things the client has no command for: browsing online
//! folders (including ones not synced yet), finding SharePoint libraries and reading quota.
//!
//! We borrow the client's saved sign-in (refresh_token) to get a short-lived access token. The
//! refresh token file is only ever read, never rewritten, so the client's own sign-in is unaffected.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::ClientConfig;

const DEFAULT_APP_ID: &str = "d50ca740-c83f-4d1b-b616-12c519384f0c";
const SCOPE: &str = "Files.ReadWrite Files.ReadWrite.All Sites.ReadWrite.All offline_access";
const GRAPH: &str = "https://graph.microsoft.com/v1.0";

pub struct Token {
    pub access: String,
    pub expires: Instant,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn endpoints(cfg: &ClientConfig) -> (String, String) {
    let tenant = cfg.get("azure_tenant_id").filter(|s| !s.is_empty()).unwrap_or_else(|| "common".into());
    let host = match cfg.get("azure_ad_endpoint").unwrap_or_default().as_str() {
        "USL4" | "USL5" => "https://login.microsoftonline.us",
        "DE" => "https://login.microsoftonline.de",
        "CN" => "https://login.chinacloudapi.cn",
        _ => "https://login.microsoftonline.com",
    };
    (format!("{host}/{tenant}/oauth2/v2.0/token"), format!("{host}/common/oauth2/nativeclient"))
}

pub fn token(confdir: &Path) -> Result<Token, String> {
    let rt = std::fs::read_to_string(confdir.join("refresh_token"))
        .map_err(|_| "This account isn't signed in.".to_string())?;
    let cfg = ClientConfig::load(confdir);
    let app = cfg.get("application_id").filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_APP_ID.into());
    let (url, redirect) = endpoints(&cfg);
    let mut resp = agent()
        .post(&url)
        .send_form([
            ("client_id", app.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", rt.trim()),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPE),
        ])
        .map_err(|e| format!("Couldn't reach Microsoft: {e}"))?;
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    match v["access_token"].as_str() {
        Some(a) => Ok(Token {
            access: a.to_string(),
            expires: Instant::now() + Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120)),
        }),
        None => Err(format!(
            "Microsoft refused the saved sign-in ({}). Try signing in again.",
            v["error"].as_str().unwrap_or("unknown error")
        )),
    }
}

fn get(tok: &Token, url: &str) -> Result<Value, String> {
    let mut resp = agent()
        .get(url)
        .header("Authorization", &format!("Bearer {}", tok.access))
        .call()
        .map_err(|e| format!("Couldn't reach Microsoft: {e}"))?;
    let status = resp.status().as_u16();
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    if status >= 400 {
        return Err(v["error"]["message"].as_str().unwrap_or("request failed").to_string());
    }
    Ok(v)
}

fn get_all(tok: &Token, mut url: String) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    loop {
        let v = get(tok, &url)?;
        if let Some(items) = v["value"].as_array() {
            out.extend(items.iter().cloned());
        }
        match v["@odata.nextLink"].as_str() {
            Some(next) => url = next.to_string(),
            None => return Ok(out),
        }
    }
}

fn enc(seg: &str) -> String {
    let mut s = String::new();
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn drive_base(drive_id: Option<&str>) -> String {
    match drive_id {
        Some(d) => format!("{GRAPH}/drives/{}", enc(d)),
        None => format!("{GRAPH}/me/drive"),
    }
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    pub has_children: bool,
}

/// Lists the sub-folders of `path` ("/" for the top).
pub fn list_folders(tok: &Token, drive_id: Option<&str>, path: &str) -> Result<Vec<Folder>, String> {
    let base = drive_base(drive_id);
    let p = path.trim_matches('/');
    let item = if p.is_empty() {
        format!("{base}/root")
    } else {
        format!("{base}/root:/{}:", p.split('/').map(enc).collect::<Vec<_>>().join("/"))
    };
    let items = get_all(tok, format!("{item}/children?$select=name,folder&$top=999"))?;
    let mut out: Vec<Folder> = items
        .iter()
        .filter(|i| i.get("folder").is_some())
        .map(|i| Folder {
            name: i["name"].as_str().unwrap_or("").to_string(),
            has_children: i["folder"]["childCount"].as_u64().unwrap_or(1) > 0,
        })
        .collect();
    out.sort_by_key(|f| f.name.to_lowercase());
    Ok(out)
}

pub struct Quota {
    pub used: u64,
    pub total: u64,
    pub owner: String,
}

pub fn quota(tok: &Token, drive_id: Option<&str>) -> Result<Quota, String> {
    let v = get(tok, &format!("{}?$select=quota,owner,driveType", drive_base(drive_id)))?;
    let owner = v["owner"]["user"]["email"]
        .as_str()
        .or(v["owner"]["user"]["displayName"].as_str())
        .or(v["owner"]["group"]["displayName"].as_str())
        .unwrap_or("")
        .to_string();
    Ok(Quota { used: v["quota"]["used"].as_u64().unwrap_or(0), total: v["quota"]["total"].as_u64().unwrap_or(0), owner })
}

/// The signed-in user's account type: "personal" or "business".
pub fn account_kind(tok: &Token) -> Result<String, String> {
    let v = get(tok, &format!("{GRAPH}/me/drive?$select=driveType"))?;
    Ok(if v["driveType"].as_str() == Some("personal") { "personal" } else { "business" }.into())
}

#[derive(Clone, Debug)]
pub struct Library {
    pub site: String,
    pub name: String,
    pub drive_id: String,
    pub url: String,
}

/// Finds document libraries on SharePoint sites matching `query` ("*" lists every site you can see).
pub fn find_libraries(tok: &Token, query: &str) -> Result<Vec<Library>, String> {
    let q = if query.trim().is_empty() { "*" } else { query.trim() };
    let sites = get_all(tok, format!("{GRAPH}/sites?search={}&$select=id,displayName,webUrl&$top=50", enc(q)))?;
    let mut out = Vec::new();
    for s in sites.iter().take(30) {
        let Some(id) = s["id"].as_str() else { continue };
        let site = s["displayName"].as_str().unwrap_or("").to_string();
        let Ok(drives) = get_all(tok, format!("{GRAPH}/sites/{}/drives?$select=id,name,webUrl", enc(id))) else { continue };
        for d in drives {
            out.push(Library {
                site: site.clone(),
                name: d["name"].as_str().unwrap_or("").to_string(),
                drive_id: d["id"].as_str().unwrap_or("").to_string(),
                url: d["webUrl"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    Ok(out)
}

pub fn human_size(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}
