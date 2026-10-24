//! The Options tab's "Advanced" section: the rest of the client's `config` options, described
//! by one table so each gets the same loading, checking and saving.

use crate::core::ClientConfig;

pub enum Kind {
    Bool,
    /// A whole number in `min..=max`; `step` > 1 means it must be a multiple of it.
    Number {
        min: u64,
        max: u64,
        step: u64,
        unit: &'static str,
    },
    Text {
        placeholder: &'static str,
        check: fn(&str) -> Result<(), String>,
    },
    /// (value written to the config, words shown); the first is the client's default.
    Choice(&'static [(&'static str, &'static str)]),
}

/// What saving a change takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The sync engine restarts to pick it up.
    Restart,
    /// A full resync (with a preview first), like the * settings.
    Resync,
    /// Only takes effect after signing out and in again.
    SignIn,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    /// OneDrive for work or school and SharePoint libraries
    Work,
}

pub struct Spec {
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub kind: Kind,
    /// The client's own default, as written in the config file.
    pub default: &'static str,
    pub effect: Effect,
    pub scope: Scope,
    /// Can lose data or break syncing if misused: shown in the warning colour.
    pub risky: bool,
    /// Only used when this other (boolean) option is on.
    pub needs: Option<&'static str>,
}

const fn bool_opt(
    key: &'static str,
    group: &'static str,
    label: &'static str,
    hint: &'static str,
) -> Spec {
    Spec {
        key,
        group,
        label,
        hint,
        kind: Kind::Bool,
        default: "false",
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    }
}

#[allow(clippy::too_many_arguments)] // a table row: named fields would be longer
const fn num(
    key: &'static str,
    group: &'static str,
    label: &'static str,
    hint: &'static str,
    default: &'static str,
    min: u64,
    max: u64,
    unit: &'static str,
) -> Spec {
    Spec {
        key,
        group,
        label,
        hint,
        kind: Kind::Number {
            min,
            max,
            step: 1,
            unit,
        },
        default,
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    }
}

const fn risky(mut s: Spec) -> Spec {
    s.risky = true;
    s
}

const fn needs(mut s: Spec, key: &'static str) -> Spec {
    s.needs = Some(key);
    s
}

const fn effect(mut s: Spec, e: Effect) -> Spec {
    s.effect = e;
    s
}

const fn work_only(mut s: Spec) -> Spec {
    s.scope = Scope::Work;
    s
}

fn check_abs_path(v: &str) -> Result<(), String> {
    if v.starts_with('/') || v.starts_with("~/") {
        Ok(())
    } else {
        Err("needs a full path, starting with / or ~/".into())
    }
}

fn check_https_url(v: &str) -> Result<(), String> {
    match v.strip_prefix("https://") {
        Some(rest) if rest.len() > 2 && !rest.contains(char::is_whitespace) => Ok(()),
        _ => Err("needs a public https:// address".into()),
    }
}

fn check_host(v: &str) -> Result<(), String> {
    if v.parse::<std::net::IpAddr>().is_ok() {
        Ok(())
    } else {
        Err("needs an IP address, like 0.0.0.0 or 127.0.0.1".into())
    }
}

pub fn check_tenant(v: &str) -> Result<(), String> {
    let ok = v
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if ok {
        Ok(())
    } else {
        Err("needs a tenant ID (a GUID) or a domain like contoso.onmicrosoft.com".into())
    }
}

fn check_app_id(v: &str) -> Result<(), String> {
    let parts: Vec<&str> = v.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    let ok = parts.len() == 5
        && parts
            .iter()
            .zip(lens)
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(())
    } else {
        Err("needs an application ID like d50ca740-c83f-4d1b-b616-12c519384f0c".into())
    }
}

fn check_any(v: &str) -> Result<(), String> {
    if v.contains('"') || v.contains('\n') {
        Err("can't contain quotes or line breaks".into())
    } else {
        Ok(())
    }
}

/// Unix permissions as the client takes them: three octal digits.
fn check_perms(v: &str) -> Result<(), String> {
    if v.len() == 3 && v.chars().all(|c| ('0'..='7').contains(&c)) {
        Ok(())
    } else {
        Err("needs three digits from 0 to 7, like 700 or 644".into())
    }
}

/// The big-delete threshold has its own field in the Safety card; it's listed here too so it's
/// checked and saved the same way.
pub const BIG_DELETE: Spec = num(
    "classify_as_big_delete",
    "Safety",
    "Stop and ask when this many items are deleted at once",
    "",
    "1000",
    1,
    1_000_000,
    "items",
);

/// Microsoft's clouds: (azure_ad_endpoint value, words shown); national ones need a tenant.
pub const CLOUDS: &[(&str, &str)] = &[
    ("", "Worldwide (normal)"),
    ("USL4", "US Government (GCC High)"),
    ("USL5", "US Government (DoD)"),
    ("CN", "China (21Vianet)"),
];

pub const SPECS: &[Spec] = &[
    // ---- syncing
    num(
        "monitor_fullscan_frequency",
        "Syncing",
        "Full online check every",
        "Counted in sync checks (set under When to sync). Lower catches missed online changes sooner but uses more data.",
        "12",
        1,
        1000,
        "checks",
    ),
    risky(bool_opt(
        "local_first",
        "Syncing",
        "Treat this computer as the source of truth",
        "When the same item differs here and online, this computer's copy wins instead of OneDrive's.",
    )),
    risky(needs(
        bool_opt(
            "mirror_local_state",
            "Syncing",
            "Make OneDrive match this computer exactly",
            "Items online that aren't on this computer are deleted online instead of downloaded. Check with a dry run first.",
        ),
        "local_first",
    )),
    risky(bool_opt(
        "remove_source_files",
        "Syncing",
        "Delete files here after they upload",
        "For moving files to OneDrive, usually with Upload only. Leaves the folders behind.",
    )),
    risky(needs(
        bool_opt(
            "remove_source_folders",
            "Syncing",
            "Also delete the emptied folders",
            "",
        ),
        "remove_source_files",
    )),
    effect(
        bool_opt(
            "sync_root_files",
            "Syncing",
            "Always sync files at the top of the OneDrive folder",
            "When only some folders are chosen, files sitting directly in the OneDrive folder still sync.",
        ),
        Effect::Resync,
    ),
    Spec {
        key: "transfer_order",
        group: "Syncing",
        label: "Transfer files in this order",
        hint: "",
        kind: Kind::Choice(&[
            ("default", "As found"),
            ("size_asc", "Smallest first"),
            ("size_dsc", "Largest first"),
            ("name_asc", "By name, A to Z"),
            ("name_dsc", "By name, Z to A"),
        ]),
        default: "default",
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    bool_opt(
        "delay_inotify_processing",
        "Syncing",
        "Wait before sending changes from busy apps",
        "Helps with apps that save on every keystroke (Obsidian and other note apps).",
    ),
    needs(
        num(
            "inotify_delay",
            "Syncing",
            "Wait",
            "",
            "5",
            1,
            15,
            "seconds",
        ),
        "delay_inotify_processing",
    ),
    bool_opt(
        "check_nomount",
        "Syncing",
        "Don't sync if the drive isn't mounted",
        "Put an empty file named .nosync in the mount point while the drive is unplugged. If it's ever visible, syncing stops instead of deleting everything online.",
    ),
    // ---- files
    effect(
        num(
            "skip_size",
            "Files",
            "Don't sync files larger than",
            "0 syncs files of any size.",
            "0",
            0,
            250_000,
            "MB",
        ),
        Effect::Resync,
    ),
    effect(
        bool_opt(
            "skip_dir_strict_match",
            "Files",
            "Folders to skip must match the whole path",
            "Then \"Projects/Old\" only skips that one folder, and a plain name only skips a folder at the top.",
        ),
        Effect::Resync,
    ),
    risky(bool_opt(
        "permanent_delete",
        "Files",
        "Delete permanently online (skip the OneDrive recycle bin)",
        "Items deleted here can't be restored online afterwards.",
    )),
    risky(bool_opt(
        "bypass_data_preservation",
        "Files",
        "Overwrite local files on a conflict instead of keeping a copy",
        "Normally a clashing local file is kept under a new name. With this on, its changes are lost.",
    )),
    needs(
        Spec {
            key: "recycle_bin_path",
            group: "Files",
            label: "Trash folder",
            hint: "Where files deleted online go on this computer.",
            kind: Kind::Text {
                placeholder: "~/.local/share/Trash/",
                check: check_abs_path,
            },
            default: "",
            effect: Effect::Restart,
            scope: Scope::All,
            risky: false,
            needs: None,
        },
        "use_recycle_bin",
    ),
    bool_opt(
        "disable_permission_set",
        "Files",
        "Don't set permissions on downloaded files",
        "Leaves permissions to the folder (needed on drives that don't support Linux permissions, like NTFS or exFAT).",
    ),
    Spec {
        key: "sync_dir_permissions",
        group: "Files",
        label: "Permissions for new folders",
        hint: "700 means only you can open them.",
        kind: Kind::Text {
            placeholder: "700",
            check: check_perms,
        },
        default: "700",
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    Spec {
        key: "sync_file_permissions",
        group: "Files",
        label: "Permissions for new files",
        hint: "600 means only you can read them.",
        kind: Kind::Text {
            placeholder: "600",
            check: check_perms,
        },
        default: "600",
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    bool_opt(
        "write_xattr_data",
        "Files",
        "Record who created and last changed each file",
        "Stored in the file's extended attributes (user.onedrive.createdBy, user.onedrive.lastModifiedBy).",
    ),
    bool_opt(
        "display_manager_integration",
        "Files",
        "Add the OneDrive folder to the file manager's sidebar",
        "With a OneDrive folder icon where the file manager supports it.",
    ),
    num(
        "space_reservation",
        "Files",
        "Keep this much disk space free",
        "Downloads stop rather than fill the disk.",
        "50",
        0,
        1_000_000,
        "MB",
    ),
    // ---- uploads and downloads
    num(
        "threads",
        "Transfers",
        "Files transferred at once",
        "More than 8 rarely helps and can get you throttled by Microsoft.",
        "8",
        1,
        16,
        "",
    ),
    Spec {
        key: "file_fragment_size",
        group: "Transfers",
        label: "Upload large files in pieces of",
        hint: "10 to 60 MB, in steps of 5. Bigger pieces can be faster on fast, steady connections.",
        kind: Kind::Number {
            min: 10,
            max: 60,
            step: 5,
            unit: "MB",
        },
        default: "10",
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    risky(bool_opt(
        "disable_upload_validation",
        "Transfers",
        "Don't check uploads",
        "Only for SharePoint libraries that change files after upload, which makes every check fail.",
    )),
    risky(bool_opt(
        "disable_download_validation",
        "Transfers",
        "Don't check downloads",
        "Only when SharePoint reports the wrong size for files, so good downloads get thrown away.",
    )),
    work_only(bool_opt(
        "create_new_file_version",
        "Transfers",
        "Keep SharePoint's changed copy as a new version",
        "SharePoint rewrites Office, PDF and HTML files after upload. Normally the client puts your original back.",
    )),
    bool_opt(
        "force_session_upload",
        "Transfers",
        "Upload every file in pieces",
        "Keeps file dates exactly right online, at the cost of a little speed for small files.",
    ),
    // ---- network
    num(
        "connect_timeout",
        "Network",
        "Give up connecting after",
        "",
        "10",
        1,
        600,
        "seconds",
    ),
    num(
        "data_timeout",
        "Network",
        "Give up when no data arrives for",
        "",
        "60",
        1,
        3600,
        "seconds",
    ),
    num(
        "operation_timeout",
        "Network",
        "Give up on any single request after",
        "0 means never. Too low cuts off large transfers.",
        "0",
        0,
        86_400,
        "seconds",
    ),
    num(
        "dns_timeout",
        "Network",
        "Remember server addresses for",
        "",
        "60",
        0,
        3600,
        "seconds",
    ),
    num(
        "max_curl_idle",
        "Network",
        "Drop idle connections after",
        "Lower this if a router or firewall cuts connections that sit idle.",
        "120",
        1,
        3600,
        "seconds",
    ),
    bool_opt(
        "force_http_11",
        "Network",
        "Use HTTP/1.1 only",
        "Can fix transfers that stall on some networks and proxies.",
    ),
    bool_opt(
        "disable_websocket_support",
        "Network",
        "Don't get instant online changes",
        "Online changes are then picked up at each sync check instead.",
    ),
    Spec {
        key: "user_agent",
        group: "Network",
        label: "Identify as",
        hint: "Leave empty for the client's own name, which Microsoft throttles least.",
        kind: Kind::Text {
            placeholder: "ISV|abraunegg|OneDrive Client for Linux",
            check: check_any,
        },
        default: "",
        effect: Effect::Restart,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    // ---- webhooks (online changes pushed to a public address)
    work_only(bool_opt(
        "webhook_enabled",
        "Webhooks",
        "Get online changes through a webhook",
        "Needs a public https address that forwards to this computer. Instant online changes usually make this unnecessary.",
    )),
    work_only(needs(
        Spec {
            key: "webhook_public_url",
            group: "Webhooks",
            label: "Public address",
            hint: "",
            kind: Kind::Text {
                placeholder: "https://example.com/webhooks/onedrive",
                check: check_https_url,
            },
            default: "",
            effect: Effect::Restart,
            scope: Scope::All,
            risky: false,
            needs: None,
        },
        "webhook_enabled",
    )),
    work_only(needs(
        Spec {
            key: "webhook_listening_host",
            group: "Webhooks",
            label: "Listen on address",
            hint: "",
            kind: Kind::Text {
                placeholder: "0.0.0.0",
                check: check_host,
            },
            default: "",
            effect: Effect::Restart,
            scope: Scope::All,
            risky: false,
            needs: None,
        },
        "webhook_enabled",
    )),
    work_only(needs(
        num(
            "webhook_listening_port",
            "Webhooks",
            "Listen on port",
            "",
            "8888",
            1,
            65_535,
            "",
        ),
        "webhook_enabled",
    )),
    work_only(needs(
        num(
            "webhook_expiration_interval",
            "Webhooks",
            "Subscription lasts",
            "",
            "600",
            60,
            259_200,
            "seconds",
        ),
        "webhook_enabled",
    )),
    work_only(needs(
        num(
            "webhook_renewal_interval",
            "Webhooks",
            "Renew the subscription every",
            "",
            "300",
            30,
            259_200,
            "seconds",
        ),
        "webhook_enabled",
    )),
    work_only(needs(
        num(
            "webhook_retry_interval",
            "Webhooks",
            "Retry a failed subscription after",
            "",
            "60",
            10,
            86_400,
            "seconds",
        ),
        "webhook_enabled",
    )),
    // ---- logging
    bool_opt(
        "enable_logging",
        "Logging",
        "Write a log file",
        "In addition to the system journal the app already reads.",
    ),
    needs(
        Spec {
            key: "log_dir",
            group: "Logging",
            label: "Log folder",
            hint: "Leave empty for the app's own folder (~/.local/share/linux-onedrive-gui/logs).",
            kind: Kind::Text {
                placeholder: "~/.local/share/linux-onedrive-gui/logs",
                check: check_abs_path,
            },
            default: "",
            effect: Effect::Restart,
            scope: Scope::All,
            risky: false,
            needs: None,
        },
        "enable_logging",
    ),
    num(
        "monitor_log_frequency",
        "Logging",
        "Log routine checks only every",
        "Quiets the log when nothing is changing.",
        "12",
        1,
        1000,
        "checks",
    ),
    bool_opt(
        "notify_file_actions",
        "Logging",
        "Notify about every file synced",
        "",
    ),
    bool_opt(
        "display_transfer_metrics",
        "Logging",
        "Log the speed of each transfer",
        "",
    ),
    risky(bool_opt(
        "debug_https",
        "Logging",
        "Log all network traffic",
        "Only for reporting a problem. The log then holds sign-in tokens: don't share it without removing them.",
    )),
    bool_opt(
        "disable_version_check",
        "Logging",
        "Don't check for new client versions",
        "",
    ),
    // ---- sign-in (changing these needs signing out and in again)
    Spec {
        key: "azure_ad_endpoint",
        group: "Sign-in",
        label: "Microsoft cloud",
        hint: "Only for government or national cloud accounts.",
        kind: Kind::Choice(CLOUDS),
        default: "",
        effect: Effect::SignIn,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    Spec {
        key: "azure_tenant_id",
        group: "Sign-in",
        label: "Tenant",
        hint: "Required for a national cloud; otherwise only if your organisation asks.",
        kind: Kind::Text {
            placeholder: "contoso.onmicrosoft.com",
            check: check_tenant,
        },
        default: "",
        effect: Effect::SignIn,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    Spec {
        key: "application_id",
        group: "Sign-in",
        label: "Application ID",
        hint: "Only if your organisation registered its own app for the client.",
        kind: Kind::Text {
            placeholder: "d50ca740-c83f-4d1b-b616-12c519384f0c",
            check: check_app_id,
        },
        default: "",
        effect: Effect::SignIn,
        scope: Scope::All,
        risky: false,
        needs: None,
    },
    effect(
        bool_opt(
            "read_only_auth_scope",
            "Sign-in",
            "Ask for read-only access",
            "The client can then only download. Uploads and online deletes fail.",
        ),
        Effect::SignIn,
    ),
];

/// Whether the option applies to this kind of account ("personal", "business", "sharepoint").
pub fn applies(s: &Spec, kind: &str) -> bool {
    match s.scope {
        Scope::All => true,
        Scope::Work => kind == "business" || kind == "sharepoint",
    }
}

/// The value the UI shows: the config's value, or the client's default.
pub fn current(s: &Spec, cfg: &ClientConfig) -> String {
    match s.kind {
        Kind::Bool => cfg.get_bool(s.key).to_string(),
        _ => cfg
            .get(s.key)
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| s.default.to_string()),
    }
}

/// Checks a value typed or picked in the UI; returns it as it should be written.
pub fn check(s: &Spec, value: &str) -> Result<String, String> {
    let v = value.trim();
    match &s.kind {
        Kind::Bool => Ok(if v == "true" { "true" } else { "false" }.into()),
        Kind::Number {
            min,
            max,
            step,
            unit,
        } => {
            let unit = if unit.is_empty() {
                String::new()
            } else {
                format!(" {unit}")
            };
            let n: u64 = v
                .parse()
                .map_err(|_| format!("needs a whole number from {min} to {max}{unit}"))?;
            if n < *min || n > *max {
                return Err(format!("needs a number from {min} to {max}{unit}"));
            }
            if *step > 1 && !n.is_multiple_of(*step) {
                return Err(format!("needs a multiple of {step}"));
            }
            Ok(n.to_string())
        }
        Kind::Text { check, .. } => {
            if v.is_empty() {
                Ok(String::new())
            } else {
                check(v).map(|_| v.to_string())
            }
        }
        Kind::Choice(list) => {
            if list.iter().any(|(val, _)| *val == v) {
                Ok(v.to_string())
            } else {
                Err("isn't one of the choices".into())
            }
        }
    }
}

/// What to write for a checked value: None when nothing needs to change, else the new config
/// value (None inside = remove the key, so the client's default applies again).
pub fn change(s: &Spec, cfg: &ClientConfig, value: &str) -> Option<Option<String>> {
    if value == current(s, cfg) {
        return None;
    }
    let is_default = match s.kind {
        Kind::Bool => value == "false",
        _ => value == s.default || value.is_empty(),
    };
    Some((!is_default).then(|| value.to_string()))
}

/// A `~/` path as the client wants it: it doesn't expand `~` in every option.
pub fn expand_home(v: &str) -> String {
    match (v.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.to_string_lossy()),
        _ => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(key: &str) -> &'static Spec {
        SPECS.iter().find(|s| s.key == key).unwrap()
    }

    #[test]
    fn keys_unique_and_not_in_main_options() {
        let main = [
            "monitor_interval",
            "rate_limit",
            "ip_protocol_version",
            "use_recycle_bin",
            "disable_notifications",
            "skip_dotfiles",
            "skip_file",
            "skip_dir",
            "skip_symlinks",
            "check_nosync",
            "sync_business_shared_items",
            "download_only",
            "upload_only",
            "cleanup_local_files",
            "no_remote_delete",
            "classify_as_big_delete",
        ];
        let mut seen = std::collections::HashSet::new();
        for s in SPECS {
            assert!(seen.insert(s.key), "duplicate {}", s.key);
            assert!(!main.contains(&s.key), "{} is on the main page", s.key);
            if let Some(n) = s.needs {
                assert!(
                    n == "use_recycle_bin" || SPECS.iter().any(|o| o.key == n),
                    "{} needs unknown {n}",
                    s.key
                );
            }
            // defaults must pass their own check
            if !matches!(s.kind, Kind::Text { .. }) || !s.default.is_empty() {
                assert_eq!(check(s, s.default).as_deref(), Ok(s.default), "{}", s.key);
            }
        }
    }

    #[test]
    fn numbers() {
        let f = spec("file_fragment_size");
        assert_eq!(check(f, " 25 ").as_deref(), Ok("25"));
        assert!(check(f, "12").is_err());
        assert!(check(f, "65").is_err());
        assert!(check(f, "abc").is_err());
        assert!(check(&BIG_DELETE, "0").is_err());
        assert_eq!(check(&BIG_DELETE, "5000").as_deref(), Ok("5000"));
    }

    #[test]
    fn texts() {
        assert!(check(spec("sync_file_permissions"), "644").is_ok());
        assert!(check(spec("sync_file_permissions"), "0644").is_err());
        assert!(check(spec("sync_file_permissions"), "689").is_err());
        assert!(check(spec("webhook_public_url"), "http://x.com/h").is_err());
        assert!(check(spec("webhook_public_url"), "https://x.com/h").is_ok());
        assert!(check(spec("webhook_listening_host"), "localhost").is_err());
        assert!(
            check(
                spec("application_id"),
                "d50ca740-c83f-4d1b-b616-12c519384f0c"
            )
            .is_ok()
        );
        assert!(check(spec("application_id"), "nope").is_err());
        assert!(check(spec("log_dir"), "logs").is_err());
        assert_eq!(check(spec("log_dir"), "  ").as_deref(), Ok(""));
        assert!(check(spec("user_agent"), "a\"b").is_err());
        assert!(check(spec("transfer_order"), "size_asc").is_ok());
        assert!(check(spec("transfer_order"), "random").is_err());
    }

    #[test]
    fn changes() {
        let dir = std::env::temp_dir().join(format!("lodg-adv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config"),
            "threads = \"8\"\nlocal_first = \"true\"\n",
        )
        .unwrap();
        let cfg = ClientConfig::load(&dir);
        // unchanged, even though it's the default and set explicitly
        assert_eq!(change(spec("threads"), &cfg, "8"), None);
        assert_eq!(change(spec("threads"), &cfg, "4"), Some(Some("4".into())));
        // back to the default removes the key
        assert_eq!(change(spec("local_first"), &cfg, "false"), Some(None));
        assert_eq!(change(spec("local_first"), &cfg, "true"), None);
        assert_eq!(change(spec("transfer_order"), &cfg, "default"), None);
        assert_eq!(
            change(spec("transfer_order"), &cfg, "size_asc"),
            Some(Some("size_asc".into()))
        );
        assert_eq!(change(spec("log_dir"), &cfg, ""), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope() {
        assert!(!applies(spec("webhook_enabled"), "personal"));
        assert!(applies(spec("webhook_enabled"), "sharepoint"));
        assert!(applies(spec("threads"), "personal"));
    }
}
