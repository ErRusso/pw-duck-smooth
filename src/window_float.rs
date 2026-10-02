//! Best-effort request to show a window as a centered floating window.
//!
//! GTK4 dropped every window type hint (`gtk_window_set_type_hint` and
//! friends), so a plain GTK window is tiled by tiling compositors. Placement is
//! owned by the window manager, so the request has to be made there: the tuner
//! installs a window rule before it maps and every window manager that ignores
//! it simply tiles the window as usual.
//!
//! Every failure is ignored on purpose: the window is usable either way, and
//! the tuner must never fail to open because a hint did not arrive. Set
//! `PW_DUCK_FLOAT=0` to switch the request off, or `PW_DUCK_FLOAT_DEBUG=1` to
//! log the handshake to `pw-duck-smooth/window-float.log` in the cache dir.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::process::Command;
use std::time::Duration;

/// Result of the window-rule handshake.
#[derive(PartialEq)]
enum Outcome {
    /// The window manager accepted the rule.
    Installed,
    /// No Lua IPC, so fall back to a dispatcher on the mapped window.
    Unsupported,
}

/// Ask the window manager to map the current process' window as a centered
/// float. Call this before the window is realized: window rules only apply to
/// windows mapped after the rule exists.
pub fn request_centered_floating(class: &str) {
    let enabled = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some();
    log(format_args!(
        "request class={class} hyprland={enabled} disabled={} pid={}",
        disabled(),
        std::process::id()
    ));
    if disabled() || !enabled {
        return;
    }

    if run_hyprland_eval(&hyprland_rule_script(class)) == Outcome::Installed {
        return;
    }
    legacy_request();
}

fn disabled() -> bool {
    matches!(
        std::env::var("PW_DUCK_FLOAT").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

/// The Hyprland 0.56 window rule that makes the tuner a centered float.
///
/// Rules are declarative and only read when a window maps, which is why this
/// runs before the window exists. The `class` is the Wayland app id, and the
/// effect field is `float`, not `floating`. Named, so a second run replaces the
/// rule instead of stacking a duplicate.
fn hyprland_rule_script(class: &str) -> String {
    format!(
        "hl.window_rule({{\n\
         \x20 name = \"pw-duck-float\",\n\
         \x20 match = {{ class = \"^{class}$\" }},\n\
         \x20 float = true,\n\
         \x20 center = true,\n\
         }})\n\
         error(\"pw-duck-float-rule-installed\")\n"
    )
}

fn run_hyprland_eval(script: &str) -> Outcome {
    let Ok(output) = Command::new("hyprctl").args(["eval", script]).output() else {
        log(format_args!("eval could not be spawned"));
        return Outcome::Unsupported;
    };
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    log(format_args!("eval report: {}", report.trim()));
    if report.contains("pw-duck-float-rule-installed") {
        Outcome::Installed
    } else {
        Outcome::Unsupported
    }
}

/// Pre-Lua Hyprland: no window rules over IPC, so toggle the window floating
/// once the compositor mapped it. No centering: the classic dispatchers have no
/// per-window move.
fn legacy_request() {
    let pid = std::process::id();
    for _ in 0..40 {
        if let Some(address) = legacy_address(pid) {
            log(format_args!("legacy togglefloating {address}"));
            let _ = Command::new("hyprctl")
                .args(["dispatch", "togglefloating", &format!("address:{address}")])
                .output();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log(format_args!("legacy: window never showed up"));
}

fn legacy_address(pid: u32) -> Option<String> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    let clients: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    clients
        .as_array()?
        .iter()
        .find(|client| {
            client.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid))
        })
        .and_then(|client| client.get("address"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn debug_enabled() -> bool {
    matches!(
        std::env::var("PW_DUCK_FLOAT_DEBUG").as_deref(),
        Ok("1" | "on" | "true" | "yes")
    )
}

fn log(args: std::fmt::Arguments<'_>) {
    if !debug_enabled() {
        return;
    }
    let path = std::env::var("XDG_CACHE_HOME").map_or_else(
        |_| std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from,
    );
    let path = path.join("pw-duck-smooth");
    let _ = std::fs::create_dir_all(&path);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.join("window-float.log"))
    {
        let _ = writeln!(file, "{args}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_floats_and_centers_the_given_class() {
        let script = hyprland_rule_script("dev.pw_duck.Tune");
        assert!(script.contains(r#"match = { class = "^dev.pw_duck.Tune$" }"#));
        assert!(script.contains("float = true"));
        assert!(script.contains("center = true"));
    }

    #[test]
    fn rule_is_named_so_a_second_run_replaces_it() {
        let script = hyprland_rule_script("dev.pw_duck.Tune");
        assert!(script.contains(r#"name = "pw-duck-float""#));
    }

    #[test]
    fn rule_reports_success_only_after_it_was_added() {
        let script = hyprland_rule_script("dev.pw_duck.Tune");
        let last = script
            .lines()
            .next_back()
            .expect("script ends with a report");
        assert!(last.contains("pw-duck-float-rule-installed"));
    }
}
