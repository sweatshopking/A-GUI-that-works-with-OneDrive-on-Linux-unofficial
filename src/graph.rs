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

/// Test builds can send Microsoft's requests to a local stand-in server instead
/// (`LODG_TEST_MICROSOFT=http://127.0.0.1:port`). Release builds always use the real addresses.
fn at(url: &str) -> String {
    #[cfg(feature = "test-driver")]
    if let Ok(fake) = std::env::var("LODG_TEST_MICROSOFT") {
        for real in [
            "https://graph.microsoft.com",
            "https://login.microsoftonline.com",
        ] {
            if let Some(rest) = url.strip_prefix(real) {
                return format!("{fake}{rest}");
            }
        }
    }
    url.to_string()
}

/// Reads a JSON reply, explaining replies that aren't JSON (an outage page, an empty "too many
/// requests" answer) instead of passing on a parser error.
fn read_reply(resp: &mut ureq::http::Response<ureq::Body>) -> Result<Value, String> {
    let status = resp.status();
    resp.body_mut().read_json().map_err(|_| {
        if status.as_u16() == 429 {
            "Microsoft is busy right now (too many requests). Try again in a minute.".to_string()
        } else {
            format!("Microsoft sent an unexpected reply ({status}). Try again later.")
        }
    })
}

fn endpoints(cfg: &ClientConfig) -> (String, String) {
    let tenant = cfg
        .get("azure_tenant_id")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "common".into());
    let host = match cfg.get("azure_ad_endpoint").unwrap_or_default().as_str() {
        "USL4" | "USL5" => "https://login.microsoftonline.us",
        "DE" => "https://login.microsoftonline.de",
        "CN" => "https://login.chinacloudapi.cn",
        _ => "https://login.microsoftonline.com",
    };
    (
        format!("{host}/{tenant}/oauth2/v2.0/token"),
        format!("{host}/common/oauth2/nativeclient"),
    )
}

pub fn token(confdir: &Path) -> Result<Token, String> {
    let rt = std::fs::read_to_string(confdir.join("refresh_token"))
        .map_err(|_| "This account isn't signed in.".to_string())?;
    let cfg = ClientConfig::load(confdir);
    let app = cfg
        .get("application_id")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_APP_ID.into());
    let (url, redirect) = endpoints(&cfg);
    let mut resp = agent()
        .post(at(&url))
        .send_form([
            ("client_id", app.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", rt.trim()),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPE),
        ])
        .map_err(|e| format!("Couldn't reach Microsoft: {e}"))?;
    let v = read_reply(&mut resp)?;
    match v["access_token"].as_str() {
        Some(a) => Ok(Token {
            access: a.to_string(),
            expires: Instant::now()
                + Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120)),
        }),
        None => Err(format!(
            "Microsoft refused the saved sign-in ({}). Try signing in again.",
            v["error"].as_str().unwrap_or("unknown error")
        )),
    }
}

fn get(tok: &Token, url: &str) -> Result<Value, String> {
    let mut resp = agent()
        .get(at(url))
        .header("Authorization", &format!("Bearer {}", tok.access))
        .call()
        .map_err(|e| format!("Couldn't reach Microsoft: {e}"))?;
    let status = resp.status().as_u16();
    let v = read_reply(&mut resp)?;
    if status >= 400 {
        return Err(v["error"]["message"]
            .as_str()
            .unwrap_or("request failed")
            .to_string());
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
pub fn list_folders(
    tok: &Token,
    drive_id: Option<&str>,
    path: &str,
) -> Result<Vec<Folder>, String> {
    let base = drive_base(drive_id);
    let p = path.trim_matches('/');
    let item = if p.is_empty() {
        format!("{base}/root")
    } else {
        format!(
            "{base}/root:/{}:",
            p.split('/').map(enc).collect::<Vec<_>>().join("/")
        )
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
    let v = get(
        tok,
        &format!("{}?$select=quota,owner,driveType", drive_base(drive_id)),
    )?;
    let owner = v["owner"]["user"]["email"]
        .as_str()
        .or(v["owner"]["user"]["displayName"].as_str())
        .or(v["owner"]["group"]["displayName"].as_str())
        .unwrap_or("")
        .to_string();
    Ok(Quota {
        used: v["quota"]["used"].as_u64().unwrap_or(0),
        total: v["quota"]["total"].as_u64().unwrap_or(0),
        owner,
    })
}

/// The signed-in user's account type: "personal" or "business".
pub fn account_kind(tok: &Token) -> Result<String, String> {
    let v = get(tok, &format!("{GRAPH}/me/drive?$select=driveType"))?;
    Ok(if v["driveType"].as_str() == Some("personal") {
        "personal"
    } else {
        "business"
    }
    .into())
}

#[derive(Clone, Debug)]
pub struct Library {
    pub site: String,
    pub name: String,
    pub drive_id: String,
    pub web_url: String,
}

/// Finds document libraries on SharePoint sites matching `query` ("*" lists every site you can see).
/// Microsoft only returns sites the signed-in account is allowed to open.
pub fn find_libraries(tok: &Token, query: &str) -> Result<Vec<Library>, String> {
    let q = if query.trim().is_empty() {
        "*"
    } else {
        query.trim()
    };
    let sites = get_all(
        tok,
        format!(
            "{GRAPH}/sites?search={}&$select=id,displayName&$top=50",
            enc(q)
        ),
    )?;
    let mut out = Vec::new();
    for s in sites.iter().take(30) {
        let (Some(id), name) = (s["id"].as_str(), s["displayName"].as_str().unwrap_or("")) else {
            continue;
        };
        // sites you can see but not open (403) are skipped
        if let Ok(libs) = site_libraries(tok, id, name) {
            out.extend(libs);
        }
    }
    Ok(out)
}

fn site_libraries(tok: &Token, site_id: &str, site_name: &str) -> Result<Vec<Library>, String> {
    let drives = get_all(
        tok,
        format!(
            "{GRAPH}/sites/{}/drives?$select=id,name,webUrl",
            enc(site_id)
        ),
    )?;
    Ok(drives
        .iter()
        .map(|d| Library {
            site: site_name.to_string(),
            name: d["name"].as_str().unwrap_or("").to_string(),
            drive_id: d["id"].as_str().unwrap_or("").to_string(),
            web_url: d["webUrl"].as_str().unwrap_or("").to_string(),
        })
        .collect())
}

/// Splits a SharePoint address into (host, site path), e.g.
/// `https://contoso.sharepoint.com/sites/Team/Shared%20Documents/x` -> ("contoso.sharepoint.com", "/sites/Team").
/// The site path is empty for the root site.
pub fn parse_site_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !host.contains('.') {
        return None;
    }
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let site = match segs.as_slice() {
        [kind, name, ..]
            if ["sites", "teams", "personal"].contains(&kind.to_lowercase().as_str()) =>
        {
            format!("/{kind}/{name}")
        }
        _ => String::new(),
    };
    Some((host.to_lowercase(), site))
}

/// Libraries of the site at a pasted address, plus the index of the library the address points
/// into, if it points into one.
pub fn libraries_at_url(tok: &Token, url: &str) -> Result<(Vec<Library>, Option<usize>), String> {
    let (host, path) = parse_site_url(url)
        .ok_or_else(|| "That doesn't look like a SharePoint address. Copy it from your browser's address bar — it starts with https://".to_string())?;
    let site_url = if path.is_empty() {
        format!("{GRAPH}/sites/{host}")
    } else {
        format!("{GRAPH}/sites/{host}:{path}")
    };
    let site = get(tok, &format!("{site_url}?$select=id,displayName"))
        .map_err(|e| format!("Couldn't open that site with this account: {e}"))?;
    let id = site["id"]
        .as_str()
        .ok_or("Microsoft didn't return the site.")?;
    let libs = site_libraries(tok, id, site["displayName"].as_str().unwrap_or(""))?;
    let wanted = percent_decode(url).to_lowercase();
    let pick = libs.iter().position(|l| {
        !l.web_url.is_empty() && url_within(&wanted, &percent_decode(&l.web_url).to_lowercase())
    });
    Ok((libs, pick))
}

/// True when `url` is `base` or somewhere below it (whole path segments, so /Model isn't /Models).
fn url_within(url: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    url.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
}

fn percent_decode(s: &str) -> String {
    String::from_utf8_lossy(&crate::core::percent_decode(s)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_urls() {
        let p = |u: &str| parse_site_url(u);
        assert_eq!(
            p(
                "https://Contoso.sharepoint.com/sites/Team/Shared%20Documents/Forms/AllItems.aspx?id=1"
            ),
            Some(("contoso.sharepoint.com".into(), "/sites/Team".into()))
        );
        assert_eq!(
            p("https://contoso.sharepoint.com/teams/x"),
            Some(("contoso.sharepoint.com".into(), "/teams/x".into()))
        );
        assert_eq!(
            p("https://contoso.sharepoint.com/"),
            Some(("contoso.sharepoint.com".into(), String::new()))
        );
        assert_eq!(p("contoso"), None);
        assert_eq!(p("Marketing"), None);
    }

    #[test]
    fn library_matching() {
        assert!(url_within(
            "https://x/sites/a/model/forms/allitems.aspx",
            "https://x/sites/a/model"
        ));
        assert!(url_within(
            "https://x/sites/a/model",
            "https://x/sites/a/model/"
        ));
        assert!(!url_within(
            "https://x/sites/a/models/forms",
            "https://x/sites/a/model"
        ));
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("Shared%20Documents%2"), "Shared Documents%2");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }
}

pub fn human_size(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

fn item_url(drive_id: Option<&str>, rel: &str) -> String {
    let p = rel.trim_matches('/');
    let base = drive_base(drive_id);
    if p.is_empty() {
        format!("{base}/root")
    } else {
        format!(
            "{base}/root:/{}:",
            p.split('/').map(enc).collect::<Vec<_>>().join("/")
        )
    }
}

/// The OneDrive website address for a file or folder (`rel` relative to the account's folder).
pub fn item_web_url(tok: &Token, drive_id: Option<&str>, rel: &str) -> Result<String, String> {
    let v = get(tok, &format!("{}?$select=webUrl", item_url(drive_id, rel)))?;
    v["webUrl"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "OneDrive didn't return a web address.".into())
}

pub fn web_url(tok: &Token, drive_id: Option<&str>) -> Result<String, String> {
    item_web_url(tok, drive_id, "")
}

/// Who a sharing link works for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LinkScope {
    Anyone,
    Organization,
}

/// Creates a view-only sharing link. Tries "anyone with the link" first and falls back to
/// "people in your organisation" where the tenant doesn't allow anonymous links.
pub fn create_link(
    tok: &Token,
    drive_id: Option<&str>,
    rel: &str,
) -> Result<(String, LinkScope), String> {
    let url = format!("{}/createLink", item_url(drive_id, rel));
    let mut last = String::new();
    for (scope, kind) in [
        ("anonymous", LinkScope::Anyone),
        ("organization", LinkScope::Organization),
    ] {
        let mut resp = agent()
            .post(at(&url))
            .header("Authorization", &format!("Bearer {}", tok.access))
            .send_json(serde_json::json!({ "type": "view", "scope": scope }))
            .map_err(|e| format!("Couldn't reach Microsoft: {e}"))?;
        let status = resp.status().as_u16();
        let v = read_reply(&mut resp)?;
        if status < 400
            && let Some(u) = v["link"]["webUrl"].as_str()
        {
            // the service may quietly downgrade the scope, so trust what it reports
            let kind = match v["link"]["scope"].as_str() {
                Some("anonymous") => LinkScope::Anyone,
                Some(_) => LinkScope::Organization,
                None => kind,
            };
            return Ok((u.to_string(), kind));
        }
        last = v["error"]["message"]
            .as_str()
            .unwrap_or("couldn't create a link")
            .to_string();
    }
    Err(last)
}
