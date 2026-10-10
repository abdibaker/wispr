#!/usr/bin/env python3
"""Browser insertion target: `webtarget.py field|chat OUT`. Serves a page to a throwaway Chrome
profile; the page reports its text to OUT. `chat` mimics a chat composer: Enter sends (and
clears), Shift+Enter is a newline; OUT gets the sent messages, then the draft after `---`."""
import http.server, os, signal, subprocess, sys, tempfile, threading

kind, out = sys.argv[1], sys.argv[2]
PAGE = """<!doctype html><meta charset=utf-8><title>vp-target-%s</title>
<body style="margin:0">%s<script>
const box = document.getElementById('box'); let sent = [];
const text = () => box.value !== undefined ? box.value : box.innerText;
const report = () => fetch('/text', {method: 'POST', body: sent.join('\\n<send>\\n') + '\\n---\\n' + text()});
box.addEventListener('input', report);
box.addEventListener('keydown', e => {
  if (%s && e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sent.push(text()); box.innerText = ''; report(); }
});
box.focus(); setInterval(report, 300);
</script>"""
EDITOR = {"field": ('<textarea id=box style="width:98vw;height:95vh"></textarea>', "false"),
          "chat": ('<div id=box contenteditable style="width:98vw;height:95vh;white-space:pre-wrap"></div>', "true")}[kind]


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = (PAGE % (kind, *EDITOR)).encode()
        self.send_response(200), self.send_header("Content-Type", "text/html"), self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        data = self.rfile.read(int(self.headers["Content-Length"]))
        with open(out, "wb") as f:
            f.write(data)
        self.send_response(204), self.end_headers()

    def log_message(self, *_):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
profile = tempfile.mkdtemp(prefix="vp-chrome-")
chrome = subprocess.Popen(["google-chrome", f"--user-data-dir={profile}", "--no-first-run", "--no-default-browser-check",
                           "--ozone-platform=wayland", f"--app=http://127.0.0.1:{server.server_port}/"],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
signal.signal(signal.SIGTERM, lambda *_: (os.killpg(chrome.pid, signal.SIGTERM), sys.exit(0)))
chrome.wait()
