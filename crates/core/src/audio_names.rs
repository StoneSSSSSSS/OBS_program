//! Matching Windows audio endpoints by name.
//!
//! When a USB headset is unplugged and plugged back in (or into another port),
//! Windows often registers it as a *new* endpoint called e.g.
//! `Headset Microphone (2- Astro A50 Voice)` with a new device ID. OBS keeps
//! pointing at the old ID and records silence. We therefore match devices by a
//! normalised name and always pick the copy that is currently connected.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// WASAPI endpoint id, e.g. `{0.0.1.00000000}.{2079d28b-...}`.
    pub id: String,
    /// Friendly name, e.g. `Headset Microphone (Astro A50 Voice)`.
    pub name: String,
}

/// Remove the `N- ` duplicate-device prefix Windows adds inside the
/// parenthesised device description, and normalise whitespace/case.
///
/// `Headset Microphone (2- Astro A50 Voice)` -> `headset microphone (astro a50 voice)`
pub fn normalize_endpoint_name(name: &str) -> String {
    static RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\(\s*\d{1,3}\s*-\s*").unwrap());
    let s = RE.replace_all(name.trim(), "(");
    crate::naming::collapse_ws(&s).to_lowercase()
}

/// Display form of a normalised name (keeps original casing, drops `N- `).
pub fn clean_display_name(name: &str) -> String {
    static RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\(\s*\d{1,3}\s*-\s*").unwrap());
    crate::naming::collapse_ws(&RE.replace_all(name.trim(), "("))
}

/// Pick the best currently-active endpoint for an ordered preference list.
///
/// For each preference (in order) we first look for an exact normalised match,
/// then for an endpoint whose name contains the preference text (so a user can
/// simply type "Astro A50"). Among several matches we prefer the one without a
/// numeric prefix, then a stable order by name/id.
pub fn pick_preferred<'a>(priority: &[String], active: &'a [Endpoint]) -> Option<(usize, &'a Endpoint)> {
    for (rank, pref) in priority.iter().enumerate() {
        let want = normalize_endpoint_name(pref);
        if want.is_empty() {
            continue;
        }
        let mut exact: Vec<&Endpoint> = active
            .iter()
            .filter(|e| normalize_endpoint_name(&e.name) == want)
            .collect();
        if exact.is_empty() {
            exact = active
                .iter()
                .filter(|e| normalize_endpoint_name(&e.name).contains(&want))
                .collect();
        }
        if exact.is_empty() {
            continue;
        }
        exact.sort_by(|a, b| {
            let a_plain = clean_display_name(&a.name) == a.name.trim();
            let b_plain = clean_display_name(&b.name) == b.name.trim();
            b_plain.cmp(&a_plain).then(a.name.cmp(&b.name)).then(a.id.cmp(&b.id))
        });
        return Some((rank, exact[0]));
    }
    None
}

/// What OBS's mic input should be set to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicDecision {
    /// Use this endpoint (index into the priority list, endpoint).
    Preferred { rank: usize, endpoint: Endpoint },
    /// No preferred mic connected; use the Windows default device.
    WindowsDefault,
    /// No preferred mic connected and fallback disabled; leave OBS alone.
    Leave,
}

pub fn decide_mic(priority: &[String], active: &[Endpoint], fallback: &str) -> MicDecision {
    if let Some((rank, ep)) = pick_preferred(priority, active) {
        return MicDecision::Preferred { rank, endpoint: ep.clone() };
    }
    if priority.is_empty() {
        // Nothing configured: follow Windows.
        return MicDecision::WindowsDefault;
    }
    match fallback {
        "none" => MicDecision::Leave,
        _ => MicDecision::WindowsDefault,
    }
}

impl MicDecision {
    /// The OBS `device_id` setting value this decision maps to.
    pub fn obs_device_id(&self) -> Option<&str> {
        match self {
            MicDecision::Preferred { endpoint, .. } => Some(&endpoint.id),
            MicDecision::WindowsDefault => Some("default"),
            MicDecision::Leave => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: &str, name: &str) -> Endpoint {
        Endpoint { id: id.into(), name: name.into() }
    }

    #[test]
    fn normalizes_windows_duplicate_prefix() {
        assert_eq!(
            normalize_endpoint_name("Headset Microphone (2- Astro A50 Voice)"),
            "headset microphone (astro a50 voice)"
        );
        assert_eq!(
            normalize_endpoint_name("Headset Microphone (11- Astro A50 Voice)"),
            normalize_endpoint_name("Headset Microphone (Astro A50 Voice)")
        );
        assert_eq!(normalize_endpoint_name("  Mic   (USB) "), "mic (usb)");
        assert_eq!(clean_display_name("Line (10- Astro A50 Game)"), "Line (Astro A50 Game)");
    }

    #[test]
    fn picks_reconnected_headset_with_new_id() {
        // Original device vanished; Windows re-registered it as "3- ...".
        let active = vec![
            ep("{webcam}", "Microphone (HD Webcam)"),
            ep("{new-id}", "Headset Microphone (3- Astro A50 Voice)"),
        ];
        let prio = vec!["Headset Microphone (Astro A50 Voice)".to_string()];
        let d = decide_mic(&prio, &active, "windows_default");
        assert_eq!(d.obs_device_id(), Some("{new-id}"));
    }

    #[test]
    fn prefers_plain_name_over_numbered_copy() {
        let active = vec![
            ep("{b}", "Headset Microphone (2- Astro A50 Voice)"),
            ep("{a}", "Headset Microphone (Astro A50 Voice)"),
        ];
        let prio = vec!["Headset Microphone (Astro A50 Voice)".to_string()];
        assert_eq!(pick_preferred(&prio, &active).unwrap().1.id, "{a}");
    }

    #[test]
    fn priority_order_and_substring_match() {
        let active = vec![ep("{web}", "Microphone (HD Webcam)"), ep("{usb}", "Microphone (Yeti Stereo)")];
        let prio = vec!["Astro A50".to_string(), "Yeti".to_string(), "Webcam".to_string()];
        let (rank, e) = pick_preferred(&prio, &active).unwrap();
        assert_eq!(rank, 1);
        assert_eq!(e.id, "{usb}");
    }

    #[test]
    fn fallbacks() {
        let active = vec![ep("{web}", "Microphone (HD Webcam)")];
        let prio = vec!["Astro A50".to_string()];
        assert_eq!(decide_mic(&prio, &active, "windows_default"), MicDecision::WindowsDefault);
        assert_eq!(decide_mic(&prio, &active, "none"), MicDecision::Leave);
        assert_eq!(decide_mic(&[], &active, "none"), MicDecision::WindowsDefault);
        assert_eq!(MicDecision::Leave.obs_device_id(), None);
    }
}
