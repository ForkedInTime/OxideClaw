#!/usr/bin/env python3
"""bench.py — reproducible startup/footprint numbers for terminal coding agents.

No dependencies. For each tool it measures the on-disk size of the
executable, the wall-clock time of `<tool> --version` (cold start of the
runtime, N runs, median and p95), and the peak resident set of that process
(ru_maxrss of the child). Prints a Markdown table.

With --first-frame it instead starts each tool's interactive UI in a
pseudo-terminal (120x40, TERM=xterm-256color) in an empty temporary git repo
with a temporary HOME and XDG directories, so no user config or credentials
are read, and times spawn until the first frame is drawn: the last output
byte before the first 50 ms of quiet once at least 20 visible characters are
on screen, or the tool's ready marker (--marker TOOL=TEXT) when one is given.
Escape sequences are not visible characters. The harness answers the queries
a terminal answers (cursor position, device attributes, colours), so a tool
is not timed waiting on a reply no real terminal would withhold. A tool that
shows a sign-in screen, or exits asking for a key, is reported as needing a
login instead of being timed.

    scripts/bench.py                      # oxideclaw + whatever rivals are on PATH
    scripts/bench.py --runs 30 oxideclaw claude codex gemini goose opencode
    scripts/bench.py --first-frame --runs 20 oxideclaw
    scripts/bench.py --self-test          # check --first-frame against dummy TUIs
"""
import argparse
import codecs
import contextlib
import fcntl
import http.server
import io
import json
import os
import platform
import pty
import re
import resource
import select
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

DEFAULT_TOOLS = ["oxideclaw", "claude", "codex", "gemini", "goose", "opencode", "jcode", "codewhale", "claurst", "ante"]


def real_size(path):
    """Follow wrappers: a tiny shell shim is not the runtime's size."""
    p = os.path.realpath(path)
    try:
        with open(p, "rb") as f:
            head = f.read(2)
    except OSError:
        return os.path.getsize(p)
    if head == b"#!":
        return None  # script wrapper; size is not meaningful
    return os.path.getsize(p)


def measure(cmd, runs):
    times, rss = [], []
    for _ in range(runs):
        before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        t0 = time.perf_counter()
        r = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        t1 = time.perf_counter()
        after = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        if r.returncode not in (0, 1, 2):
            return None
        times.append((t1 - t0) * 1000.0)
        # ru_maxrss for CHILDREN is the max over all children so far; the
        # first run for each tool is the honest per-tool value, later runs
        # can only report a value >= it.
        rss.append(after if after >= before else before)
    return {
        "median_ms": statistics.median(times),
        "p95_ms": p95(times),
        "rss_mb": min(rss) / 1024.0,
    }


def p95(times):
    """Nearest-rank p95; with fewer than 20 runs, the slowest run."""
    times = sorted(times)
    return times[int(len(times) * 0.95) - 1] if len(times) >= 20 else times[-1]


def version_of(tool):
    try:
        out = subprocess.run([tool, "--version"], capture_output=True, text=True, timeout=30)
        line = (out.stdout or out.stderr).strip().splitlines()
        return line[0][:40] if line else "?"
    except Exception:
        return "?"


# ── Time to first frame ──────────────────────────────────────────────────────

COLS, ROWS = 120, 40
# A drawn frame that asks the user to sign in is a login screen, not the UI.
LOGIN_SCREEN = re.compile(r"\b(log ?in|sign ?in)\b", re.I)
# Exiting before any frame with one of these in the output: it needs a key.
NO_CREDENTIAL = re.compile(
    r"\b(log ?in|sign ?in|authenticat\w*|unauthori[sz]ed)\b|api[ _-]?key|credential", re.I
)


class Terminal:
    """Just enough of a terminal to time a first frame: counts the visible
    characters a tool prints (escape sequences are not visible) and answers
    the queries a real terminal answers, so a TUI that asks for the cursor
    position or device attributes is not timed waiting out a reply."""

    MAX_TEXT = 20000

    def __init__(self, fd):
        self.fd = fd
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self.state = "text"
        self.seq = ""
        self.visible = 0
        self.text = []

    def feed(self, data):
        for ch in self.decoder.decode(data):
            self._char(ch)

    def excerpt(self, n=60):
        return " ".join("".join(self.text).split())[:n]

    def _char(self, ch):
        st = self.state
        if st == "text":
            if ch == "\x1b":
                self.state = "esc"
            elif ch.isprintable() and not ch.isspace():
                self.visible += 1
                if len(self.text) < self.MAX_TEXT:
                    self.text.append(ch)
            elif self.text and self.text[-1] != " ":
                # Spaces, newlines and cursor moves separate words.
                self.text.append(" ")
        elif st == "esc":
            self.seq = ""
            self.state = {
                "[": "csi",
                "]": "osc",
                "P": "str",
                "_": "str",
                "^": "str",
                "X": "str",
            }.get(ch, "esc-arg" if ch in "()*+-./#%" else "text")
        elif st == "esc-arg":  # charset designation: ESC ( B
            self.state = "text"
        elif st == "csi":
            if "@" <= ch <= "~":
                self._csi(self.seq, ch)
                self.state = "text"
            else:
                self.seq += ch
        elif st in ("osc", "str"):
            if ch == "\x07":
                self._end_string(st)
            elif ch == "\x1b":
                self.state = st + "-esc"
            else:
                self.seq += ch
        elif st in ("osc-esc", "str-esc"):
            # ESC \ (string terminator); anything else starts a new escape.
            self._end_string(st[:3])
            if ch != "\\":
                self.state = "esc"
                self._char(ch)

    def _end_string(self, kind):
        if kind == "osc":
            self._osc(self.seq)
        self.state = "text"

    def _csi(self, params, final):
        if final != "m" and self.text and self.text[-1] != " ":
            # A cursor move or erase separates words as a space would; only
            # colours and attributes (SGR) sit inside a word.
            self.text.append(" ")
        if final == "n" and params == "6":  # cursor position
            self._reply("\x1b[1;1R")
        elif final == "n" and params == "5":  # device status
            self._reply("\x1b[0n")
        elif final == "c" and params in ("", "0"):  # primary device attributes
            self._reply("\x1b[?62;22c")
        elif final == "c" and params.startswith(">"):  # secondary attributes
            self._reply("\x1b[>1;10;0c")
        # CSI ? u (kitty keyboard flags) goes unanswered, as in xterm: the
        # primary-attributes reply that follows it says "not supported".

    def _osc(self, body):
        if body in ("10;?", "11;?"):  # foreground / background colour
            rgb = "ffff/ffff/ffff" if body.startswith("10") else "0000/0000/0000"
            self._reply(f"\x1b]{body[:2]};rgb:{rgb}\x1b\\")

    def _reply(self, s):
        try:
            os.write(self.fd, s.encode())
        except OSError:
            pass


def clean_env(root):
    """Only what a shell would need: no API keys, no user config."""
    home = os.path.join(root, "home")
    env = {
        "PATH": os.environ.get("PATH", os.defpath),
        "HOME": home,
        "USER": os.environ.get("USER", "bench"),
        "LANG": "C.UTF-8",
        "TERM": "xterm-256color",
        "XDG_CONFIG_HOME": os.path.join(home, ".config"),
        "XDG_DATA_HOME": os.path.join(home, ".local", "share"),
        "XDG_STATE_HOME": os.path.join(home, ".local", "state"),
        "XDG_CACHE_HOME": os.path.join(home, ".cache"),
    }
    for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"):
        os.makedirs(env[key])
    return env


def _take_tty():
    # Runs in the child after setsid: make the pty its controlling terminal,
    # as a terminal emulator does. Python code between fork and exec is only
    # safe while this process has no other thread (one could hold a lock the
    # child then waits on forever), so nothing here starts threads: the
    # Ollama stub runs in a process of its own.
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class OllamaStub(http.server.BaseHTTPRequestHandler):
    """Ollama's model listing and nothing else: one tool-capable model. With
    no key, OxideClaw starts on a local Ollama model when one answers; this
    lets a machine without Ollama time that path. No model ever runs."""

    def do_GET(self):
        self._json({"models": [{"name": "bench-stub:latest"}]} if self.path == "/api/tags" else None)

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        self._json({"capabilities": ["completion", "tools"]} if self.path == "/api/show" else None)

    def _json(self, body):
        data = json.dumps(body).encode() if body is not None else b""
        self.send_response(200 if body is not None else 404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


def serve_ollama_stub():
    """Child side of ollama_stub(): serve on a free local port, print it, and
    exit when the parent closes stdin (or dies)."""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), OllamaStub)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(server.server_address[1], flush=True)
    sys.stdin.read()


@contextlib.contextmanager
def ollama_stub():
    """Run OllamaStub in a separate process; yields its OLLAMA_HOST."""
    proc = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "--serve-ollama-stub"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        port = proc.stdout.readline().strip()
        if not port.isdigit():
            raise RuntimeError("the Ollama stub did not start")
        yield f"http://127.0.0.1:{port}"
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        proc.stdout.close()


def first_frame_once(argv, min_chars=20, quiet_ms=50, marker=None, timeout_s=10.0, ollama_host=None):
    """One launch. Returns (status, ms, excerpt): status is "frame" (ms is
    spawn to first frame), "needs login", "timeout" or "exited N"."""
    with tempfile.TemporaryDirectory(prefix="bench-ff-") as tmp:
        # The physical path, as the tool's getcwd() reports it: a temp dir
        # behind a symlink (macOS's /var -> /private/var) would otherwise give
        # HOME and the working directory different spellings.
        root = os.path.realpath(tmp)
        env = clean_env(root)
        if ollama_host:
            env["OLLAMA_HOST"] = ollama_host
        repo = os.path.join(root, "repo")
        subprocess.run(["git", "init", "-q", repo], env=env, check=True)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        term = Terminal(master)
        quiet = quiet_ms / 1000.0
        t0 = time.perf_counter()
        try:
            proc = subprocess.Popen(
                argv,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                cwd=repo,
                env=env,
                start_new_session=True,
                preexec_fn=_take_tty,
            )
        except OSError:
            os.close(master)
            raise
        finally:
            os.close(slave)
        try:
            last = None
            while True:
                now = time.perf_counter()
                framed = not marker and last is not None and term.visible >= min_chars
                if framed and now - last >= quiet:
                    status = "needs login" if LOGIN_SCREEN.search(term.excerpt(10000)) else "frame"
                    return status, (last - t0) * 1000.0, term.excerpt()
                if now - t0 >= timeout_s:
                    return "timeout", None, term.excerpt()
                wait = t0 + timeout_s - now
                if framed:
                    wait = min(wait, last + quiet - now)
                ready, _, _ = select.select([master], [], [], max(wait, 0))
                if not ready:
                    continue
                try:
                    data = os.read(master, 65536)
                except OSError:  # EIO: every slave fd is closed
                    data = b""
                if not data:
                    # The tool closed the pty; it may still be running (it
                    # detached, or redirected its output), so the wait is
                    # bounded by the same timeout.
                    try:
                        code = proc.wait(timeout=max(t0 + timeout_s - time.perf_counter(), 0.1))
                    except subprocess.TimeoutExpired:
                        return "timeout", None, term.excerpt()
                    if NO_CREDENTIAL.search(term.excerpt(10000)):
                        return "needs login", None, term.excerpt()
                    return f"exited {code}", None, term.excerpt()
                last = time.perf_counter()
                term.feed(data)
                if marker and marker in "".join(term.text):
                    return "frame", (last - t0) * 1000.0, term.excerpt()
        finally:
            stop(proc, master)
            os.close(master)


def stop(proc, master):
    """SIGTERM the tool's process group, then SIGKILL after a second. Output
    is drained meanwhile so a tool blocked on a full pty can still exit."""
    for sig, grace in ((signal.SIGTERM, 1.0), (signal.SIGKILL, 5.0)):
        if proc.poll() is not None:
            break
        try:
            os.killpg(proc.pid, sig)
        except ProcessLookupError:
            break
        deadline = time.monotonic() + grace
        while proc.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.02)
            if ready:
                try:
                    os.read(master, 65536)
                except OSError:
                    pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def first_frame(argv, runs, **opts):
    """Median and p95 over `runs` launches, or the first non-frame outcome
    (a timeout, a login screen, an early exit) which repeating won't change."""
    times, excerpt = [], ""
    for _ in range(runs):
        status, ms, text = first_frame_once(argv, **opts)
        if status != "frame":
            return {"status": status, "excerpt": text}
        times.append(ms)
        excerpt = excerpt or text
    return {
        "status": "frame",
        "median_ms": statistics.median(times),
        "p95_ms": p95(times),
        "excerpt": excerpt,
    }


def first_frame_report(args):
    markers = dict(m.split("=", 1) for m in args.marker if "=" in m)
    if len(markers) != len(args.marker):
        sys.exit("--marker takes TOOL=TEXT")
    opts = {"min_chars": args.min_chars, "quiet_ms": args.quiet_ms, "timeout_s": args.timeout}
    rows = []
    with ollama_stub() if args.ollama_stub else contextlib.nullcontext() as host:
        for tool in args.tools:
            path = shutil.which(tool)
            if not path:
                continue
            m = first_frame([path], args.runs, marker=markers.get(tool), ollama_host=host, **opts)
            rows.append((tool, version_of(tool), markers.get(tool), m))
    # The path column, not the header, says which run a row came from, so
    # rows from runs with and without --ollama-stub fit one table.
    how = "Ollama stub (`--ollama-stub`)" if args.ollama_stub else "default"
    print(f"Machine: {platform.node()} · {platform.machine()} · {platform.system()} {platform.release()}")
    print(
        f"Date: {time.strftime('%Y-%m-%d')} · runs per tool: {args.runs} · "
        f"terminal: {COLS}x{ROWS} xterm-256color · first frame: "
        f"{args.min_chars} visible characters then {args.quiet_ms:g} ms quiet, "
        f"or the ready marker\n"
    )
    print("| Tool | Version | Path | First frame (median) | p95 | Frame begins |")
    print("|---|---|---|---|---|---|")
    for tool, ver, marker, m in rows:
        shown = m["excerpt"].replace("|", "\\|").replace("`", "'")
        shown = f"`{shown}`" if shown else ""
        if marker:
            shown += f" (marker `{marker}`)"
        if m["status"] == "frame":
            print(f"| {tool} | {ver} | {how} | {m['median_ms']:.0f} ms | {m['p95_ms']:.0f} ms | {shown} |")
        elif m["status"] == "timeout":
            print(f"| {tool} | {ver} | {how} | timeout (> {args.timeout:g} s) | | {shown} |")
        else:
            print(f"| {tool} | {ver} | {how} | {m['status']} | | {shown} |")


# ── Self-test: --first-frame against dummy TUIs ──────────────────────────────

# Behaves like a TUI's startup: raw mode, escape sequences only (no visible
# text), a cursor-position query it waits on as ratatui's inline viewport
# does, then the banner 100 ms later. The banner reports what it saw of the
# terminal and environment, so the self-test checks the isolation too.
DUMMY_BANNER = r"""
import os, select, sys, time, tty
fd = sys.stdin.fileno()
tty.setraw(fd)
os.write(1, b"\x1b[2J\x1b[H\x1b[?25l\x1b(B\x1b[38;5;208m\x1b]11;?\x07\x1b[6n")
reply, deadline = b"", time.monotonic() + 2
while b"R" not in reply and time.monotonic() < deadline:
    if select.select([fd], [], [], 0.05)[0]:
        reply += os.read(fd, 64)
time.sleep(0.1)
cols, rows = os.get_terminal_size(fd)
leak = "ANTHROPIC_API_KEY" in os.environ
home = os.environ["HOME"].startswith(os.path.dirname(os.getcwd()))
banner = "READY %dx%d dsr=%s key=%s git=%s home=%s" % (
    cols, rows, "ok" if b"\x1b[1;1R" in reply else "missing",
    "LEAKED" if leak else "absent", "yes" if os.path.isdir(".git") else "no",
    "temp" if home else os.environ["HOME"])
os.write(1, banner.encode() + b"\x1b[0m\r\n")
time.sleep(60)
"""

# Visible text trickles in for 280 ms or more: 20 characters arrive well
# before the frame is finished, so only the quiet window can time it right.
DUMMY_TRICKLE = r"""
import os, time
for c in "loading the dummy agent ui...":
    os.write(1, c.encode())
    time.sleep(0.01)
time.sleep(60)
"""

# Plenty of visible text, then quiet, then the marker 100 ms later: with a
# marker given, only the marker counts.
DUMMY_MARKER = r"""
import os, time
os.write(1, b"booting the dummy agent, please wait\r\n")
time.sleep(0.1)
os.write(1, b"PROMPT> ")
time.sleep(60)
"""

DUMMY_SILENT = "import time; time.sleep(60)\n"
DUMMY_NO_KEY = "import sys; print('Error: ANTHROPIC_API_KEY is not set'); sys.exit(1)\n"
# What OxideClaw's keyless start asks a local Ollama, from OLLAMA_HOST.
DUMMY_OLLAMA = r"""
import json, os, time, urllib.request
host = os.environ.get("OLLAMA_HOST", "http://127.0.0.1:11434")
tags = json.load(urllib.request.urlopen(host + "/api/tags", timeout=5))
show = urllib.request.Request(host + "/api/show", data=b'{"model": "x"}', method="POST")
caps = json.load(urllib.request.urlopen(show, timeout=5))["capabilities"]
os.write(1, ("model %s tools=%s" % (tags["models"][0]["name"], "tools" in caps)).encode())
time.sleep(60)
"""

DUMMY_LOGIN = (
    "import time; print('Welcome! Please sign in with your browser to continue.'); "
    "time.sleep(60)\n"
)

# Closes the terminal and keeps running, as a tool that daemonises does: the
# pty reads EOF while the process lives on, and the timeout still holds.
DUMMY_DETACH = "import os, time; os.close(0); os.close(1); os.close(2); time.sleep(60)\n"

# A tool on PATH for the report: answers --version, draws one line that says
# whether OLLAMA_HOST reached it.
DUMMY_AGENT = r"""
import os, sys, time
if "--version" in sys.argv:
    print("dummy-agent 1.0")
    sys.exit(0)
os.write(1, ("dummy agent ready, OLLAMA_HOST=%s" % ("set" if os.environ.get("OLLAMA_HOST") else "unset")).encode())
time.sleep(60)
"""


def self_test():
    # A credential in the caller's environment must not reach the tool.
    os.environ["ANTHROPIC_API_KEY"] = "sk-bench-self-test-not-a-key"
    failures = []

    def check(name, ok, detail):
        print(f"{'ok  ' if ok else 'FAIL'} {name}: {detail}")
        if not ok:
            failures.append(name)

    with tempfile.TemporaryDirectory(prefix="bench-selftest-") as d:

        def script(name, body):
            path = os.path.join(d, name)
            with open(path, "w") as f:
                f.write(body)
            return [sys.executable, path]

        # The temp dir behind a symlink, as on macOS (/var -> /private/var):
        # the tool's HOME and working directory must still agree.
        os.mkdir(os.path.join(d, "tmp"))
        os.symlink(os.path.join(d, "tmp"), os.path.join(d, "tmp-link"))
        saved_tempdir, tempfile.tempdir = tempfile.tempdir, os.path.join(d, "tmp-link")
        try:
            m = first_frame(script("banner.py", DUMMY_BANNER), 3)
        finally:
            tempfile.tempdir = saved_tempdir
        want = f"READY {COLS}x{ROWS} dsr=ok key=absent git=yes home=temp"
        check("banner frame", m["status"] == "frame", m["status"])
        if m["status"] == "frame":
            check("banner text", m["excerpt"] == want, repr(m["excerpt"]))
            check("banner time", 100 <= m["median_ms"] <= 1600, f"{m['median_ms']:.0f} ms")

        m = first_frame(script("trickle.py", DUMMY_TRICKLE), 1)
        ok = m["status"] == "frame" and 280 <= m["median_ms"] <= 1800
        check("quiet window", ok, f"{m['status']} {m.get('median_ms', 0):.0f} ms")

        m = first_frame(script("marker.py", DUMMY_MARKER), 1, marker="PROMPT>")
        ok = m["status"] == "frame" and 100 <= m["median_ms"] <= 1600
        check("ready marker", ok, f"{m['status']} {m.get('median_ms', 0):.0f} ms")

        t = time.perf_counter()
        m = first_frame(script("silent.py", DUMMY_SILENT), 3, timeout_s=0.5)
        elapsed = time.perf_counter() - t
        check("timeout", m["status"] == "timeout", m["status"])
        check("timeout killed it", elapsed < 5, f"{elapsed:.1f} s for one 0.5 s timeout")

        m = first_frame(script("nokey.py", DUMMY_NO_KEY), 1)
        check("exits without a key", m["status"] == "needs login", m["status"])

        m = first_frame(script("login.py", DUMMY_LOGIN), 1)
        check("sign-in screen", m["status"] == "needs login", m["status"])

        t = time.perf_counter()
        m = first_frame(script("detach.py", DUMMY_DETACH), 1, timeout_s=0.5)
        elapsed = time.perf_counter() - t
        check("closes the pty, keeps running", m["status"] == "timeout", m["status"])
        check("detached tool killed", elapsed < 5, f"{elapsed:.1f} s for one 0.5 s timeout")

        with ollama_stub() as host:
            # The harness forks the tool and runs Python in the child before
            # exec, so the stub must not add a thread to this process.
            threads = threading.active_count()
            check("ollama stub out of process", threads == 1, f"{threads} threads")
            m = first_frame(script("ollama.py", DUMMY_OLLAMA), 1, ollama_host=host)
        ok = m["excerpt"] == "model bench-stub:latest tools=True"
        check("ollama stub", ok, f"{m['status']} {m['excerpt']!r}")

        # The report's table, as BENCHMARKS.md shows it.
        bindir = os.path.join(d, "bin")
        os.mkdir(bindir)
        agent = os.path.join(bindir, "dummy-agent")
        with open(agent, "w") as f:
            f.write(f"#!{sys.executable}\n{DUMMY_AGENT}")
        os.chmod(agent, 0o755)
        args = argparse.Namespace(
            tools=["dummy-agent"], runs=1, marker=[], min_chars=20, quiet_ms=50,
            timeout=10, ollama_stub=True,
        )
        saved_path = os.environ["PATH"]
        os.environ["PATH"] = bindir + os.pathsep + saved_path
        out = io.StringIO()
        try:
            with contextlib.redirect_stdout(out):
                first_frame_report(args)
        finally:
            os.environ["PATH"] = saved_path
        lines = out.getvalue().splitlines()
        head = "| Tool | Version | Path | First frame (median) | p95 | Frame begins |"
        row = "| dummy-agent | dummy-agent 1.0 | Ollama stub (`--ollama-stub`) | "
        ok = (
            head in lines
            and any(l.startswith(row) and "`dummy agent ready, OLLAMA_HOST=set`" in l for l in lines)
            and any(l.startswith("Date: ") and "runs per tool: 1 ·" in l for l in lines)
        )
        check("report table", ok, repr(lines[-1] if lines else ""))

    if failures:
        print(f"self-test failed: {', '.join(failures)}")
        sys.exit(1)
    print("self-test passed")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tools", nargs="*", default=DEFAULT_TOOLS)
    ap.add_argument("--runs", type=int, default=20)
    ap.add_argument("--first-frame", action="store_true", help="time to first frame in a pty")
    ap.add_argument("--min-chars", type=int, default=20, help="visible characters in a frame")
    ap.add_argument("--quiet-ms", type=float, default=50, help="output quiet that ends a frame")
    ap.add_argument("--timeout", type=float, default=10, help="seconds before a launch times out")
    ap.add_argument(
        "--marker", action="append", default=[], metavar="TOOL=TEXT",
        help="text that means TOOL's UI is up; times its first appearance",
    )
    ap.add_argument(
        "--ollama-stub", action="store_true",
        help="answer Ollama's model listing locally (OxideClaw's keyless path)",
    )
    ap.add_argument("--self-test", action="store_true", help="check --first-frame on dummy TUIs")
    ap.add_argument("--serve-ollama-stub", action="store_true", help=argparse.SUPPRESS)
    args = ap.parse_args()
    if args.serve_ollama_stub:
        return serve_ollama_stub()
    if args.self_test:
        return self_test()
    if args.first_frame:
        return first_frame_report(args)
    rows = []
    # Measure each tool in its own subprocess so ru_maxrss(CHILDREN) is per-tool.
    for tool in args.tools:
        path = shutil.which(tool)
        if not path:
            continue
        if os.environ.get("BENCH_CHILD") == tool:
            m = measure([path, "--version"], args.runs)
            print(f"{m['median_ms']:.1f} {m['p95_ms']:.1f} {m['rss_mb']:.1f}" if m else "x")
            return
        sub = subprocess.run(
            [sys.executable, __file__, "--runs", str(args.runs), tool],
            env={**os.environ, "BENCH_CHILD": tool},
            capture_output=True,
            text=True,
        )
        out = sub.stdout.strip()
        if out == "x" or not out:
            rows.append((tool, version_of(tool), real_size(path), None))
            continue
        med, p95, rss = (float(x) for x in out.split())
        rows.append((tool, version_of(tool), real_size(path), (med, p95, rss)))
    print(f"Machine: {platform.node()} · {platform.machine()} · {platform.system()} {platform.release()}")
    print(f"Date: {time.strftime('%Y-%m-%d')} · runs per tool: {args.runs} · command: `<tool> --version`\n")
    print("| Tool | Version | Executable | Cold start (median) | p95 | Peak RSS |")
    print("|---|---|---|---|---|---|")
    for tool, ver, size, m in rows:
        size_s = f"{size / 1_048_576:.1f} MB" if size else "script wrapper"
        if m is None:
            print(f"| {tool} | {ver} | {size_s} | failed | | |")
            continue
        med, p95, rss = m
        print(f"| {tool} | {ver} | {size_s} | {med:.0f} ms | {p95:.0f} ms | {rss:.0f} MB |")


if __name__ == "__main__":
    main()
