//! Telling the user when a newer version is out: a once-a-day look at the project's latest
//! release on GitHub. Nothing is downloaded or installed; the user gets a link to the release.
//! Switched off with "Tell me when a new version is out" in App settings.

use std::time::Duration;

pub const RELEASES_URL: &str = "https://github.com/sweatshopking/DriveDeck/releases/latest";
const API_URL: &str = "https://api.github.com/repos/sweatshopking/DriveDeck/releases/latest";

/// "1.2.3" or "v1.2.3" as numbers; anything else (a pre-release tag, say) is None.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let mut it = v.split('.');
    let t = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(t)
}

/// Whether `latest` is a newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// The latest release's version ("0.3.6"); Ok(None) when GitHub answered but has no ordinary
/// release to offer (none yet, or a pre-release); Err when it couldn't be asked (offline, busy).
pub fn latest_version() -> Result<Option<String>, ()> {
    #[cfg(feature = "test-driver")]
    let url = std::env::var("LODG_TEST_RELEASES").unwrap_or_else(|_| API_URL.into());
    #[cfg(not(feature = "test-driver"))]
    let url = API_URL.to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(&url)
        // GitHub asks for one; the version tells nothing about the user
        .header("User-Agent", "drivedeck")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|_| ())?;
    match resp.status().as_u16() {
        200 => {}
        // no release yet
        404 => return Ok(None),
        _ => return Err(()),
    }
    let v: serde_json::Value = resp.body_mut().read_json().map_err(|_| ())?;
    // drafts and pre-releases aren't offered
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    Ok(v["tag_name"]
        .as_str()
        .and_then(parse)
        .map(|(a, b, c)| format!("{a}.{b}.{c}")))
}

/// The newer version, if there is one (None also when GitHub couldn't be asked).
pub fn newer_version() -> Option<String> {
    latest_version()
        .ok()
        .flatten()
        .filter(|l| is_newer(l, env!("CARGO_PKG_VERSION")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.3.6", "0.3.5"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("v0.3.5", "0.3.5"));
        assert!(!is_newer("0.3.4", "0.3.5"));
        assert!(!is_newer("v0.4.0-beta", "0.3.5"));
        assert!(!is_newer("nonsense", "0.3.5"));
        assert!(!is_newer("0.4", "0.3.5"));
    }
}
