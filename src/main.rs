mod render;

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, StdoutLock, Write};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::Instant;

use render::{CurlTiming, Renderer};

/// Starts the line that `sjq curl ...` asks curl to print (via `-w`) after the response body.
const CURL_MARKER: &str = "@@sjq-curl-timing@@";

const HELP: &str = "\
sjq: read OpenAI-compatible /v1/chat/completions and /v1/responses streams at a glance

Usage: curl -N ... | sjq [OPTIONS]
       sjq [OPTIONS] curl [CURL ARGS]...
       sjq [OPTIONS] [FILE]...

Each `data:` chunk becomes one line with only what it carries:
  data: role=assistant                          first delta
  data: reasoning_content='The'                 reasoning delta
  data: ' user'                                 more of the field above
  data: content='¡'                             content delta, after reasoning
  data: ' in'                                   more content
  data: tool_calls[0] id=call_1 name=get_time   tool call delta
  data: finish_reason=stop, usage: {...}        final chunk (zero usage details dropped)
Text fields are named only when they change, and content only right after other text.
id / model / created are printed once per stream on a `meta:` line, and again when
they change. Unrecognized non-null fields are always shown. Other lines (event:,
comments, curl -w output) pass through; blank lines are dropped, and so are event:
lines that repeat the next payload's `type`.

Responses API events print their type without `response.`, e.g. `data: 'Hi'` for an
output_text.delta and `data: output_item.added message role=assistant id=msg_1`.
item_id / output_index / *_index show only when they change, sequence_number only when
it skips. Full text repeated by .done events is hidden when it matches the deltas and
shown in red when it doesn't.

A JSON body that isn't a stream (a `\"stream\": false` response, an HTTP error) goes to jq:
pretty in a terminal, one line otherwise. Without jq, or if jq can't parse it, it passes
through as-is. -t doesn't prefix jq's lines.

When the stream ends, a `time:` line reports:
  connect, tls, ttfb   from curl, only in `sjq curl ...` mode
  ttft                 first generated token (reasoning, content or tool call)
  first_content        first answer token, if reasoning or a tool call came first
  total                last line received
  speed                completion_tokens (or output_tokens) / (total - ttft)
Times count from when sjq started, which in `curl ... | sjq` is when the shell started
curl. A pipe only carries the body, so connect is only known when sjq runs curl itself:
`sjq curl ...` adds `-N -sS -w ...` (replacing any -w of yours) and exits with curl's
status. Saved captures (`sjq FILE`, `sjq < FILE`) get no `time:` line.

Options:
  -t, --time            prefix lines with seconds elapsed since sjq started
      --color <WHEN>    auto, always or never [default: auto, honors NO_COLOR]
  -h, --help            print help
  -V, --version         print version

Tip: use `curl -N`, otherwise curl buffers its output and chunks arrive in bursts.
";

struct Opts {
    files: Vec<String>,
    /// Arguments for `sjq curl ...`, when sjq runs the request itself.
    curl: Option<Vec<String>>,
    time: bool,
    color: bool,
    /// Whether stdout is a terminal, where jq pretty-prints JSON bodies instead of compacting them.
    pretty: bool,
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(Some(opts)) => opts,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("sjq: {msg}\nTry 'sjq --help' for more information.");
            return ExitCode::from(2);
        }
    };
    match run(&opts) {
        Ok(code) => code,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS, // e.g. `| head`
        Err(e) => {
            eprintln!("sjq: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Returns `None` when the invocation was fully handled (--help, --version).
fn parse_args() -> Result<Option<Opts>, String> {
    let mut files = Vec::new();
    let mut curl = None;
    let mut time = false;
    let mut color = "auto".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("sjq {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-t" | "--time" => time = true,
            "--color" => color = args.next().ok_or("--color needs a value")?,
            a if a.starts_with("--color=") => color = a["--color=".len()..].to_owned(),
            a if a.starts_with('-') && a != "-" => return Err(format!("unknown option '{a}'")),
            "curl" if files.is_empty() => {
                curl = Some(args.by_ref().collect());
                break;
            }
            _ => files.push(arg.clone()),
        }
    }
    let color = match color.as_str() {
        "always" => true,
        "never" => false,
        "auto" => io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        other => {
            return Err(format!(
                "invalid --color '{other}' (expected auto, always or never)"
            ));
        }
    };
    if files.is_empty() {
        files.push("-".to_owned());
    }
    Ok(Some(Opts {
        files,
        curl,
        time,
        color,
        pretty: io::stdout().is_terminal(),
    }))
}

fn run(opts: &Opts) -> io::Result<ExitCode> {
    let mut session = Session {
        opts,
        renderer: Renderer::new(opts.color),
        out: io::stdout().lock(),
        start: Instant::now(),
        jq: true,
    };
    if let Some(args) = &opts.curl {
        return session.curl(args);
    }
    for path in &opts.files {
        if path == "-" {
            session.pump(io::stdin().lock())?;
            if !stdin_is_file() {
                session.summary(None)?;
            }
        } else {
            let file =
                File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
            session.pump(BufReader::new(file))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether stdin is a redirected file (`sjq < capture`), whose timings would be meaningless.
fn stdin_is_file() -> bool {
    std::fs::metadata("/dev/stdin").is_ok_and(|m| m.is_file())
}

struct Session<'a> {
    opts: &'a Opts,
    renderer: Renderer,
    /// Stdout is line-buffered, so every line is flushed as soon as it is written.
    out: StdoutLock<'static>,
    /// When the request started: sjq's own start, or when it spawned curl.
    start: Instant,
    /// Whether jq can be started; cleared once it couldn't.
    jq: bool,
}

/// A JSON body (a `"stream": false` response, an HTTP error) that jq is printing.
struct JsonBody {
    jq: Child,
    /// jq's input, until a write fails because jq quit.
    stdin: Option<ChildStdin>,
    /// The body as received, passed through as-is if jq can't print it.
    raw: Vec<u8>,
}

impl JsonBody {
    fn feed(&mut self, chunk: &[u8]) {
        self.raw.extend_from_slice(chunk);
        if let Some(stdin) = &mut self.stdin
            && stdin.write_all(chunk).is_err()
        {
            self.stdin = None; // jq quit early, e.g. on a parse error
        }
    }
}

impl Session<'_> {
    /// Runs curl with the user's arguments and renders its output; returns curl's exit status.
    fn curl(&mut self, args: &[String]) -> io::Result<ExitCode> {
        let write_out = format!(
            "\n{CURL_MARKER} %{{time_connect}} %{{time_appconnect}} %{{time_starttransfer}}\n"
        );
        self.start = Instant::now();
        let mut child = Command::new("curl")
            .args(args)
            // -N: pass chunks on unbuffered; -sS: no progress meter, but still report errors
            .args(["-N", "-sS", "-w", &write_out])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("curl: {e}")))?;
        let stdout = child.stdout.take().expect("curl's stdout is piped");
        let curl = self.pump(BufReader::new(stdout))?;
        let status = child.wait()?;
        self.summary(curl.as_ref())?;
        Ok(status
            .code()
            .map_or(ExitCode::FAILURE, |c| ExitCode::from(c as u8)))
    }

    /// Renders `input` line by line; returns curl's timings if its `-w` line came through.
    fn pump(&mut self, mut input: impl BufRead) -> io::Result<Option<CurlTiming>> {
        let mut curl = None;
        let mut buf = Vec::new();
        let mut json = None;
        let mut sse = false;
        loop {
            buf.clear();
            let n = input.read_until(b'\n', &mut buf)?;
            let now = self.start.elapsed().as_secs_f64();
            if n == 0 {
                if let Some(json) = json {
                    self.end_json(json, now)?;
                }
                for rendered in self.renderer.finish() {
                    self.emit(now, &rendered)?;
                }
                return Ok(curl);
            }
            let raw = String::from_utf8_lossy(&buf);
            let line = raw.trim_end_matches(['\n', '\r']);
            if let Some(timing) = line.strip_prefix(CURL_MARKER) {
                curl = parse_curl_timing(timing);
                continue;
            }
            // SSE lines start with a field name or `:`, so a `{` before any `data:` starts a
            // JSON body rather than a stream.
            if json.is_none() && !sse && line.trim_start().starts_with('{') {
                json = self.spawn_jq()?;
            }
            if let Some(json) = &mut json {
                self.renderer.received(now);
                json.feed(&buf);
                continue;
            }
            sse |= line.starts_with("data:");
            for rendered in self.renderer.line(line, now) {
                self.emit(now, &rendered)?;
            }
        }
    }

    /// Starts jq on a JSON body: pretty in a terminal, one line otherwise. `None` without jq.
    fn spawn_jq(&mut self) -> io::Result<Option<JsonBody>> {
        if !self.jq {
            return Ok(None);
        }
        self.out.flush()?; // jq writes to the same stdout, after everything printed so far
        let spawned = Command::new("jq")
            .arg(if self.opts.color { "-C" } else { "-M" })
            .args((!self.opts.pretty).then_some("-c"))
            .args(["--unbuffered", "."])
            .stdin(Stdio::piped())
            .spawn();
        let Ok(mut jq) = spawned else {
            self.jq = false;
            return Ok(None);
        };
        Ok(Some(JsonBody {
            stdin: jq.stdin.take(),
            jq,
            raw: Vec::new(),
        }))
    }

    /// Waits for jq to print the body, and passes the body through as-is if it couldn't.
    fn end_json(&mut self, mut json: JsonBody, now: f64) -> io::Result<()> {
        drop(json.stdin.take()); // EOF for jq
        if json.jq.wait()?.success() {
            return Ok(());
        }
        for line in String::from_utf8_lossy(&json.raw).lines() {
            for rendered in self.renderer.line(line, now) {
                self.emit(now, &rendered)?;
            }
        }
        Ok(())
    }

    fn summary(&mut self, curl: Option<&CurlTiming>) -> io::Result<()> {
        match self.renderer.summary(curl) {
            Some(line) => self.emit(self.start.elapsed().as_secs_f64(), &line),
            None => Ok(()),
        }
    }

    fn emit(&mut self, now: f64, line: &str) -> io::Result<()> {
        if self.opts.time {
            let stamp = format!("{now:7.3}s ");
            write!(
                self.out,
                "{}",
                render::paint(self.opts.color, render::DIM, &stamp)
            )?;
        }
        writeln!(self.out, "{line}")
    }
}

/// Parses the `connect appconnect starttransfer` seconds written by our `-w`.
fn parse_curl_timing(s: &str) -> Option<CurlTiming> {
    let mut secs = s.split_whitespace().map(|v| v.parse().ok());
    Some(CurlTiming {
        connect: secs.next()??,
        tls: secs.next()??,
        ttfb: secs.next()??,
    })
}
