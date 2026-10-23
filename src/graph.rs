//! Read-only Microsoft Graph calls for things the client has no command for: browsing online
//! folders (including ones not synced yet), finding SharePoint libraries and reading quota.
//!
//! We borrow the client's saved sign-in (refresh_token) to get a short-lived access token. The
//! refresh token file is only ever read, never rewritten, so the client's own sign-in is unaffected.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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

fn unreachable(e: ureq::Error) -> String {
    format!("Couldn't reach Microsoft. Check your internet connection and try again. ({e})")
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
        .map_err(unreachable)?;
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
        .map_err(unreachable)?;
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
    // a server that keeps sending next links (or the same one again) mustn't hold us forever
    const MAX_PAGES: usize = 100;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..MAX_PAGES {
        if !seen.insert(url.clone()) {
            return Err("Microsoft repeated a page while listing items. Try again later.".into());
        }
        let v = get(tok, &url)?;
        if let Some(items) = v["value"].as_array() {
            out.extend(items.iter().cloned());
        }
        match v["@odata.nextLink"].as_str() {
            // the next page carries the sign-in too, so it may only go to Microsoft Graph
            Some(next) if is_graph(next) => url = next.to_string(),
            Some(_) => return Err("Microsoft sent an unexpected reply. Try again later.".into()),
            None => return Ok(out),
        }
    }
    Err("Microsoft sent too many pages for this list. Try again later.".into())
}

fn is_graph(url: &str) -> bool {
    #[cfg(feature = "test-driver")]
    if let Ok(fake) = std::env::var("LODG_TEST_MICROSOFT")
        && url.starts_with(&fake)
    {
        return true;
    }
    url.starts_with("https://graph.microsoft.com/")
}

/// An address from Microsoft that is safe to hand to the browser: only https.
fn web_address(url: &str) -> Result<String, String> {
    if url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        Err("OneDrive didn't return a web address.".into())
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
    drives
        .iter()
        .map(|d| {
            Ok(Library {
                site: site_name.to_string(),
                name: d["name"].as_str().unwrap_or("").to_string(),
                drive_id: d["id"]
                    .as_str()
                    .ok_or_else(|| "Microsoft returned a library without an ID.".to_string())?
                    .to_string(),
                web_url: d["webUrl"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
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
    web_address(v["webUrl"].as_str().unwrap_or(""))
}

pub fn web_url(tok: &Token, drive_id: Option<&str>) -> Result<String, String> {
    item_web_url(tok, drive_id, "")
}

/// Pages of the OneDrive website the Windows app links to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WebPage {
    Home,
    RecycleBin,
    /// "Restore your OneDrive": roll everything back to an earlier time (personal accounts)
    Restore,
}

/// The SharePoint site a work, school or SharePoint library address belongs to: the address
/// without its last part (the library). None for a personal account's address.
fn sharepoint_site(library_url: &str) -> Option<String> {
    let rest = library_url.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    if !host.contains(".sharepoint.") {
        return None;
    }
    let trimmed = library_url.trim_end_matches('/');
    let (site, _library) = trimmed.rsplit_once('/')?;
    (site.len() > "https://".len() + host.len())
        .then(|| site.to_string())
        .or_else(|| Some(format!("https://{host}")))
}

/// The address of a page of the OneDrive website for an account.
pub fn web_page(tok: &Token, drive_id: Option<&str>, page: WebPage) -> Result<String, String> {
    let home = web_url(tok, drive_id)?;
    Ok(web_page_for(&home, page))
}

fn web_page_for(home: &str, page: WebPage) -> String {
    match (page, sharepoint_site(home)) {
        (WebPage::Home, _) => home.to_string(),
        (WebPage::RecycleBin, Some(site)) => format!("{site}/_layouts/15/RecycleBin.aspx"),
        // no restore page of its own for work or school: the recycle bin is where to start
        (WebPage::Restore, Some(site)) => format!("{site}/_layouts/15/RecycleBin.aspx"),
        (WebPage::RecycleBin, None) => "https://onedrive.live.com/?v=recyclebin".into(),
        (WebPage::Restore, None) => "https://onedrive.live.com/?v=restore".into(),
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;

    #[test]
    fn pages() {
        let b = "https://contoso-my.sharepoint.com/personal/jo_contoso_com/Documents";
        assert_eq!(
            web_page_for(b, WebPage::RecycleBin),
            "https://contoso-my.sharepoint.com/personal/jo_contoso_com/_layouts/15/RecycleBin.aspx"
        );
        let sp = "https://contoso.sharepoint.com/sites/Team/Shared%20Documents";
        assert_eq!(
            web_page_for(sp, WebPage::RecycleBin),
            "https://contoso.sharepoint.com/sites/Team/_layouts/15/RecycleBin.aspx"
        );
        let root = "https://contoso.sharepoint.com/Shared%20Documents";
        assert_eq!(
            web_page_for(root, WebPage::RecycleBin),
            "https://contoso.sharepoint.com/_layouts/15/RecycleBin.aspx"
        );
        let p = "https://onedrive.live.com/?cid=abc";
        assert!(web_page_for(p, WebPage::RecycleBin).starts_with("https://onedrive.live.com/"));
        assert_eq!(web_page_for(p, WebPage::Home), p);
    }
}

/// One saved version of a file, newest first in `versions`.
#[derive(Clone, Debug)]
pub struct Version {
    pub id: String,
    /// RFC 3339
    pub modified: String,
    pub size: u64,
    pub by: String,
}

/// A file's saved versions, newest (the current one) first.
pub fn versions(tok: &Token, drive_id: Option<&str>, rel: &str) -> Result<Vec<Version>, String> {
    let items = get_all(
        tok,
        format!(
            "{}/versions?$select=id,lastModifiedDateTime,size,lastModifiedBy",
            item_url(drive_id, rel)
        ),
    )?;
    let mut out: Vec<Version> = items
        .iter()
        .map(|v| Version {
            id: v["id"].as_str().unwrap_or("").to_string(),
            modified: v["lastModifiedDateTime"].as_str().unwrap_or("").to_string(),
            size: v["size"].as_u64().unwrap_or(0),
            by: v["lastModifiedBy"]["user"]["displayName"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        })
        .filter(|v| !v.id.is_empty())
        .collect();
    // RFC 3339 times in UTC sort as text
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(out)
}

fn version_url(drive_id: Option<&str>, rel: &str, id: &str) -> String {
    format!("{}/versions/{}", item_url(drive_id, rel), enc(id))
}

/// Saves an old version of a file to `dest`.
pub fn download_version(
    tok: &Token,
    drive_id: Option<&str>,
    rel: &str,
    id: &str,
    dest: &Path,
) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        if meta.file_type().is_file() {
            return Ok(()); // a completed cached copy is safe to reuse
        }
        return Err(format!(
            "{} already exists and isn't a regular file.",
            dest.display()
        ));
    }
    let resp = agent()
        .get(at(&format!("{}/content", version_url(drive_id, rel, id))))
        .header("Authorization", &format!("Bearer {}", tok.access))
        .call()
        .map_err(unreachable)?;
    if resp.status().as_u16() >= 400 {
        return Err(format!(
            "Microsoft wouldn't send that version ({}).",
            resp.status()
        ));
    }
    static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
    let parent = dest
        .parent()
        .ok_or_else(|| "Couldn't find where to save that version.".to_string())?;
    let base = dest
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut temp = None;
    for _ in 0..32 {
        let n = NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{base}.download-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                temp = Some((path, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't save it: {e}")),
        }
    }
    let (temp_path, mut file) = temp.ok_or_else(|| {
        "Couldn't allocate a temporary file for that version download.".to_string()
    })?;
    let result = (|| {
        std::io::copy(&mut resp.into_body().into_reader(), &mut file)
            .map_err(|e| format!("The download stopped: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("Couldn't finish saving that version: {e}"))?;
        drop(file);
        match crate::core::rename_noreplace(&temp_path, dest) {
            Ok(()) => Ok(()),
            // Another request completed this same version at the same time.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::symlink_metadata(dest).is_ok_and(|m| m.file_type().is_file()) {
                    Ok(())
                } else {
                    Err(format!(
                        "{} already exists and isn't a regular file.",
                        dest.display()
                    ))
                }
            }
            Err(e) => Err(format!("Couldn't finish saving that version: {e}")),
        }
    })();
    let _ = std::fs::remove_file(&temp_path);
    result
}

/// Sends a POST and reads the reply; an empty reply (204) is fine.
fn post(tok: &Token, url: &str, body: Option<Value>) -> Result<Value, String> {
    let req = agent()
        .post(at(url))
        .header("Authorization", &format!("Bearer {}", tok.access));
    let mut resp = match body {
        Some(b) => req.send_json(b),
        None => req.header("Content-Length", "0").send_empty(),
    }
    .map_err(unreachable)?;
    let status = resp.status().as_u16();
    if status == 204 {
        return Ok(Value::Null);
    }
    let v = read_reply(&mut resp).unwrap_or(Value::Null);
    if status >= 400 {
        return Err(v["error"]["message"]
            .as_str()
            .unwrap_or("Microsoft turned the request down.")
            .to_string());
    }
    Ok(v)
}

/// Makes an old version the current one again (the current one becomes a version itself).
pub fn restore_version(
    tok: &Token,
    drive_id: Option<&str>,
    rel: &str,
    id: &str,
) -> Result<(), String> {
    post(
        tok,
        &format!("{}/restoreVersion", version_url(drive_id, rel, id)),
        None,
    )
    .map(|_| ())
}

/// Shares a file or folder with particular people by email: they get an invitation and must
/// sign in to open it.
pub fn invite(
    tok: &Token,
    drive_id: Option<&str>,
    rel: &str,
    emails: &[String],
    edit: bool,
    message: &str,
) -> Result<(), String> {
    let recipients: Vec<Value> = emails
        .iter()
        .map(|e| serde_json::json!({ "email": e }))
        .collect();
    let mut body = serde_json::json!({
        "recipients": recipients,
        "requireSignIn": true,
        "sendInvitation": true,
        "roles": [if edit { "write" } else { "read" }],
    });
    if !message.trim().is_empty() {
        body["message"] = Value::String(message.trim().chars().take(2000).collect());
    }
    post(
        tok,
        &format!("{}/invite", item_url(drive_id, rel)),
        Some(body),
    )
    .map(|_| ())
}

/// Who a sharing link works for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LinkScope {
    Anyone,
    Organization,
}

/// Creates a sharing link, view-only or for editing. Personal accounts get "anyone with the
/// link"; work, school and SharePoint accounts only ever get "people in your organization".
/// When the account's kind isn't known yet it is looked up first: a failed organization link
/// doesn't prove an account is personal, so we never fall back to a public link.
pub fn create_link(
    tok: &Token,
    drive_id: Option<&str>,
    rel: &str,
    account_kind: &str,
    edit: bool,
) -> Result<(String, LinkScope), String> {
    let url = format!("{}/createLink", item_url(drive_id, rel));
    let looked_up;
    let account_kind = match account_kind {
        "personal" | "business" | "sharepoint" => account_kind,
        _ => {
            looked_up = crate::graph::account_kind(tok).map_err(|e| {
                format!("Couldn't check what kind of account this is, so no link was made: {e}")
            })?;
            looked_up.as_str()
        }
    };
    let scopes: &[(&str, LinkScope)] = match account_kind {
        "personal" => &[("anonymous", LinkScope::Anyone)],
        _ => &[("organization", LinkScope::Organization)],
    };
    let mut last = String::new();
    for &(scope, kind) in scopes {
        let mut resp = agent()
            .post(at(&url))
            .header("Authorization", &format!("Bearer {}", tok.access))
            .send_json(serde_json::json!({
                "type": if edit { "edit" } else { "view" },
                "scope": scope
            }))
            .map_err(unreachable)?;
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
