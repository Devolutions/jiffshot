//! Linux command-line entry points for the editor and capture workflows.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

#[derive(Debug, Eq, PartialEq)]
pub enum Launch {
    Editor(Option<PathBuf>),
    Background,
    Capture,
    InteractiveCapture,
    GrabText,
    Scrolling,
    HorizontalScrolling,
    Panoramic,
    Interval,
    Record,
    Help,
    Version,
}

pub const USAGE: &str = "\
Usage: jiffshot [FILE]
       jiffshot --capture | --capture-interactive | --grab-text | --scrolling | --scrolling-horizontal | --panoramic | --interval | --record
       jiffshot --background
       jiffshot --help | --version

  --background            Start in the tray when enabled and available
  --capture               Capture a monitor or window
  --capture-interactive   Open the desktop screenshot chooser
  --grab-text             Copy text from a desktop-selected screenshot
  --scrolling             Start manual vertical scrolling capture
  --scrolling-horizontal  Start manual horizontal scrolling capture
  --panoramic             Start manual panoramic capture
  --interval              Start interval capture with saved preferences
  --record                Start screen recording without audio
  --help                  Show this help
  --version               Show the application version

Use -- before a file name that starts with a hyphen.";

pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Launch, String> {
    let mut arguments = arguments.into_iter();
    let Some(first) = arguments.next() else {
        return Ok(Launch::Editor(None));
    };
    if first == OsStr::new("--") {
        let path = arguments.next().map(PathBuf::from);
        if arguments.next().is_some() {
            return Err("only one file can be opened at a time".to_owned());
        }
        return Ok(Launch::Editor(path));
    }

    let launch = match first.to_str() {
        Some("--background") => Launch::Background,
        Some("--capture") => Launch::Capture,
        Some("--capture-interactive") => Launch::InteractiveCapture,
        Some("--grab-text") => Launch::GrabText,
        Some("--scrolling") => Launch::Scrolling,
        Some("--scrolling-horizontal") => Launch::HorizontalScrolling,
        Some("--panoramic") => Launch::Panoramic,
        Some("--interval") => Launch::Interval,
        Some("--record") => Launch::Record,
        Some("--help" | "-h") => Launch::Help,
        Some("--version" | "-V") => Launch::Version,
        _ if first.as_os_str().as_bytes().starts_with(b"-") => {
            return Err(format!("unrecognized option: {}", first.to_string_lossy()))
        }
        _ => Launch::Editor(Some(PathBuf::from(first))),
    };
    if arguments.next().is_some() {
        return Err("only one launch action or file can be specified".to_owned());
    }
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_each_supported_launch_action() {
        for (arguments, expected) in [
            (vec![], Launch::Editor(None)),
            (vec!["--background"], Launch::Background),
            (vec!["--capture"], Launch::Capture),
            (vec!["--capture-interactive"], Launch::InteractiveCapture),
            (vec!["--grab-text"], Launch::GrabText),
            (vec!["--scrolling"], Launch::Scrolling),
            (vec!["--scrolling-horizontal"], Launch::HorizontalScrolling),
            (vec!["--panoramic"], Launch::Panoramic),
            (vec!["--interval"], Launch::Interval),
            (vec!["--record"], Launch::Record),
            (vec!["--help"], Launch::Help),
            (vec!["-h"], Launch::Help),
            (vec!["--version"], Launch::Version),
            (vec!["-V"], Launch::Version),
        ] {
            assert_eq!(parse(self::arguments(&arguments)), Ok(expected));
        }
    }

    #[test]
    fn preserves_non_utf8_and_hyphenated_paths() {
        let non_utf8 = OsString::from_vec(b"capture-\xff.png".to_vec());
        assert_eq!(
            parse([non_utf8.clone()]),
            Ok(Launch::Editor(Some(PathBuf::from(non_utf8))))
        );
        assert_eq!(
            parse(arguments(&["--", "-capture.png"])),
            Ok(Launch::Editor(Some(PathBuf::from("-capture.png"))))
        );
        assert_eq!(parse(arguments(&["--"])), Ok(Launch::Editor(None)));
    }

    #[test]
    fn rejects_unknown_options_and_conflicting_actions() {
        for arguments in [
            vec!["--missing"],
            vec!["--capture", "--record"],
            vec!["--background", "--capture"],
            vec!["--capture-interactive", "--record"],
            vec!["--interval", "--capture"],
            vec!["one.png", "two.png"],
            vec!["--", "one.png", "two.png"],
        ] {
            assert!(
                parse(self::arguments(&arguments)).is_err(),
                "invalid arguments should be rejected: {arguments:?}"
            );
        }
    }
}
