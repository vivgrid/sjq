mod render;

use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, StdoutLock, Write};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use render::{CurlTiming, Renderer};

/// Starts the line that `sjq curl ...` asks curl to print (via `-w`) after the response body.
const CURL_MARKER: &str = "@@sjq-curl-timing@@";

const HELP: &str = "\
sjq: read OpenAI-compatible /v1/chat/completions streams at a glance

Usage: curl -N ... | sjq [OPTIONS]
       sjq [OPTIONS] curl [CURL ARGS]...
       sjq [OPTIONS] [FILE]...

Each `data:` chunk becomes one line with only what it carries:
  data: role=assistant, '¡'                     first delta
  data: reasoning_content=' user'               reasoning delta
  data: ' in'                                   content delta
  data: tool_calls[0] id=call_1 name=get_time   tool call delta
  data: finish_reason=stop, usage: {...}        final chunk (zero usage details dropped)
id / model / created are printed once per stream on a `meta:` line, and again when
they change. Unrecognized non-null fields are always shown. Other lines (event:,
comments, curl -w output) pass through; blank lines are dropped.

When the stream ends, a `time:` line reports:
  connect, tls, ttfb   from curl, only in `sjq curl ...` mode
  ttft                 first generated token (reasoning, content or tool call)
  first_content        first answer token, if reasoning or a tool call came first
  total                last line received
  speed                completion_tokens / (total - ttft)
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
    }))
}

fn run(opts: &Opts) -> io::Result<ExitCode> {
    let mut session = Session {
        opts,
        renderer: Renderer::new(opts.color),
        out: io::stdout().lock(),
        start: Instant::now(),
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
        loop {
            buf.clear();
            if input.read_until(b'\n', &mut buf)? == 0 {
                return Ok(curl);
            }
            let now = self.start.elapsed().as_secs_f64();
            let raw = String::from_utf8_lossy(&buf);
            let line = raw.trim_end_matches(['\n', '\r']);
            if let Some(timing) = line.strip_prefix(CURL_MARKER) {
                curl = parse_curl_timing(timing);
                continue;
            }
            for rendered in self.renderer.line(line, now) {
                self.emit(now, &rendered)?;
            }
        }
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
