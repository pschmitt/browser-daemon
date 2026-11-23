//! `browser-daemon local install`: register the native messaging host with
//! Firefox and Chromium-based browsers for the current user. (With Nix, the
//! `browser-native-host` package and the Home Manager module do this.)

use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use serde_json::json;

use super::{CHROMIUM_EXTENSION_ID, FIREFOX_EXTENSION_ID, HOST_NAME, HOST_PROGRAM, R};

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browser {
    Firefox,
    Chromium,
    Chrome,
    Brave,
}

#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Browsers to register with (repeatable; default: all).
    #[arg(long = "browser", value_enum)]
    browsers: Vec<Browser>,
    /// The Chromium extension's id, when it is not the published one.
    #[arg(long, default_value = CHROMIUM_EXTENSION_ID)]
    chromium_extension_id: String,
    /// Remove the registrations instead.
    #[arg(long)]
    uninstall: bool,
}

fn home() -> R<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

fn manifest_dir(browser: Browser, home: &Path) -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    match browser {
        Browser::Firefox => home.join(".mozilla/native-messaging-hosts"),
        Browser::Chromium => config.join("chromium/NativeMessagingHosts"),
        Browser::Chrome => config.join("google-chrome/NativeMessagingHosts"),
        Browser::Brave => config.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
    }
}

pub fn manifest(browser: Browser, program: &Path, chromium_id: &str) -> serde_json::Value {
    let mut m = json!({
        "name": HOST_NAME,
        "description": "Browser Daemon local tab API",
        "path": program,
        "type": "stdio",
    });
    if browser == Browser::Firefox {
        m["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]);
    } else {
        m["allowed_origins"] = json!([format!("chrome-extension://{chromium_id}/")]);
    }
    m
}

/// The program the manifests point at: `browser-daemon-native-host` next to
/// this binary, or a link to it in `$XDG_DATA_HOME/browser-daemon`.
fn host_program(home: &Path) -> R<PathBuf> {
    let exe = std::env::current_exe()?;
    if let Some(sibling) = exe.parent().map(|d| d.join(HOST_PROGRAM)) {
        if sibling.exists() {
            return Ok(sibling);
        }
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("browser-daemon");
    std::fs::create_dir_all(&data)?;
    let link = data.join(HOST_PROGRAM);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&exe, &link)?;
    Ok(link)
}

pub fn install(args: InstallArgs) -> R<()> {
    let home = home()?;
    let browsers = if args.browsers.is_empty() {
        vec![
            Browser::Firefox,
            Browser::Chromium,
            Browser::Chrome,
            Browser::Brave,
        ]
    } else {
        args.browsers
    };
    let program = if args.uninstall {
        PathBuf::new()
    } else {
        host_program(&home)?
    };
    for browser in browsers {
        let dir = manifest_dir(browser, &home);
        let path = dir.join(format!("{HOST_NAME}.json"));
        if args.uninstall {
            match std::fs::remove_file(&path) {
                Ok(()) => println!("removed {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display()).into()),
            }
            continue;
        }
        // A browser that was never started has no profile directory: skip it,
        // except Firefox's, which needs no profile.
        if browser != Browser::Firefox && dir.parent().is_some_and(|p| !p.exists()) {
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        let m = manifest(browser, &program, &args.chromium_extension_id);
        std::fs::write(&path, serde_json::to_string_pretty(&m)? + "\n")?;
        println!("registered {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_name_the_extension_per_engine() {
        let program = Path::new("/bin/x");
        let f = manifest(Browser::Firefox, program, "abc");
        assert_eq!(f["allowed_extensions"][0], FIREFOX_EXTENSION_ID);
        assert!(f.get("allowed_origins").is_none());
        let c = manifest(Browser::Chrome, program, "abc");
        assert_eq!(c["allowed_origins"][0], "chrome-extension://abc/");
        assert_eq!(c["path"], "/bin/x");
        assert_eq!(c["name"], HOST_NAME);
    }

    /// The id baked in here is the one the release publishes.
    #[test]
    fn knows_the_published_chromium_id() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/browser/packages/metadata.json"
        );
        // The Nix build leaves browser/ out of the source.
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let meta: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(meta["chromiumId"], CHROMIUM_EXTENSION_ID);
    }
}
