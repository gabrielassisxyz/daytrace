use crate::activity::ActivitySnapshot;
use crate::config::{Blacklist, redact_title};
use serde::Deserialize;
use std::process::Command;

/// The desktop boundary the capture loop observes through.
///
/// Exists so the loop's failure handling can be driven by a fake: a compositor query that
/// fails once and then recovers is the case that used to end the daemon, and it cannot be
/// staged against a live compositor.
pub trait ActiveWindowSource {
    fn active_snapshot(&self, blacklist: &Blacklist) -> Result<Option<ActivitySnapshot>, String>;
}

pub struct HyprlandClient {
    command: String,
}

#[derive(Debug, Deserialize)]
struct HyprlandWindow {
    address: Option<String>,
    mapped: Option<bool>,
    #[serde(rename = "class")]
    app_class: Option<String>,
    title: Option<String>,
    workspace: Option<HyprlandWorkspace>,
    monitor: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HyprlandWorkspace {
    name: String,
}

impl HyprlandClient {
    pub fn new() -> Self {
        Self {
            command: "hyprctl".to_string(),
        }
    }
}

impl ActiveWindowSource for HyprlandClient {
    fn active_snapshot(&self, blacklist: &Blacklist) -> Result<Option<ActivitySnapshot>, String> {
        let output = Command::new(&self.command)
            .args(["-j", "activewindow"])
            .output()
            .map_err(|error| format!("failed to run hyprctl: {error}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("hyprctl activewindow failed: {}", stderr.trim()));
        }

        parse_active_window(&String::from_utf8_lossy(&output.stdout), blacklist)
    }
}

pub fn parse_active_window(
    json: &str,
    blacklist: &Blacklist,
) -> Result<Option<ActivitySnapshot>, String> {
    if json.trim().is_empty() {
        return Ok(Some(ActivitySnapshot::unknown()));
    }

    let window: HyprlandWindow =
        serde_json::from_str(json).map_err(|error| format!("invalid hyprctl JSON: {error}"))?;

    if !window.mapped.unwrap_or(false) || window.address.as_deref() == Some("0x0") {
        return Ok(Some(ActivitySnapshot::unknown()));
    }

    if blacklist.should_skip(window.app_class.as_deref(), window.title.as_deref())
        || is_private_browser_window(window.app_class.as_deref(), window.title.as_deref())
    {
        return Ok(None);
    }

    let title = recordable_title(window.app_class.as_deref(), window.title.as_deref());

    Ok(Some(ActivitySnapshot::window(
        window.app_class,
        title,
        window.workspace.map(|workspace| workspace.name),
        window.monitor,
    )))
}

fn recordable_title(app_class: Option<&str>, title: Option<&str>) -> Option<String> {
    let title = title?;
    if is_browser_class(app_class) {
        return Some("[browser title redacted]".to_string());
    }

    Some(redact_title(title))
}

fn is_private_browser_window(app_class: Option<&str>, title: Option<&str>) -> bool {
    is_browser_class(app_class)
        && title.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("private browsing")
                || value.contains("incognito")
                || value.contains("inprivate")
                || value.contains("private window")
                || value.contains("tor browser")
        })
}

fn is_browser_class(app_class: Option<&str>) -> bool {
    const BROWSER_CLASS_TERMS: &[&str] = &[
        "arc",
        "brave",
        "browser",
        "chrome",
        "chromium",
        "edge",
        "firefox",
        "floorp",
        "librewolf",
        "microsoft-edge",
        "opera",
        "vivaldi",
        "waterfox",
        "zen",
    ];

    app_class.is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        BROWSER_CLASS_TERMS
            .iter()
            .any(|browser| value.contains(browser))
    })
}

#[cfg(test)]
mod tests {
    use super::parse_active_window;
    use crate::activity::{ActivityKind, ActivitySnapshot};
    use crate::config::Blacklist;

    #[test]
    fn parses_active_hyprland_window() {
        let json = r#"{
            "address": "0xabc",
            "mapped": true,
            "class": "com.mitchellh.ghostty",
            "title": "tmux",
            "workspace": { "id": 3, "name": "3" },
            "monitor": 1
        }"#;

        assert_eq!(
            parse_active_window(json, &Blacklist::default()).expect("valid window"),
            Some(ActivitySnapshot {
                kind: ActivityKind::Window,
                app_class: Some("com.mitchellh.ghostty".to_string()),
                title: Some("tmux".to_string()),
                workspace: Some("3".to_string()),
                monitor: Some(1),
            })
        );
    }

    #[test]
    fn skips_blacklisted_windows_before_storage() {
        let json = r#"{
            "address": "0xabc",
            "mapped": true,
            "class": "KeePassXC",
            "title": "Passwords",
            "workspace": { "id": 1, "name": "1" },
            "monitor": 0
        }"#;
        let blacklist = Blacklist::new(vec!["keepassxc".to_string()], Vec::new(), Vec::new());

        assert_eq!(
            parse_active_window(json, &blacklist).expect("valid window"),
            None
        );
    }

    #[test]
    fn redacts_browser_titles_by_default() {
        let json = r#"{
            "address": "0xabc",
            "mapped": true,
            "class": "brave-browser",
            "title": "Inbox - Brave",
            "workspace": { "id": 2, "name": "2" },
            "monitor": 1
        }"#;

        let snapshot = parse_active_window(json, &Blacklist::default())
            .expect("valid window")
            .expect("recordable browser");
        assert_eq!(snapshot.title, Some("[browser title redacted]".to_string()));
    }

    #[test]
    fn skips_private_browser_windows() {
        let json = r#"{
            "address": "0xabc",
            "mapped": true,
            "class": "firefox",
            "title": "Private Browsing",
            "workspace": { "id": 2, "name": "2" },
            "monitor": 1
        }"#;

        assert_eq!(
            parse_active_window(json, &Blacklist::default()).expect("valid window"),
            None
        );
    }

    #[test]
    fn redacts_common_non_default_browser_titles() {
        for app_class in [
            "LibreWolf",
            "zen-browser",
            "vivaldi-stable",
            "microsoft-edge",
        ] {
            let json = format!(
                r#"{{
                    "address": "0xabc",
                    "mapped": true,
                    "class": "{app_class}",
                    "title": "Sensitive Page",
                    "workspace": {{ "id": 2, "name": "2" }},
                    "monitor": 1
                }}"#
            );

            let snapshot = parse_active_window(&json, &Blacklist::default())
                .expect("valid window")
                .expect("recordable browser");
            assert_eq!(snapshot.title, Some("[browser title redacted]".to_string()));
        }
    }

    #[test]
    fn skips_inprivate_browser_windows() {
        let json = r#"{
            "address": "0xabc",
            "mapped": true,
            "class": "microsoft-edge",
            "title": "InPrivate Browsing",
            "workspace": { "id": 2, "name": "2" },
            "monitor": 1
        }"#;

        assert_eq!(
            parse_active_window(json, &Blacklist::default()).expect("valid window"),
            None
        );
    }

    #[test]
    fn treats_empty_active_window_as_unknown() {
        assert_eq!(
            parse_active_window("{}", &Blacklist::default()).expect("valid empty object"),
            Some(ActivitySnapshot::unknown())
        );
    }

    #[test]
    fn treats_blank_active_window_output_as_unknown() {
        assert_eq!(
            parse_active_window(" \n\t", &Blacklist::default()).expect("valid blank output"),
            Some(ActivitySnapshot::unknown())
        );
    }
}
