//! Installing (and on Arch, updating) the OneDrive Client for Linux for people who'd rather not
//! use a terminal. Always a real package, so the system's own tools can update and remove it:
//!
//!   Fedora, openSUSE         the distribution's own package
//!   Ubuntu, Debian and their derivatives
//!                            the client author's package source (its Debian/Ubuntu packages on the
//!                            openSUSE Build Service), as in its Ubuntu/Debian installation guide
//!   Arch and its derivatives the AUR recipe, built on this computer and installed with pacman; the
//!                            app checks the AUR for newer versions, since pacman doesn't
//!
//! Everything that needs root runs as one small shell script (packaging/install-client) through
//! pkexec, so there is one password prompt.

use std::collections::HashMap;
use std::fs;
use std::process::{Command, Stdio};

use crate::core::which;

const AUR_PACKAGE: &str = "onedrive-abraunegg";

/// How the client gets installed on this system.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// The distribution's own, current package.
    Package { manager: Manager },
    /// The client author's package source for this Ubuntu or Debian release.
    /// `dir` is the release's folder there (e.g. "xUbuntu_24.04"), `release` its name for people.
    AptSource { dir: String, release: String },
    /// Built from the AUR recipe and installed with pacman.
    Aur,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Manager {
    Dnf,
    Zypper,
}

/// /etc/os-release as key → value (quotes removed).
/// Read once: the window asks every few seconds while the banner shows.
pub fn os_release() -> HashMap<String, String> {
    static OS: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    OS.get_or_init(|| {
        // lets the automated tests pretend to be another distribution
        let path =
            std::env::var("DRIVEDECK_OS_RELEASE").unwrap_or_else(|_| "/etc/os-release".into());
        parse_os_release(&fs::read_to_string(path).unwrap_or_default())
    })
    .clone()
}

pub fn parse_os_release(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim().trim_matches(['"', '\'']).to_string(),
            )
        })
        .collect()
}

/// The distribution and the ones it's based on, lower case ("linuxmint", "ubuntu", "debian").
fn family(os: &HashMap<String, String>) -> Vec<String> {
    ["ID", "ID_LIKE"]
        .iter()
        .filter_map(|k| os.get(*k))
        .flat_map(|v| v.split_whitespace().map(str::to_lowercase))
        .collect()
}

fn version_starts(os: &HashMap<String, String>, prefix: &str) -> bool {
    os.get("VERSION_ID").is_some_and(|v| v.starts_with(prefix))
}

fn is(os: &HashMap<String, String>, id: &str) -> bool {
    family(os)
        .iter()
        .any(|f| f == id || (id == "suse" && f.contains("suse")))
}

/// The folder in the client's package source for this Ubuntu- or Debian-based release, and the
/// release's name. Derivatives (Linux Mint, Pop!_OS, Zorin, elementary, LMDE, Raspberry Pi OS…)
/// say which Ubuntu or Debian release they're built on.
pub fn apt_release(os: &HashMap<String, String>, debian_version: &str) -> Option<(String, String)> {
    let get = |k: &str| os.get(k).map(|v| v.to_lowercase()).unwrap_or_default();
    let ubuntu = match get("UBUNTU_CODENAME").as_str() {
        "" if get("ID") == "ubuntu" => get("VERSION_CODENAME"),
        c => c.to_string(),
    };
    if !ubuntu.is_empty() {
        let v = match ubuntu.as_str() {
            "jammy" => "22.04",
            "noble" => "24.04",
            "questing" => "25.10",
            "resolute" => "26.04",
            _ => return None,
        };
        return Some((format!("xUbuntu_{v}"), format!("Ubuntu {v}")));
    }
    if !is(os, "debian") && !is(os, "raspbian") {
        return None;
    }
    let by_name = |c: &str| match c {
        "bullseye" | "elsie" => Some(11),
        "bookworm" | "faye" => Some(12),
        "trixie" | "gigi" => Some(13),
        _ => None,
    };
    let n = by_name(&get("DEBIAN_CODENAME"))
        .or_else(|| by_name(&get("VERSION_CODENAME")))
        .or_else(|| {
            // e.g. "12.7"; Ubuntu-based systems have "trixie/sid" here, but they were handled above
            debian_version
                .trim()
                .split('.')
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| (11..=13).contains(n))
        })?;
    Some((format!("Debian_{n}"), format!("Debian {n}")))
}

/// What can install the client here, if anything can without a terminal. `has` says whether a
/// program is installed; `ostree` is true on image-based systems (Fedora Silverblue, Kinoite…).
pub fn plan_for(
    os: &HashMap<String, String>,
    debian_version: &str,
    has: impl Fn(&str) -> bool,
    ostree: bool,
) -> Option<Plan> {
    if !has("pkexec") || ostree {
        return None;
    }
    if is(os, "fedora") && has("dnf") {
        return Some(Plan::Package {
            manager: Manager::Dnf,
        });
    }
    // openSUSE Leap 15 and SUSE Linux Enterprise 15 only have the 2.4 client
    if is(os, "suse") && has("zypper") && !version_starts(os, "15") {
        return Some(Plan::Package {
            manager: Manager::Zypper,
        });
    }
    if has("apt-get")
        && let Some((dir, release)) = apt_release(os, debian_version)
    {
        return Some(Plan::AptSource { dir, release });
    }
    // SteamOS is Arch-based but its system is read-only
    if is(os, "arch") && has("pacman") && os.get("ID").map(String::as_str) != Some("steamos") {
        return Some(Plan::Aur);
    }
    None
}

pub fn plan() -> Option<Plan> {
    plan_for(
        &os_release(),
        &fs::read_to_string("/etc/debian_version").unwrap_or_default(),
        |p| which(p).is_some(),
        image_based(),
    )
}

fn image_based() -> bool {
    std::path::Path::new("/run/ostree-booted").exists()
}

/// The installer that runs as root. The .deb and .rpm also install it as `HELPER`, together with a
/// polkit action that words the password prompt; the AppImage hands it to /bin/sh instead.
const SCRIPT: &str = include_str!("../packaging/install-client");
const HELPER: &str = "/usr/libexec/drivedeck/install-client";

/// pkexec, unless the automated tests replace it.
pub fn elevate() -> String {
    std::env::var("DRIVEDECK_PKEXEC").unwrap_or_else(|_| "pkexec".into())
}

impl Plan {
    /// The command that installs (or updates) the client. Asks for the password itself.
    pub fn command(&self) -> Command {
        let mut c = Command::new(elevate());
        if std::path::Path::new(HELPER).is_file() {
            c.arg(HELPER);
        } else {
            c.args(["/bin/sh", "-c", SCRIPT, "install-client"]);
        }
        match self {
            Plan::Package {
                manager: Manager::Dnf,
            } => c.arg("dnf"),
            Plan::Package {
                manager: Manager::Zypper,
            } => c.arg("zypper"),
            Plan::AptSource { dir, .. } => c.args(["apt", dir]),
            Plan::Aur => c.arg("aur"),
        };
        c.stdin(Stdio::null());
        c
    }

    /// What the confirmation says before the password prompt.
    pub fn confirmation(&self) -> String {
        let what = match self {
            Plan::Package { .. } => {
                "This installs the OneDrive client from your system's own software \
                 collection, the same place your other programs come from. Your normal software \
                 updates keep it up to date."
                    .to_string()
            }
            Plan::AptSource { release, .. } => format!(
                "{release} comes with an old version of the OneDrive client that its author no \
                 longer supports. So this does two things:\n\n\
                 • Adds the place the client's author recommends getting it from to your \
                 computer's list of software sources. It's like adding a trusted app store.\n\
                 • Installs the current version of the client from there.\n\n\
                 After that, your normal software updates keep it up to date, like any other program."
            ),
            Plan::Aur => {
                "Arch doesn't include the OneDrive client, but the AUR does. The AUR is a \
                 collection of install recipes shared by Arch users. This downloads the client's \
                 recipe, builds the program on your computer and installs it. Any tools needed to \
                 build it are installed first. It takes a few minutes.\n\n\
                 Your normal software updates don't cover the AUR, so this app checks for new \
                 versions of the client and offers to update it for you."
                    .to_string()
            }
        };
        let only = match self {
            Plan::Package { .. } => {
                "to install the OneDrive client (plus a few small parts it needs)"
            }
            Plan::AptSource { .. } => {
                "to add that software source and install the OneDrive client (plus a few small \
                 parts it needs)"
            }
            Plan::Aur => "to install the build tools and the OneDrive client",
        };
        format!(
            "{what}\n\n\
             Why your password? Installing software changes the whole computer, not just your \
             account, so your computer checks that it's really you. You type it into your \
             computer's own password window, not into this app. Your computer checks it once and \
             then forgets it: it isn't saved, and this app never sees it or sends it anywhere. \
             It's only used {only}. Nothing else is changed."
        )
    }
}

/// A short, distribution-specific note for the banner.
pub fn hint(plan: Option<&Plan>) -> String {
    let os = os_release();
    match plan {
        Some(Plan::Package { .. }) => "The app can install it for you.".into(),
        Some(Plan::AptSource { .. }) => "The copy that comes with your system is too old, so the app gets the current version for you.".into(),
        Some(Plan::Aur) => "On Arch it comes from the AUR (install recipes shared by Arch users). The app can build and install it for you.".into(),
        None if is(&os, "fedora") && image_based() => "On Fedora Silverblue, Kinoite and other image-based versions: open a terminal and run  rpm-ostree install onedrive  then restart.".into(),
        None if is(&os, "fedora") => "Open a terminal and run  sudo dnf install onedrive".into(),
        None if is(&os, "suse") && version_starts(&os, "15") => "This version of openSUSE only has an old OneDrive client. The instructions show how to get a current one.".into(),
        None if is(&os, "debian") || is(&os, "ubuntu") => "The app can't install it automatically on this version of your system. The instructions show how to install it yourself.".into(),
        None => "The instructions cover every major distribution.".into(),
    }
}

/// The installation instructions that fit this system best.
pub fn instructions_url() -> &'static str {
    let os = os_release();
    if is(&os, "debian") || is(&os, "ubuntu") {
        "https://github.com/abraunegg/onedrive/blob/master/docs/ubuntu-package-install.md"
    } else {
        crate::core::CLIENT_INSTALL_URL
    }
}

/// On Arch, when the AUR has a newer client than the one installed by its recipe:
/// (installed, newest). None when there's nothing to update, or it can't be checked right now.
pub fn aur_update() -> Option<(String, String)> {
    if plan() != Some(Plan::Aur) {
        return None;
    }
    let installed = String::from_utf8(
        Command::new("pacman")
            .args(["-Q", AUR_PACKAGE])
            .stderr(Stdio::null())
            .output()
            .ok()?
            .stdout,
    )
    .ok()?;
    let installed = installed.split_whitespace().nth(1)?.to_string();
    let url = std::env::var("DRIVEDECK_AUR_RPC")
        .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5/info".into());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .into();
    let reply: serde_json::Value = agent
        .get(&url)
        .query("arg[]", AUR_PACKAGE)
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    let newest = reply["results"][0]["Version"].as_str()?.to_string();
    // pacman's own version comparison: <0 when the first is older
    let cmp = Command::new("vercmp")
        .args([&installed, &newest])
        .output()
        .ok()?;
    let older = String::from_utf8_lossy(&cmp.stdout)
        .trim()
        .parse::<i32>()
        .ok()?
        < 0;
    older.then_some((installed, newest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(os: &str, debian_version: &str) -> Option<Plan> {
        plan_for(&parse_os_release(os), debian_version, |_| true, false)
    }

    fn apt(dir: &str) -> Option<String> {
        Some(dir.to_string())
    }

    fn dir_of(p: Option<Plan>) -> Option<String> {
        match p {
            Some(Plan::AptSource { dir, .. }) => Some(dir),
            _ => None,
        }
    }

    #[test]
    fn package_based_systems() {
        assert_eq!(
            plan_of("ID=fedora\nVERSION_ID=43", ""),
            Some(Plan::Package {
                manager: Manager::Dnf
            })
        );
        assert_eq!(
            plan_of("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"", ""),
            Some(Plan::Package {
                manager: Manager::Zypper
            })
        );
        // Leap 15 only has the 2.4 client
        assert_eq!(
            plan_of(
                "ID=\"opensuse-leap\"\nVERSION_ID=\"15.6\"\nID_LIKE=\"suse opensuse\"",
                ""
            ),
            None
        );
        assert!(
            plan_of(
                "ID=\"opensuse-leap\"\nVERSION_ID=\"16.0\"\nID_LIKE=\"suse opensuse\"",
                ""
            )
            .is_some()
        );
        // image-based Fedora can't use dnf install
        assert_eq!(
            plan_for(&parse_os_release("ID=fedora"), "", |_| true, true),
            None
        );
        // no password prompt available
        assert_eq!(
            plan_for(&parse_os_release("ID=fedora"), "", |p| p != "pkexec", false),
            None
        );
    }

    #[test]
    fn ubuntu_and_its_derivatives() {
        let ubuntu = "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\nVERSION_CODENAME=noble\nUBUNTU_CODENAME=noble";
        assert_eq!(dir_of(plan_of(ubuntu, "trixie/sid")), apt("xUbuntu_24.04"));
        let mint =
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=xia\nUBUNTU_CODENAME=noble";
        assert_eq!(dir_of(plan_of(mint, "trixie/sid")), apt("xUbuntu_24.04"));
        let mint21 = "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=virginia\nUBUNTU_CODENAME=jammy";
        assert_eq!(
            dir_of(plan_of(mint21, "bookworm/sid")),
            apt("xUbuntu_22.04")
        );
        let pop =
            "ID=pop\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=noble\nUBUNTU_CODENAME=noble";
        assert_eq!(dir_of(plan_of(pop, "")), apt("xUbuntu_24.04"));
        let zorin =
            "ID=zorin\nID_LIKE=\"ubuntu debian\"\nVERSION_CODENAME=jammy\nUBUNTU_CODENAME=jammy";
        assert_eq!(dir_of(plan_of(zorin, "")), apt("xUbuntu_22.04"));
        // releases the client's packages don't cover
        let old = "ID=ubuntu\nVERSION_CODENAME=focal\nUBUNTU_CODENAME=focal";
        assert_eq!(plan_of(old, ""), None);
    }

    #[test]
    fn debian_and_its_derivatives() {
        let debian = "ID=debian\nVERSION_ID=\"12\"\nVERSION_CODENAME=bookworm";
        assert_eq!(dir_of(plan_of(debian, "12.7")), apt("Debian_12"));
        let trixie = "ID=debian\nVERSION_CODENAME=trixie";
        assert_eq!(dir_of(plan_of(trixie, "13.1")), apt("Debian_13"));
        let lmde = "ID=linuxmint\nID_LIKE=debian\nVERSION_CODENAME=faye\nDEBIAN_CODENAME=bookworm";
        assert_eq!(dir_of(plan_of(lmde, "12.5")), apt("Debian_12"));
        let pi = "ID=raspbian\nID_LIKE=debian\nVERSION_CODENAME=bookworm";
        assert_eq!(dir_of(plan_of(pi, "12.4")), apt("Debian_12"));
        // a derivative with its own codename still says which Debian it is
        let mx = "ID=mx\nID_LIKE=debian\nVERSION_CODENAME=libretto";
        assert_eq!(dir_of(plan_of(mx, "12.6")), apt("Debian_12"));
        let kali = "ID=kali\nID_LIKE=debian\nVERSION_CODENAME=kali-rolling";
        assert_eq!(plan_of(kali, "kali-rolling"), None);
        let sid = "ID=debian\nVERSION_CODENAME=sid";
        assert_eq!(plan_of(sid, "trixie/sid"), None);
    }

    #[test]
    fn arch_and_its_derivatives() {
        for os in [
            "ID=arch",
            "ID=cachyos\nID_LIKE=arch",
            "ID=endeavouros\nID_LIKE=arch",
            "ID=manjaro\nID_LIKE=arch",
        ] {
            assert_eq!(plan_of(os, ""), Some(Plan::Aur), "{os}");
        }
        assert_eq!(plan_of("ID=steamos\nID_LIKE=arch", ""), None);
        assert_eq!(plan_of("ID=gentoo", ""), None);
    }

    #[test]
    fn scripts_get_their_arguments() {
        let args = |p: Plan| -> Vec<String> {
            p.command()
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect()
        };
        let apt = args(Plan::AptSource {
            dir: "xUbuntu_24.04".into(),
            release: "Ubuntu 24.04".into(),
        });
        assert_eq!(apt[apt.len() - 2..], ["apt", "xUbuntu_24.04"]);
        assert_eq!(args(Plan::Aur).last().unwrap(), "aur");
        assert!(SCRIPT.contains(AUR_PACKAGE));
    }
}
