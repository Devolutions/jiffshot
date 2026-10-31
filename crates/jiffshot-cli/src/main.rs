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

const USAGE: &str = r##"jiffshot-cli: drive the running Jiffshot from scripts and AI agents

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
    capture scroll [TARGET] [--out FILE.png] [--horizontal] [--max-frames N]
                   [--timeout-ms N] [--settle-ms N] [--notches N]
    record start [TARGET] [--out FILE.mp4] [--fps N] [--audio none|system|mic|both]
                 [--clicks] [--highlight] [--no-cursor] [--max-seconds N] [--key ID]
    record status | pause | resume
    record stop [--id ID]                   waits until the MP4 is final; returns its metadata
    record cancel [--id ID]                 stops and deletes the recording
    record options [--id ID] [--args JSON]  change camera/audio/cursor switches
    record annotate --kind JSON [--style JSON] [--duration-ms N]
    record clear [--id ID]                 clear live annotations
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

EDIT IMAGES AND ANNOTATIONS (document pixels, separate from open GUI documents)
    doc tools | list                      discover tools/styles/effects, or list sessions
    doc new --width W --height H [--color "#RRGGBB"]
    doc open PATH                         load an image or .jsd, returns a document id
    doc status --id ID                     objects, selection, undo/redo and dimensions
    doc draw --id ID --tool NAME --from X,Y --to X,Y [--text T] [--variant V]
             [--style JSON] [--points JSON] [--amount N]
    doc add --id ID --kind JSON [--style JSON]
    doc update --id ID --object-id N [--kind JSON] [--style JSON] [--rotation N]
    doc image PATH --id ID [--at X,Y]
    doc select --id ID --object-ids JSON   select an array of object ids
    doc action NAME --id ID                undo/redo, delete, group, stacking, transforms
    doc crop --id ID --x X --y Y --width W --height H
    doc resize --id ID --width W --height H [--canvas] [--color "#RRGGBB"]
    doc effect NAME --id ID                named effect (see doc tools)
    doc ocr | redact | simplify --id ID    OCR, sensitive-pattern redaction, UI simplification
    doc combine --document-ids JSON [--layout vertical|horizontal|grid]
                [--template NAME] [--theme NAME] [--captions] [--numbers] [--footer TEXT]
    doc save --id ID --out FILE.jsd        editable document
    doc export --id ID --out FILE          PNG/JPEG/BMP/GIF/WebP/PDF
    doc close --id ID                      release the document

EDIT VIDEO
    video info PATH                       dimensions, duration, fps and audio
    video frame PATH --at N --out FILE.png
    video export PATH --out FILE.mp4|gif [--from N] [--to N] [--mute]
                 [--fps N] [--width W --height H] [--overlays JSON]
    video join --segments JSON --out FILE.mp4 [--mute] [--overlays JSON]

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
"##;

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

EDITING AND LIVE ANNOTATIONS
- `doc open screenshot.png` returns an id for a headless editable document.
  Use `doc draw --id ID --tool Arrow --from 10,20 --to 200,80` or other
  tools from `doc tools`: text, callouts, steps, shapes, pen, highlights,
  blur, pixelation, redaction, spotlight and magnification. `--style` is JSON.
  Save .jsd with `doc save`, export with `doc export`, then `doc close`.
- `record annotate --kind JSON --style JSON --duration-ms 3000` renders
  annotations into recorded frames without stealing desktop input. Coordinates
  are recording-local. A zero duration persists until `record clear`.
- `record options --args '{"microphone":false,"webcam":true}'` changes
  live switches. Start with --webcam, --webcam-corner, --webcam-size,
  --webcam-circle and --noise-gate as needed.
- `video info`, `video frame`, `video export` and `video join` edit saved
  recordings without operating the editor GUI. Seconds select trim boundaries.
- Script steps may use `"save_as":"shot"` to retain a successful result;
  an argument `{"$ref":"shot.path"}` reuses its path with the original JSON
  type. References also work in cleanup. See docs/cli.md for full examples
  and the capability coverage/limitations.
"#;

/// Options that take no value.
const FLAGS: &[&str] = &[
    "desktop",
    "no-activate",
    "cursor",
    "no-cursor",
    "clicks",
    "highlight",
    "right",
    "middle",
    "double",
    "open",
    "restart",
    "gone",
    "horizontal",
    "all",
    "step",
    "no-step",
    "canvas",
    "webcam",
    "webcam-circle",
    "noise-gate",
    "mute",
    "shift",
    "ctrl",
    "alt",
    "captions",
    "numbers",
];

/// Options holding paths, made absolute against the caller's directory
/// (Jiffshot runs elsewhere).
const PATHS: &[&str] = &["out", "path"];
const JSON_OPTIONS: &[&str] = &[
    "kind",
    "style",
    "points",
    "object-ids",
    "document-ids",
    "segments",
    "overlays",
];

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
    ("doc.open", &["path"]),
    ("doc.image", &["path"]),
    ("doc.action", &["name"]),
    ("doc.effect", &["name*"]),
    ("video.info", &["path"]),
    ("video.frame", &["path"]),
    ("video.export", &["path"]),
];

/// Command groups written as two words (`record start`).
const GROUPS: &[&str] = &["record", "steps", "wait", "doc", "video", "capture"];

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
    Call {
        command: String,
        args: Map<String, Value>,
    },
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

type GlobalArguments = (Globals, Vec<String>, Map<String, Value>);

/// Splits off the global options anywhere on the line.
fn globals(argv: &[String]) -> Result<GlobalArguments, Error> {
    let mut g = Globals::default();
    let mut rest = Vec::new();
    let mut extra = Map::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pretty" => g.pretty = true,
            "--no-launch" => g.no_launch = true,
            "--timeout" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--timeout needs a number of seconds"))?;
                let secs: f64 = v
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s > 0.0)
                    .ok_or_else(|| {
                        usage(format!("--timeout must be positive seconds, not {v:?}"))
                    })?;
                g.timeout = Some(
                    Duration::try_from_secs_f64(secs)
                        .map_err(|_| usage("--timeout is outside the supported duration range"))?,
                );
            }
            "--args" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--args needs a JSON object"))?;
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
        "help" | "--help" | "-h" | "/?" => {
            return Ok(Action::Help(rest.first().cloned().unwrap_or_default()))
        }
        "--help-agent" => return Ok(Action::Help("agent".into())),
        "sleep" => {
            let ms = rest
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| usage("sleep needs milliseconds"))?;
            return Ok(Action::Sleep(ms));
        }
        "run" => {
            let [file] = rest else {
                return Err(usage("run needs one script file"));
            };
            return Ok(Action::Run(cwd.join(file)));
        }
        "call" => {
            let command = rest
                .first()
                .ok_or_else(|| usage("call needs a command"))?
                .clone();
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
            let sub = rest
                .first()
                .filter(|s| !s.starts_with("--"))
                .ok_or_else(|| usage(format!("{group} needs a subcommand; see help")))?;
            rest = &rest[1..];
            format!("{group}.{sub}")
        }
        other => other.to_string(),
    };

    let slots = POSITIONALS
        .iter()
        .find(|(c, _)| *c == command)
        .map_or(&[][..], |(_, s)| *s);
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
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage(format!("--{name} needs a value")))?;
                i += 1;
                json!(v)
            }
        };
        let value = if JSON_OPTIONS.contains(&name) {
            serde_json::from_str(
                value
                    .as_str()
                    .ok_or_else(|| usage(format!("--{name} needs JSON")))?,
            )
            .map_err(|e| usage(format!("--{name}: {e}")))?
        } else {
            value
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
            Some(_) => std::iter::once(word)
                .chain(words.by_ref())
                .collect::<Vec<_>>()
                .join(" "),
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
    let ms = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
    };
    match command {
        "wait.window" | "wait.text" => {
            Duration::from_millis(ms("timeout_ms").unwrap_or(10_000.0) as u64)
                + Duration::from_secs(15)
        }
        "capture.scroll" => {
            Duration::from_millis(ms("timeout_ms").unwrap_or(30_000.0) as u64)
                + Duration::from_secs(15)
        }
        "record.stop" | "record.cancel" | "steps.finish" | "video.export" | "video.join" => {
            Duration::from_secs(300)
        }
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
        .map_err(|e| {
            Error::new(
                ErrorCode::Unavailable,
                format!("cannot start {}: {e}", exe.display()),
            )
        })?;
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
                return Err(Error::new(
                    ErrorCode::Unavailable,
                    format!("Jiffshot exited ({status}) without answering"),
                ));
            }
        }
        if Instant::now() > deadline {
            return Err(Error::new(
                ErrorCode::Unavailable,
                "Jiffshot started but did not answer within 30 s",
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Sends one command, starting Jiffshot first when it is not running.
fn call(command: &str, mut args: Map<String, Value>, g: &Globals) -> Result<Value, Error> {
    absolute_paths(
        &mut args,
        &std::env::current_dir().map_err(|e| usage(format!("current directory: {e}")))?,
    );
    let timeout = g.timeout.unwrap_or_else(|| default_timeout(command, &args));
    let args = Value::Object(args);
    match jiffshot_control::call(command, args.clone(), timeout) {
        Err(e) if e.code == ErrorCode::Unavailable && command == "quit" => {
            Ok(json!({ "running": false }))
        }
        Err(e) if e.code == ErrorCode::Unavailable && !g.no_launch => {
            launch()?;
            jiffshot_control::call(command, args, timeout)
        }
        other => other,
    }
}
fn absolute_paths(args: &mut Map<String, Value>, cwd: &Path) {
    for key in PATHS {
        if let Some(Value::String(p)) = args.get(*key) {
            args.insert((*key).to_string(), json!(cwd.join(p)));
        }
    }
    for key in ["segments", "overlays"] {
        if let Some(Value::Array(items)) = args.get_mut(key) {
            for item in items {
                if let Some(fields) = item.as_object_mut() {
                    absolute_paths(fields, cwd);
                }
            }
        }
    }
}

/// Exact reference objects preserve JSON types and never interpolate code.
fn resolve_refs(value: &Value, bindings: &Map<String, Value>) -> Result<Value, Error> {
    match value {
        Value::Object(fields) if fields.contains_key("$ref") => {
            if fields.len() != 1 {
                return Err(usage("a $ref object must have exactly one field"));
            }
            let reference = fields["$ref"]
                .as_str()
                .ok_or_else(|| usage("$ref must be text"))?;
            let mut parts = reference.split('.');
            let name = parts.next().ok_or_else(|| usage("empty reference"))?;
            let mut result = bindings
                .get(name)
                .ok_or_else(|| usage(format!("no saved result {name:?}")))?;
            for field in parts {
                result = match result {
                    Value::Array(items) => field.parse::<usize>().ok().and_then(|i| items.get(i)),
                    Value::Object(fields) => fields.get(field),
                    _ => None,
                }
                .ok_or_else(|| usage(format!("reference {reference:?} does not exist")))?;
            }
            Ok(result.clone())
        }
        Value::Object(fields) => fields
            .iter()
            .map(|(k, v)| Ok((k.clone(), resolve_refs(v, bindings)?)))
            .collect::<Result<Map<String, Value>, Error>>()
            .map(Value::Object),
        Value::Array(items) => items
            .iter()
            .map(|v| resolve_refs(v, bindings))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Ok(value.clone()),
    }
}

fn bound_step(
    step: &Value,
    bindings: &mut Map<String, Value>,
    g: &Globals,
    cwd: &Path,
) -> (Value, Option<Error>) {
    let resolved = match resolve_refs(step, bindings) {
        Ok(step) => step,
        Err(error) => {
            return (
                json!({"command":step.get("command"),"ok":false,"error":error_json(&error),"ms":0}),
                Some(error),
            )
        }
    };
    let (entry, error) = run_step(&resolved, g, cwd);
    if error.is_none() {
        if let Some(name) = step.get("save_as").and_then(Value::as_str) {
            bindings.insert(name.to_owned(), entry["result"].clone());
        }
    }
    (entry, error)
}
fn error_json(e: &Error) -> Value {
    json!({ "code": e.code.as_str(), "message": e.message })
}

/// Runs one step of a script: its result (or error) and elapsed time.
fn run_step(step: &Value, g: &Globals, cwd: &Path) -> (Value, Option<Error>) {
    let started = Instant::now();
    let outcome = (|| -> Result<(String, Value), (String, Error)> {
        if let Some(ms) = step.get("sleep_ms") {
            let ms = ms
                .as_u64()
                .ok_or_else(|| ("sleep".to_string(), usage("sleep_ms must be milliseconds")))?;
            std::thread::sleep(Duration::from_millis(ms));
            return Ok(("sleep".into(), json!({ "slept_ms": ms })));
        }
        let action = if let Some(argv) = step.get("argv") {
            let words: Option<Vec<String>> = argv
                .as_array()
                .and_then(|a| a.iter().map(|w| w.as_str().map(str::to_owned)).collect());
            let words = words.ok_or_else(|| {
                (
                    "argv".to_string(),
                    usage("argv must be an array of strings"),
                )
            })?;
            parse(&words, cwd).map_err(|e| (words.join(" "), e))?
        } else if let Some(command) = step.get("command").and_then(Value::as_str) {
            let args = match step.get("args") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(m)) => m.clone(),
                Some(_) => return Err((command.to_string(), usage("args must be an object"))),
            };
            Action::Call {
                command: command.to_string(),
                args,
            }
        } else {
            return Err((
                String::new(),
                usage("a step needs command, argv or sleep_ms"),
            ));
        };
        match action {
            Action::Call { command, args } => call(&command, args, g)
                .map(|r| (command.clone(), r))
                .map_err(|e| (command, e)),
            Action::Sleep(ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                Ok(("sleep".into(), json!({ "slept_ms": ms })))
            }
            Action::Help(_) | Action::Run(_) => {
                Err((String::new(), usage("help and run cannot be script steps")))
            }
        }
    })();
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok((command, result)) => (
            json!({ "command": command, "ok": true, "result": result, "ms": ms }),
            None,
        ),
        Err((command, e)) => (
            json!({ "command": command, "ok": false, "error": error_json(&e), "ms": ms }),
            Some(e),
        ),
    }
}

/// Runs a script: its steps until one fails, then its `finally` steps. The
/// report of every step, and the first failure.
fn run(file: &Path, g: &Globals, cwd: &Path) -> Result<(Value, Option<Error>), Error> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| usage(format!("cannot read {}: {e}", file.display())))?;
    let script: Value = serde_json::from_str(&text)
        .map_err(|e| usage(format!("{} is not valid JSON: {e}", file.display())))?;
    let (steps, finally, keep_going) = match &script {
        Value::Array(steps) => (steps.clone(), Vec::new(), false),
        Value::Object(o) => (
            o.get("steps")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| usage("the script needs a steps array"))?,
            o.get("finally")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            o.get("continue_on_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        _ => {
            return Err(usage(
                "the script must be a steps array or an object with steps",
            ))
        }
    };
    if let Some(version) = script.get("version") {
        if version.as_u64() != Some(1) {
            return Err(usage("only script version 1 is supported"));
        }
    }
    if script.get("finally").is_some_and(|v| !v.is_array()) {
        return Err(usage("finally must be an array"));
    }
    let mut names = std::collections::HashSet::new();
    for step in steps.iter().chain(&finally) {
        if let Some(name) = step.get("save_as") {
            let name = name
                .as_str()
                .filter(|s| {
                    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .ok_or_else(|| usage("save_as must be a nonempty alphanumeric/underscore name"))?;
            if !names.insert(name) {
                return Err(usage(format!("duplicate save_as name {name:?}")));
            }
        }
    }

    let mut first_error: Option<Error> = None;
    let mut bindings = Map::new();
    let mut report = Vec::new();
    let mut stopped = false;
    for (index, step) in steps.iter().enumerate() {
        let (mut entry, error) = bound_step(step, &mut bindings, g, cwd);
        entry["index"] = json!(index);
        eprintln!(
            "jiffshot-cli: step {index} {} {}",
            entry["command"].as_str().unwrap_or(""),
            if error.is_some() { "failed" } else { "ok" }
        );
        report.push(entry);
        if let Some(e) = error {
            let tolerate = step
                .get("continue_on_error")
                .and_then(Value::as_bool)
                .unwrap_or(keep_going);
            first_error.get_or_insert(e);
            if !tolerate {
                stopped = true;
                break;
            }
        }
    }
    let mut cleanup = Vec::new();
    for (index, step) in finally.iter().enumerate() {
        let (mut entry, error) = bound_step(step, &mut bindings, g, cwd);
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
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
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
            print!(
                "{}",
                if topic.eq_ignore_ascii_case("agent") {
                    AGENT
                } else {
                    USAGE
                }
            );
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
        assert_eq!(
            call_of("record start --process notepad --fps 30"),
            (
                "record.start".into(),
                json!({ "process": "notepad", "fps": "30" })
            )
        );
        assert_eq!(
            call_of("wait text Saved file --gone"),
            (
                "wait.text".into(),
                json!({ "text": "Saved file", "gone": true })
            )
        );
        assert_eq!(
            call_of("steps start How to save"),
            ("steps.start".into(), json!({ "title": "How to save" }))
        );
        assert!(parse(&words("record"), Path::new(".")).is_err());
    }

    #[test]
    fn positionals_options_and_flags() {
        assert_eq!(
            call_of("click 10,20 --double --no-step"),
            (
                "click".into(),
                json!({ "at": "10,20", "double": true, "step": false })
            )
        );
        assert_eq!(
            call_of("drag 1,2 3,4"),
            ("drag".into(), json!({ "from": "1,2", "to": "3,4" }))
        );
        assert_eq!(
            call_of("scroll -3 --text=Docs"),
            ("scroll".into(), json!({ "amount": "-3", "text": "Docs" }))
        );
        assert_eq!(
            call_of("click --element Save --no-activate --title App"),
            (
                "click".into(),
                json!({ "element": "Save", "no_activate": true, "title": "App" })
            )
        );
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
        let (g, rest, extra) = globals(&words(
            r#"--pretty click --timeout 5 1,1 --no-launch --args {"glide_ms":0}"#,
        ))
        .unwrap();
        assert_eq!(
            g,
            Globals {
                pretty: true,
                timeout: Some(Duration::from_secs(5)),
                no_launch: true
            }
        );
        assert_eq!(rest, words("click 1,1"));
        assert_eq!(Value::Object(extra), json!({ "glide_ms": 0 }));
        assert!(globals(&words("--timeout 0")).is_err());
        assert!(globals(&words("--timeout inf")).is_err());
        assert!(globals(&words("--timeout NaN")).is_err());
        assert!(globals(&words("--timeout 1e300")).is_err());
    }

    #[test]
    fn client_side_actions() {
        assert_eq!(
            parse(&[], Path::new(".")).unwrap(),
            Action::Help(String::new())
        );
        assert_eq!(
            parse(&words("help agent"), Path::new(".")).unwrap(),
            Action::Help("agent".into())
        );
        assert_eq!(
            parse(&words("sleep 250"), Path::new(".")).unwrap(),
            Action::Sleep(250)
        );
        assert_eq!(
            parse(&words("run a.json"), Path::new(r"C:\w")).unwrap(),
            Action::Run(PathBuf::from(r"C:\w\a.json"))
        );
        let Action::Call { command, args } = parse(
            &[
                String::from("call"),
                "record.stop".into(),
                r#"{"id":"r1"}"#.into(),
            ],
            Path::new("."),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            (command.as_str(), Value::Object(args)),
            ("record.stop", json!({ "id": "r1" }))
        );
    }

    #[test]
    fn wait_timeouts_cover_the_wait() {
        let args = json!({ "timeout_ms": "60000" })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(default_timeout("wait.text", &args), Duration::from_secs(75));
        assert_eq!(
            default_timeout("click", &Map::new()),
            Duration::from_secs(60)
        );
        assert_eq!(
            default_timeout("video.export", &Map::new()),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn editor_json_options_preserve_structure() {
        let argv = vec![
            "doc",
            "add",
            "--id",
            "d1",
            "--kind",
            r#"{"type":"Pen","points":[{"x":1,"y":2},{"x":3,"y":4}]}"#,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let Action::Call { command, args } =
            parse(&argv, Path::new(r"C:\work")).expect("parse annotation")
        else {
            panic!("expected call")
        };
        assert_eq!(command, "doc.add");
        assert_eq!(args["kind"]["points"][1]["y"], 4);
        assert!(args["kind"].is_object());
        assert_eq!(
            call_of("doc resize --id d1 --width 20 --height 30 --canvas").1["canvas"],
            true
        );
    }

    #[test]
    fn references_reuse_typed_results_and_reject_missing_fields() {
        let bindings = json!({"capture":{"id":"d1","width":100,"objects":[{"id":7}]}})
            .as_object()
            .expect("bindings")
            .clone();
        let result = resolve_refs(&json!({"id":{"$ref":"capture.id"},"width":{"$ref":"capture.width"},"ids":[{"$ref":"capture.objects.0.id"}]}), &bindings).expect("resolve");
        assert_eq!(result, json!({"id":"d1","width":100,"ids":[7]}));
        assert!(resolve_refs(&json!({"$ref":"capture.missing"}), &bindings).is_err());
        assert!(resolve_refs(&json!({"$ref":"capture.id","extra":1}), &bindings).is_err());
        assert!(resolve_refs(&json!({"$ref":"missing.id"}), &bindings).is_err());
    }

    #[test]
    fn segment_paths_are_absolute_in_raw_commands() {
        let mut args =
            json!({"path":"in.mp4","out":"out.mp4","segments":[{"path":"a.mp4","from":1}]})
                .as_object()
                .expect("args")
                .clone();
        absolute_paths(&mut args, Path::new(r"C:\demo"));
        assert_eq!(args["path"], r"C:\demo\in.mp4");
        assert_eq!(args["segments"][0]["path"], r"C:\demo\a.mp4");
        assert_eq!(args["segments"][0]["from"], 1);
    }

    #[test]
    fn script_steps_report_errors() {
        let g = Globals {
            no_launch: true,
            ..Globals::default()
        };
        let (entry, error) = run_step(&json!({ "sleep_ms": 1 }), &g, Path::new("."));
        assert!(error.is_none() && entry["ok"] == json!(true));
        let (entry, error) = run_step(&json!({ "nothing": 1 }), &g, Path::new("."));
        assert_eq!(error.unwrap().code, ErrorCode::InvalidRequest);
        assert_eq!(entry["error"]["code"], json!("invalid_request"));
    }
}
