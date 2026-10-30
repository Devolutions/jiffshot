//! `jiffshot-cli`: drives the running Jiffshot from scripts and AI agents.
//!
//! Every command is one request over Jiffshot's control pipe (see the
//! `jiffshot-control` crate). The result is printed to stdout as one JSON
//! object, `{"ok":true,"result":…}` or `{"ok":false,"error":{"code","message"}}`,
//! and the exit code says how it went:
//!
//! | code | meaning                                                        |
//! |------|----------------------------------------------------------------|
//! | 0    | success                                                        |
//! | 1    | the command failed (not found, conflict, internal)             |
//! | 2    | invalid usage or arguments                                     |
//! | 3    | Jiffshot is unavailable (not running and could not be started) |
//! | 4    | timed out                                                      |
//!
//! When Jiffshot is not running it is started (unless `--no-launch`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use jiffshot_control::{Error, ErrorCode};
use serde_json::{json, Map, Value};

const USAGE: &str = r#"jiffshot-cli: drive the running Jiffshot from scripts and AI agents

USAGE
    jiffshot-cli [GLOBAL OPTIONS] COMMAND [ARGS] [--option value]...

GLOBAL OPTIONS
    --pretty            indent the JSON output
    --timeout SECS      give up waiting for the reply after SECS (default 60)
    --no-launch         fail with exit code 3 instead of starting Jiffshot
    --args JSON         extra arguments as a JSON object (merged last)

CAPTURE
    status                                  what is running, the library folder, all commands
    monitors                                displays with their rectangles and DPI
    windows [TITLE] [--process P] [--all]   top-level windows (hwnd, title, process, rect)
    cursor                                  the pointer position
    screenshot [TARGET] [--out FILE.png] [--cursor] [--delay-ms N]
    record start [TARGET] [--out FILE.mp4] [--fps N] [--audio none|system|mic|both]
                 [--clicks] [--highlight] [--no-cursor] [--max-seconds N] [--key ID]
    record status | pause | resume
    record stop [--id ID]                   waits until the MP4 is final; returns its metadata
    record cancel [--id ID]                 stops and deletes the recording
    steps start [TITLE] [--restart]         begin a step-by-step guide
    steps add [X,Y] [--description TEXT] [--right]
    steps status | cancel
    steps finish [--out FILE.png] [--title T] [--open]

INPUT (pointer moves glide visibly and typing is paced while recording)
    click [X,Y] [POINTER] [--right|--middle] [--double] [--no-step] [--description TEXT]
    move [X,Y] [POINTER]
    drag X1,Y1 X2,Y2 [--right]
    scroll N [POINTER] [--horizontal]       positive scrolls up (or right)
    type TEXT [TARGET] [--delay-ms N]       \n presses Enter, \t presses Tab
    key COMBO [TARGET] [--repeat N]         e.g. ctrl+s, alt+f4, win+r, enter, f5
    focus [TITLE] [--process P]             restore and bring a window to the front

SEE AND WAIT
    ocr [TARGET] [--find TEXT]              recognized text with screen rectangles
    elements [TARGET] [--name N] [--type T] [--max N]   UI Automation controls
    wait window [TITLE] [--process P] [--gone] [--timeout-ms N]
    wait text TEXT [TARGET] [--gone] [--timeout-ms N]
    sleep MS                                pause (client side)

OTHER
    open [PATH]                             open a capture in the editor (or show the editor)
    quit                                    exit Jiffshot
    run FILE.json                           run a scripted sequence (see `help agent`)
    call COMMAND [JSON]                     send a raw command with a JSON arguments object
    help [agent]                            this text, or the guide for AI agents

TARGET (what to capture, look at or type into; default: the screen under the pointer)
    --screen N | --desktop | --region X,Y,W,H
    --window HWND | --title TEXT | --process NAME | --class NAME   (windows come to the front
    first unless --no-activate)

POINTER (where to click, move or scroll; in screen pixels)
    X,Y | --text TEXT (found by OCR in TARGET) | --element NAME [--type T] (UI Automation in
    TARGET) and optionally --nth N (1-based match), --offset DX,DY

EXIT CODES
    0 success, 1 failed, 2 invalid usage, 3 Jiffshot unavailable, 4 timed out
"#;

const AGENT: &str = r#"jiffshot-cli for AI agents
=========================

Jiffshot is a Windows screen capture and recording app. jiffshot-cli lets you
screenshot, record and narrate a demo of any desktop software by driving the
mouse and keyboard yourself. It talks to the running Jiffshot (starting it if
needed) and prints exactly one JSON object per invocation on stdout:

    {"ok":true,"result":{...}}
    {"ok":false,"error":{"code":"not_found","message":"..."}}

Check "ok" (or the exit code: 0 ok, 1 failed, 2 bad usage, 3 unavailable,
4 timed out). Error codes: invalid_request, not_found, conflict, unavailable,
timeout, unsupported, transport, internal. Paths in results are absolute.
Never parse stderr; it only carries diagnostics.

RECOMMENDED LOOP
1. Look before you act: `windows`, `monitors`, `screenshot --out x.png` (then
   view the PNG), `ocr --title App`, `elements --title App`.
2. Act with targets rather than guessed coordinates:
   `click --title App --element "Save"` or `click --title App --text "Next"`.
   Coordinates are absolute screen pixels; `--offset DX,DY` nudges a match.
3. Synchronize with bounded waits, never blind sleeps:
   `wait window "Settings" --timeout-ms 10000`,
   `wait text "Completed" --title App --timeout-ms 30000`.
4. Verify with another screenshot or OCR before moving on.

RECORDING A DEMO
    jiffshot-cli focus --process notepad
    jiffshot-cli record start --process notepad --out C:\demo\demo.mp4 --clicks --key demo-1
    jiffshot-cli type "Hello from an agent\n"
    jiffshot-cli key ctrl+s
    jiffshot-cli wait window "Save as" --timeout-ms 5000
    jiffshot-cli record stop

`record start` returns only once frames are flowing, so actions after it are
on video. `record stop` returns after the MP4 is finalized with its path,
size, duration and frame count. Pass `--key` (any unique string) so that
retrying a start after a lost reply does not start a second recording. While
recording, pointer moves glide (--glide-ms, default 400) and typing is paced
(--delay-ms, default 35) so viewers can follow; pass 0 to disable.
A window target records that window's rectangle at start; keep the window
still. Only one recording runs at a time (conflict otherwise).

STEP-BY-STEP GUIDES
    jiffshot-cli steps start "How to save a file"
    jiffshot-cli click --title Notepad --element File --description "Open the File menu"
    jiffshot-cli click --title Notepad --element "Save as"
    jiffshot-cli steps finish --out C:\demo\guide.png

While a guide is in progress every click adds a step (screen captured just
before the click, with the click marked). Without --description the step is
captioned from UI Automation. Use `--no-step` to click without a step and
`steps add X,Y --description ...` to add one without clicking.

SCRIPTED SEQUENCES: `jiffshot-cli run demo.json`
    {
      "continue_on_error": false,
      "steps": [
        {"command": "focus", "args": {"process": "notepad"}},
        {"command": "record.start", "args": {"process": "notepad", "out": "C:\\demo\\a.mp4"}},
        {"argv": ["type", "Hello\n"]},
        {"sleep_ms": 500},
        {"argv": ["wait", "text", "Hello", "--process", "notepad"], "continue_on_error": true}
      ],
      "finally": [
        {"command": "record.stop"}
      ]
    }
Steps take either a command (dotted name as listed by `status`) with an args
object, an argv array (the same words you would type after jiffshot-cli), or
sleep_ms. The run stops at the first failure unless continue_on_error, then
runs "finally" (use it to stop recordings). The result lists every step with
its result or error and elapsed milliseconds; the exit code is that of the
first failure.

TIPS
- Target the exact window: list `windows` first and pass `--window HWND`.
  Many apps (Notepad, browsers, Office) open new documents as tabs in an
  existing window, so `--process` alone may pick a window the user is using.
- Windows come to the front when targeted; pass --no-activate to just look.
- OCR text matching ignores case and finds the smallest run of words; use
  --nth to pick among repeats.
- `status` lists every command the running Jiffshot supports.
- Typed text is sent as Unicode, independent of the keyboard layout.
"#;

/// Options that take no value.
const FLAGS: &[&str] = &[
    "desktop", "no-activate", "cursor", "no-cursor", "clicks", "highlight", "right", "middle", "double", "open", "restart", "gone",
    "horizontal", "all", "step", "no-step",
];

/// Options holding paths, made absolute against the caller's directory
/// (Jiffshot runs elsewhere).
const PATHS: &[&str] = &["out", "path"];

/// What each command's positional arguments mean. A trailing `*` joins the
/// rest with spaces.
const POSITIONALS: &[(&str, &[&str])] = &[
    ("windows", &["title"]),
    ("screenshot", &[]),
    ("record.start", &[]),
    ("steps.start", &["title*"]),
    ("steps.add", &["at"]),
    ("click", &["at"]),
    ("move", &["at"]),
    ("drag", &["from", "to"]),
    ("scroll", &["amount"]),
    ("type", &["text*"]),
    ("key", &["keys"]),
    ("focus", &["title*"]),
    ("ocr", &[]),
    ("elements", &[]),
    ("wait.window", &["title*"]),
    ("wait.text", &["text*"]),
    ("open", &["path"]),
];

/// Command groups written as two words (`record start`).
const GROUPS: &[&str] = &["record", "steps", "wait"];

#[derive(Debug, Default, PartialEq)]
struct Globals {
    pretty: bool,
    timeout: Option<Duration>,
    no_launch: bool,
}

#[derive(Debug, PartialEq)]
enum Action {
    Help(String),
    Sleep(u64),
    Run(PathBuf),
    Call { command: String, args: Map<String, Value> },
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

/// Splits off the global options anywhere on the line.
fn globals(argv: &[String]) -> Result<(Globals, Vec<String>, Map<String, Value>), Error> {
    let mut g = Globals::default();
    let mut rest = Vec::new();
    let mut extra = Map::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pretty" => g.pretty = true,
            "--no-launch" => g.no_launch = true,
            "--timeout" => {
                let v = it.next().ok_or_else(|| usage("--timeout needs a number of seconds"))?;
                let secs: f64 = v.parse().ok().filter(|s: &f64| *s > 0.0).ok_or_else(|| usage(format!("--timeout must be positive seconds, not {v:?}")))?;
                g.timeout = Some(Duration::from_secs_f64(secs));
            }
            "--args" => {
                let v = it.next().ok_or_else(|| usage("--args needs a JSON object"))?;
                match serde_json::from_str(v) {
                    Ok(Value::Object(m)) => extra.extend(m),
                    _ => return Err(usage("--args must be a JSON object")),
                }
            }
            _ => rest.push(a.clone()),
        }
    }
    Ok((g, rest, extra))
}

/// Parses a command line (without the global options).
fn parse(words: &[String], cwd: &Path) -> Result<Action, Error> {
    let Some(first) = words.first() else {
        return Ok(Action::Help(String::new()));
    };
    let mut rest = &words[1..];
    let command = match first.as_str() {
        "help" | "--help" | "-h" | "/?" => return Ok(Action::Help(rest.first().cloned().unwrap_or_default())),
        "--help-agent" => return Ok(Action::Help("agent".into())),
        "sleep" => {
            let ms = rest.first().and_then(|s| s.parse().ok()).ok_or_else(|| usage("sleep needs milliseconds"))?;
            return Ok(Action::Sleep(ms));
        }
        "run" => {
            let [file] = rest else { return Err(usage("run needs one script file")) };
            return Ok(Action::Run(cwd.join(file)));
        }
        "call" => {
            let command = rest.first().ok_or_else(|| usage("call needs a command"))?.clone();
            let args = match rest.get(1) {
                None => Map::new(),
                Some(s) => match serde_json::from_str(s) {
                    Ok(Value::Object(m)) => m,
                    _ => return Err(usage("call's arguments must be a JSON object")),
                },
            };
            return Ok(Action::Call { command, args });
        }
        group if GROUPS.contains(&group) => {
            let sub = rest.first().filter(|s| !s.starts_with("--")).ok_or_else(|| usage(format!("{group} needs a subcommand; see help")))?;
            rest = &rest[1..];
            format!("{group}.{sub}")
        }
        other => other.to_string(),
    };

    let slots = POSITIONALS.iter().find(|(c, _)| *c == command).map_or(&[][..], |(_, s)| *s);
    let mut args = Map::new();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let word = &rest[i];
        i += 1;
        let Some(option) = word.strip_prefix("--").filter(|o| !o.is_empty()) else {
            positional.push(word.clone());
            continue;
        };
        let (name, inline) = match option.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (option, None),
        };
        if name == "no-step" {
            args.insert("step".into(), json!(false));
            continue;
        }
        let key = name.replace('-', "_");
        let value = match inline {
            Some(v) => json!(v),
            None if FLAGS.contains(&name) => json!(true),
            None => {
                let v = rest.get(i).ok_or_else(|| usage(format!("--{name} needs a value")))?;
                i += 1;
                json!(v)
            }
        };
        args.insert(key, value);
    }

    let mut slot = slots.iter();
    let mut words = positional.into_iter();
    while let Some(word) = words.next() {
        let Some(s) = slot.next() else {
            return Err(usage(format!("unexpected argument {word:?} for {command}")));
        };
        let value = match s.strip_suffix('*') {
            Some(_) => std::iter::once(word).chain(words.by_ref()).collect::<Vec<_>>().join(" "),
            None => word,
        };
        args.insert(s.trim_end_matches('*').to_string(), json!(value));
    }

    for key in PATHS {
        if let Some(Value::String(p)) = args.get(*key) {
            args.insert(key.to_string(), json!(cwd.join(p).to_string_lossy()));
        }
    }
    Ok(Action::Call { command, args })
}

/// How long to wait for a command's reply by default.
fn default_timeout(command: &str, args: &Map<String, Value>) -> Duration {
    let ms = |k: &str| args.get(k).and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
    match command {
        "wait.window" | "wait.text" => Duration::from_millis(ms("timeout_ms").unwrap_or(10_000.0) as u64) + Duration::from_secs(15),
        "record.stop" | "record.cancel" | "steps.finish" => Duration::from_secs(300),
        _ => Duration::from_secs(60),
    }
}

/// The Jiffshot executable to start: `JIFFSHOT_EXE`, or the one beside this.
fn jiffshot_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("JIFFSHOT_EXE") {
        return Some(PathBuf::from(p));
    }
    let beside = std::env::current_exe().ok()?.with_file_name("jiffshot.exe");
    beside.exists().then_some(beside)
}

/// Starts Jiffshot in the tray and waits until it answers.
fn launch() -> Result<(), Error> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let exe = jiffshot_exe().ok_or_else(|| {
        Error::new(ErrorCode::Unavailable, "Jiffshot is not running and jiffshot.exe was not found beside jiffshot-cli (set JIFFSHOT_EXE)")
    })?;
    eprintln!("jiffshot-cli: starting {}", exe.display());
    let mut child = std::process::Command::new(&exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::new(ErrorCode::Unavailable, format!("cannot start {}: {e}", exe.display())))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exited: Option<Instant> = None;
    loop {
        match jiffshot_control::call("status", json!({}), Duration::from_secs(5)) {
            Ok(_) => return Ok(()),
            Err(e) if e.code != ErrorCode::Unavailable => return Err(e),
            Err(_) => {}
        }
        // An exiting launch may have handed over to an instance that is still
        // starting; give that a moment before giving up.
        if let Ok(Some(status)) = child.try_wait() {
            if exited.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
                return Err(Error::new(ErrorCode::Unavailable, format!("Jiffshot exited ({status}) without answering")));
            }
        }
        if Instant::now() > deadline {
            return Err(Error::new(ErrorCode::Unavailable, "Jiffshot started but did not answer within 30 s"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Sends one command, starting Jiffshot first when it is not running.
fn call(command: &str, args: Map<String, Value>, g: &Globals) -> Result<Value, Error> {
    let timeout = g.timeout.unwrap_or_else(|| default_timeout(command, &args));
    let args = Value::Object(args);
    match jiffshot_control::call(command, args.clone(), timeout) {
        Err(e) if e.code == ErrorCode::Unavailable && command == "quit" => Ok(json!({ "running": false })),
        Err(e) if e.code == ErrorCode::Unavailable && !g.no_launch => {
            launch()?;
            jiffshot_control::call(command, args, timeout)
        }
        other => other,
    }
}

fn error_json(e: &Error) -> Value {
    json!({ "code": e.code.as_str(), "message": e.message })
}

/// Runs one step of a script: its result (or error) and elapsed time.
fn run_step(step: &Value, g: &Globals, cwd: &Path) -> (Value, Option<Error>) {
    let started = Instant::now();
    let outcome = (|| -> Result<(String, Value), (String, Error)> {
        if let Some(ms) = step.get("sleep_ms") {
            let ms = ms.as_u64().ok_or_else(|| ("sleep".to_string(), usage("sleep_ms must be milliseconds")))?;
            std::thread::sleep(Duration::from_millis(ms));
            return Ok(("sleep".into(), json!({ "slept_ms": ms })));
        }
        let action = if let Some(argv) = step.get("argv") {
            let words: Option<Vec<String>> = argv.as_array().and_then(|a| a.iter().map(|w| w.as_str().map(str::to_owned)).collect());
            let words = words.ok_or_else(|| ("argv".to_string(), usage("argv must be an array of strings")))?;
            parse(&words, cwd).map_err(|e| (words.join(" "), e))?
        } else if let Some(command) = step.get("command").and_then(Value::as_str) {
            let args = match step.get("args") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(m)) => m.clone(),
                Some(_) => return Err((command.to_string(), usage("args must be an object"))),
            };
            Action::Call { command: command.to_string(), args }
        } else {
            return Err((String::new(), usage("a step needs command, argv or sleep_ms")));
        };
        match action {
            Action::Call { command, args } => call(&command, args, g).map(|r| (command.clone(), r)).map_err(|e| (command, e)),
            Action::Sleep(ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                Ok(("sleep".into(), json!({ "slept_ms": ms })))
            }
            Action::Help(_) | Action::Run(_) => Err((String::new(), usage("help and run cannot be script steps"))),
        }
    })();
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok((command, result)) => (json!({ "command": command, "ok": true, "result": result, "ms": ms }), None),
        Err((command, e)) => (json!({ "command": command, "ok": false, "error": error_json(&e), "ms": ms }), Some(e)),
    }
}

/// Runs a script: its steps until one fails, then its `finally` steps. The
/// report of every step, and the first failure.
fn run(file: &Path, g: &Globals, cwd: &Path) -> Result<(Value, Option<Error>), Error> {
    let text = std::fs::read_to_string(file).map_err(|e| usage(format!("cannot read {}: {e}", file.display())))?;
    let script: Value = serde_json::from_str(&text).map_err(|e| usage(format!("{} is not valid JSON: {e}", file.display())))?;
    let (steps, finally, keep_going) = match &script {
        Value::Array(steps) => (steps.clone(), Vec::new(), false),
        Value::Object(o) => (
            o.get("steps").and_then(Value::as_array).cloned().ok_or_else(|| usage("the script needs a steps array"))?,
            o.get("finally").and_then(Value::as_array).cloned().unwrap_or_default(),
            o.get("continue_on_error").and_then(Value::as_bool).unwrap_or(false),
        ),
        _ => return Err(usage("the script must be a steps array or an object with steps")),
    };

    let mut first_error: Option<Error> = None;
    let mut report = Vec::new();
    let mut stopped = false;
    for (index, step) in steps.iter().enumerate() {
        let (mut entry, error) = run_step(step, g, cwd);
        entry["index"] = json!(index);
        eprintln!("jiffshot-cli: step {index} {} {}", entry["command"].as_str().unwrap_or(""), if error.is_some() { "failed" } else { "ok" });
        report.push(entry);
        if let Some(e) = error {
            let tolerate = step.get("continue_on_error").and_then(Value::as_bool).unwrap_or(keep_going);
            first_error.get_or_insert(e);
            if !tolerate {
                stopped = true;
                break;
            }
        }
    }
    let mut cleanup = Vec::new();
    for (index, step) in finally.iter().enumerate() {
        let (mut entry, error) = run_step(step, g, cwd);
        entry["index"] = json!(index);
        cleanup.push(entry);
        if let Some(e) = error {
            first_error.get_or_insert(e);
        }
    }
    let summary = json!({ "steps": report, "finally": cleanup, "stopped_early": stopped, "failed": first_error.as_ref().map(error_json) });
    Ok((summary, first_error))
}

fn print(value: &Value, pretty: bool) {
    let text = if pretty { serde_json::to_string_pretty(value) } else { serde_json::to_string(value) };
    println!("{}", text.unwrap_or_default());
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    let outcome = globals(&argv).and_then(|(g, words, extra)| {
        let action = parse(&words, &cwd)?;
        Ok((g, action, extra))
    });
    let (g, action, extra) = match outcome {
        Ok(v) => v,
        Err(e) => {
            print(&json!({ "ok": false, "error": error_json(&e) }), false);
            eprintln!("jiffshot-cli: {}; see `jiffshot-cli help`", e.message);
            return ExitCode::from(2);
        }
    };

    // Jiffshot brings target windows to the front; let it.
    // SAFETY: plain Win32 call.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX) };

    let result = match action {
        Action::Help(topic) => {
            print!("{}", if topic.eq_ignore_ascii_case("agent") { AGENT } else { USAGE });
            return ExitCode::SUCCESS;
        }
        Action::Sleep(ms) => {
            std::thread::sleep(Duration::from_millis(ms));
            Ok(json!({ "slept_ms": ms }))
        }
        // A failed script still reports every step it ran.
        Action::Run(file) => match run(&file, &g, &cwd) {
            Ok((report, None)) => Ok(report),
            Ok((report, Some(e))) => Err((e, report)),
            Err(e) => Err((e, Value::Null)),
        },
        Action::Call { command, mut args } => {
            args.extend(extra);
            call(&command, args, &g).map_err(|e| (e, Value::Null))
        }
    };

    match result {
        Ok(value) => {
            print(&json!({ "ok": true, "result": value }), g.pretty);
            ExitCode::SUCCESS
        }
        Err((e, report)) => {
            let mut out = json!({ "ok": false, "error": error_json(&e) });
            if !report.is_null() {
                out["result"] = report;
            }
            print(&out, g.pretty);
            eprintln!("jiffshot-cli: {}", e.message);
            ExitCode::from(e.code.exit_code().clamp(1, 255) as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    fn call_of(s: &str) -> (String, Value) {
        match parse(&words(s), Path::new(r"C:\work")).unwrap() {
            Action::Call { command, args } => (command, Value::Object(args)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn groups_become_dotted_commands() {
        assert_eq!(call_of("record start --process notepad --fps 30"), ("record.start".into(), json!({ "process": "notepad", "fps": "30" })));
        assert_eq!(call_of("wait text Saved file --gone"), ("wait.text".into(), json!({ "text": "Saved file", "gone": true })));
        assert_eq!(call_of("steps start How to save"), ("steps.start".into(), json!({ "title": "How to save" })));
        assert!(parse(&words("record"), Path::new(".")).is_err());
    }

    #[test]
    fn positionals_options_and_flags() {
        assert_eq!(call_of("click 10,20 --double --no-step"), ("click".into(), json!({ "at": "10,20", "double": true, "step": false })));
        assert_eq!(call_of("drag 1,2 3,4"), ("drag".into(), json!({ "from": "1,2", "to": "3,4" })));
        assert_eq!(call_of("scroll -3 --text=Docs"), ("scroll".into(), json!({ "amount": "-3", "text": "Docs" })));
        assert_eq!(call_of("click --element Save --no-activate --title App"), ("click".into(), json!({ "element": "Save", "no_activate": true, "title": "App" })));
        assert!(parse(&words("key ctrl+s extra"), Path::new(".")).is_err());
        assert!(parse(&words("type --delay-ms"), Path::new(".")).is_err());
    }

    #[test]
    fn paths_are_made_absolute() {
        let (_, args) = call_of(r"screenshot --out shots\a.png");
        assert_eq!(args["out"], json!(r"C:\work\shots\a.png"));
        let (_, args) = call_of(r"screenshot --out D:\b.png");
        assert_eq!(args["out"], json!(r"D:\b.png"));
    }

    #[test]
    fn globals_anywhere() {
        let (g, rest, extra) = globals(&words(r#"--pretty click --timeout 5 1,1 --no-launch --args {"glide_ms":0}"#)).unwrap();
        assert_eq!(g, Globals { pretty: true, timeout: Some(Duration::from_secs(5)), no_launch: true });
        assert_eq!(rest, words("click 1,1"));
        assert_eq!(Value::Object(extra), json!({ "glide_ms": 0 }));
        assert!(globals(&words("--timeout 0")).is_err());
    }

    #[test]
    fn client_side_actions() {
        assert_eq!(parse(&[], Path::new(".")).unwrap(), Action::Help(String::new()));
        assert_eq!(parse(&words("help agent"), Path::new(".")).unwrap(), Action::Help("agent".into()));
        assert_eq!(parse(&words("sleep 250"), Path::new(".")).unwrap(), Action::Sleep(250));
        assert_eq!(parse(&words("run a.json"), Path::new(r"C:\w")).unwrap(), Action::Run(PathBuf::from(r"C:\w\a.json")));
        let Action::Call { command, args } = parse(&[String::from("call"), "record.stop".into(), r#"{"id":"r1"}"#.into()], Path::new(".")).unwrap()
        else {
            panic!()
        };
        assert_eq!((command.as_str(), Value::Object(args)), ("record.stop", json!({ "id": "r1" })));
    }

    #[test]
    fn wait_timeouts_cover_the_wait() {
        let args = json!({ "timeout_ms": "60000" }).as_object().unwrap().clone();
        assert_eq!(default_timeout("wait.text", &args), Duration::from_secs(75));
        assert_eq!(default_timeout("click", &Map::new()), Duration::from_secs(60));
    }

    #[test]
    fn script_steps_report_errors() {
        let g = Globals { no_launch: true, ..Globals::default() };
        let (entry, error) = run_step(&json!({ "sleep_ms": 1 }), &g, Path::new("."));
        assert!(error.is_none() && entry["ok"] == json!(true));
        let (entry, error) = run_step(&json!({ "nothing": 1 }), &g, Path::new("."));
        assert_eq!(error.unwrap().code, ErrorCode::InvalidRequest);
        assert_eq!(entry["error"]["code"], json!("invalid_request"));
    }
}
