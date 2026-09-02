#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
极简网页剪贴板 —— 私人自用版，零依赖单文件（只用 Python 3 标准库）

没有 ID、没有槽位，就一块内容，地址永远是域名本身：

    curl -d "要传的内容" https://HOST/     存进去（覆盖）
    curl -sL https://HOST/                取回来（只有这一条命令）

浏览器打开同一个域名，看到的就是这块内容，改完点保存即可。
curl 和浏览器访问同一地址，靠 Accept 头自动区分返回纯文本还是网页。

启动：
    python3 main.py
    SLIP_PORT=80 SLIP_TTL=86400 python3 main.py

环境变量：
    SLIP_HOST       监听地址，默认 0.0.0.0
    SLIP_PORT       监听端口，默认 8080
    SLIP_FILE       根路径内容文件，默认脚本同级的 slip.txt
    SLIP_STORE      /a、/b 等路径文件目录，默认 slip.txt.files
    SLIP_TTL        有效期秒数，默认 7 天；0 = 不过期
    SLIP_MAX_BYTES  大小上限，默认 2MB
    SLIP_TOKEN      可选访问令牌，留空则公开（见下方说明）

接口：
    GET  /        浏览器→网页；curl→纯文本原文
    GET  /raw     强制纯文本（浏览器想看原文时用）
    POST /        保存（请求体即内容，覆盖旧内容）
    PUT  /        同上，方便 curl -T 传文件
    DELETE /      清空
    GET  /healthz 健康检查
"""

import html
import json
import mimetypes
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

VER = "Slip/1.0"

HOST = os.environ.get("SLIP_HOST", "0.0.0.0")
PORT = int(os.environ.get("SLIP_PORT", "8080"))
FILE = os.environ.get(
    "SLIP_FILE", os.path.join(os.path.dirname(os.path.abspath(__file__)), "slip.txt")
)
STORE = os.environ.get("SLIP_STORE", FILE + ".files")
TTL = int(os.environ.get("SLIP_TTL", str(7 * 24 * 3600)))
MAX = int(os.environ.get("SLIP_MAX_BYTES", str(2 * 1024 * 1024)))
TOKEN = os.environ.get(
    "SLIP_TOKEN", ""
)  # 留空=公开；设置后需 X-Slip-Token 头或 ?token=


# ---------------- 存储：就一个纯文本文件，可直接 cat / 手工编辑 ----------------


def target(name):
    if not name:
        return FILE
    os.makedirs(STORE, exist_ok=True)
    return os.path.join(STORE, name)


def metadata_path(path):
    os.makedirs(os.path.join(STORE, ".meta"), exist_ok=True)
    return os.path.join(STORE, ".meta", os.path.basename(path) + ".json")


def save(name, body, content_type):
    path = target(name)
    os.makedirs(os.path.dirname(os.path.abspath(path)) or ".", exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "wb") as f:
        f.write(body)
    os.replace(tmp, path)
    meta = metadata_path(path)
    meta_tmp = meta + ".tmp"
    with open(meta_tmp, "w", encoding="utf-8") as f:
        json.dump({"content_type": content_type}, f)
    os.replace(meta_tmp, meta)


def load(name):
    path = target(name)
    if not os.path.exists(path):
        return None
    if TTL > 0 and time.time() - os.path.getmtime(path) > TTL:
        try:
            os.remove(path)
            meta = metadata_path(path)
            if os.path.exists(meta):
                os.remove(meta)
        except OSError:
            pass
        return None
    content_type = mimetypes.guess_type(path)[0] or "application/octet-stream"
    try:
        with open(metadata_path(path), encoding="utf-8") as f:
            content_type = json.load(f).get("content_type") or content_type
    except (OSError, ValueError):
        pass
    with open(path, "rb") as f:
        return f.read(), content_type, os.path.getmtime(path)


def clear(name):
    path = target(name)
    try:
        os.remove(path)
        meta = metadata_path(path)
        if os.path.exists(meta):
            os.remove(meta)
        return True
    except OSError:
        return False


# ---------------- 页面：一个框，既是显示也是编辑 ----------------

CSS = """
:root{
  --bg:#0a0a0e;--surface:#121217;--surface-2:#1a1a21;
  --border:#23232c;--border-strong:#33333e;
  --text:#e9e9f0;--text-2:#9b9baa;--text-3:#62626f;
  --accent:#e6b54e;--accent-strong:#f2c86a;--accent-ink:#241a06;
  --ok:#8fd3b0;--danger:#e9828f;
  --radius:12px;
  --font-ui:-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Hiragino Sans GB","Microsoft YaHei","Noto Sans SC",sans-serif;
  --font-mono:ui-monospace,"SF Mono","Cascadia Code","JetBrains Mono",Menlo,Consolas,"PingFang SC",monospace;
  color-scheme:dark;
}
@media (prefers-color-scheme: light){
  :root{
    --bg:#f5f4f0;--surface:#fff;--surface-2:#efeee9;
    --border:#dddcd5;--border-strong:#c8c7bf;
    --text:#23211d;--text-2:#615e56;--text-3:#97948b;
    --accent:#a97617;--accent-strong:#8f6109;--accent-ink:#fff;
    --ok:#2e7d52;--danger:#a63b3b;
    color-scheme:light;
  }
}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--text);font:15px/1.7 var(--font-ui);-webkit-font-smoothing:antialiased}
body::before{
  content:"";position:fixed;inset:0;z-index:-1;pointer-events:none;
  background:radial-gradient(1100px 520px at 50% -180px,color-mix(in srgb,var(--accent) 10%,transparent),transparent 62%);
}
@supports not (background:color-mix(in srgb,red 50%,blue)){
  body::before{background:radial-gradient(1100px 520px at 50% -180px,rgba(230,181,78,.07),transparent 62%)}
}
::selection{background:color-mix(in srgb,var(--accent) 32%,transparent)}
main{max-width:760px;margin:0 auto;padding:clamp(24px,6vw,56px) 20px 48px;animation:rise .45s cubic-bezier(.16,1,.3,1) both}
@keyframes rise{from{opacity:0;transform:translateY(8px)}to{opacity:1;transform:none}}
.head{display:flex;justify-content:space-between;align-items:flex-start;gap:16px;flex-wrap:wrap}
h1{font-size:20px;font-weight:700;letter-spacing:-.01em;margin:0;display:flex;align-items:baseline;gap:10px}
h1 .tag{font:600 12px/1 var(--font-mono);color:var(--text-3);letter-spacing:.02em}
.hint{color:var(--text-2);font-size:13.5px;margin:8px 0 0}
.chip{
  display:inline-flex;align-items:center;gap:7px;white-space:nowrap;margin-top:2px;
  font:600 12px/1 var(--font-mono);
  color:color-mix(in srgb,var(--accent) 78%,var(--text));
  background:color-mix(in srgb,var(--accent) 10%,transparent);
  border:1px solid color-mix(in srgb,var(--accent) 26%,transparent);
  border-radius:999px;padding:7px 12px;
}
textarea{
  width:100%;height:min(56vh,520px);min-height:220px;margin-top:20px;
  background:var(--surface);color:var(--text);
  border:1px solid var(--border);border-radius:var(--radius);
  padding:18px 20px;font:13.5px/1.75 var(--font-mono);
  resize:vertical;outline:none;caret-color:var(--accent);
  box-shadow:inset 0 1px 2px rgba(0,0,0,.18);
  transition:border-color .18s,box-shadow .18s,background .18s;
  scrollbar-width:thin;scrollbar-color:var(--border-strong) transparent;
}
textarea::placeholder{color:var(--text-3)}
textarea:focus{border-color:var(--accent);box-shadow:inset 0 1px 2px rgba(0,0,0,.18),0 0 0 3px color-mix(in srgb,var(--accent) 16%,transparent)}
textarea.drop{border-color:var(--accent);background:color-mix(in srgb,var(--accent) 5%,var(--surface));box-shadow:0 0 0 3px color-mix(in srgb,var(--accent) 18%,transparent)}
textarea::-webkit-scrollbar{width:10px}
textarea::-webkit-scrollbar-thumb{background:var(--border-strong);border-radius:8px;border:2px solid var(--surface)}
textarea::-webkit-scrollbar-track{background:transparent}
.preview{display:block;max-width:100%;max-height:46vh;margin:20px auto 0;border:1px solid var(--border);border-radius:var(--radius);background:var(--surface)}
.row{display:flex;align-items:center;gap:10px;margin:14px 0 0;flex-wrap:wrap}
button{
  appearance:none;-webkit-appearance:none;
  border:1px solid var(--border);border-radius:10px;
  background:var(--surface-2);color:var(--text);
  font:600 13.5px/1 var(--font-ui);padding:10px 18px;cursor:pointer;
  transition:transform .12s cubic-bezier(.16,1,.3,1),background .18s,border-color .18s;
}
button:hover{background:var(--border-strong);border-color:var(--border-strong)}
button:active{transform:translateY(1px) scale(.98)}
button:focus-visible{outline:none;box-shadow:0 0 0 3px color-mix(in srgb,var(--accent) 32%,transparent)}
button.primary{background:var(--accent);border-color:var(--accent);color:var(--accent-ink);font-weight:700}
button.primary:hover{background:var(--accent-strong);border-color:var(--accent-strong)}
button.danger{background:color-mix(in srgb,var(--danger) 14%,transparent);border-color:color-mix(in srgb,var(--danger) 36%,transparent);color:var(--danger)}
button.danger:hover{background:color-mix(in srgb,var(--danger) 24%,transparent)}
button.pulse{animation:pulse .5s cubic-bezier(.16,1,.3,1)}
@keyframes pulse{0%{transform:scale(1)}35%{transform:scale(1.06)}100%{transform:scale(1)}}
#msg{color:var(--text-3);font-size:13px;margin-left:auto}
#msg.err{color:var(--danger)}
#msg.ok{color:var(--ok)}
.section-label{font:600 11.5px/1 var(--font-mono);letter-spacing:.14em;text-transform:uppercase;color:var(--text-3);margin:26px 0 10px}
.cmd{margin:0;border:1px solid var(--border);border-radius:var(--radius);background:var(--surface);overflow:hidden}
.cmd .line{display:grid;grid-template-columns:auto 1fr auto;gap:12px;align-items:baseline;padding:9px 14px;font:12.5px/1.7 var(--font-mono)}
.cmd .line + .line{border-top:1px solid var(--border)}
.cmd .p{color:var(--accent);user-select:none}
.cmd .t{color:var(--text-2);overflow-wrap:anywhere}
.cmd .t .u{color:color-mix(in srgb,var(--accent) 72%,var(--text));word-break:break-all}
.cmd .c{color:var(--text-3);white-space:nowrap}
.foot{display:flex;justify-content:space-between;gap:10px;flex-wrap:wrap;margin-top:22px;padding-top:14px;border-top:1px solid var(--border);color:var(--text-3);font:12px/1.7 var(--font-mono)}
a{color:var(--accent)}
a:focus-visible{outline:2px solid var(--accent);outline-offset:2px;border-radius:4px}
@media (prefers-reduced-motion:reduce){*{animation:none!important;transition:none!important}}
"""

PAGE = """<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>slip</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='8' fill='%23e6b54e'/%3E%3Cg fill='%231a1406'%3E%3Crect x='8' y='8' width='16' height='3.5' rx='1.75'/%3E%3Crect x='8' y='14.25' width='16' height='3.5' rx='1.75'/%3E%3Crect x='8' y='20.5' width='9' height='3.5' rx='1.75'/%3E%3C/g%3E%3C/svg%3E">
<style>__CSS__</style></head><body><main>
<header class="head">
  <div>
    <h1>剪贴板<span class="tag">slip</span></h1>
    <p class="hint">一个地址，一块内容。curl 与浏览器共用同一块存储。</p>
  </div>
  <span class="chip">__CHIP__</span>
</header>
__PREVIEW__
<textarea id="c" spellcheck="false" placeholder="空的。粘贴或拖入内容，点保存。">__CONTENT__</textarea>
<div class="row">
  <button id="cp" type="button">复制全文</button>
  <button id="save" type="button">保存</button>
  <button class="danger" id="clr" type="button">清空</button>
  <span id="msg" role="status">Ctrl / ⌘ + Enter 保存</span>
</div>
<footer>
  <p class="section-label">命令</p>
  <div class="cmd">
    <div class="line"><span class="p">$</span><span class="t">curl -sL <span class="u">__B__</span></span><span class="c"># 取回</span></div>
    <div class="line"><span class="p">$</span><span class="t">curl -d "内容" <span class="u">__B__</span></span><span class="c"># 保存</span></div>
    <div class="line"><span class="p">$</span><span class="t">curl -T ./file <span class="u">__B__</span></span><span class="c"># 上传文件</span></div>
    <div class="line"><span class="p">$</span><span class="t">curl -X DELETE <span class="u">__B__</span></span><span class="c"># 清空</span></div>
  </div>
  <div class="foot">
    <span>存储 __FILE_NAME__ · 上限 __MAXS__ · __VER__</span>
    <span>__META__</span>
  </div>
</footer>
</main>
<script>
const ta=document.getElementById('c'),msg=document.getElementById('msg'),T='__TOK__';
const saveBtn=document.getElementById('save');
function setMsg(t,c){msg.textContent=t;msg.className=c||'';}
const endpoint=__PATH_JSON__;
function q(){return T?'?token='+encodeURIComponent(T):'';}
function hdr(x){const h=Object.assign({},x||{});if(T)h['X-Slip-Token']=T;return h;}
document.getElementById('cp').onclick=e=>{navigator.clipboard.writeText(ta.value).then(()=>{const b=e.target;b.textContent='已复制';setTimeout(()=>b.textContent='复制全文',1200);});};
saveBtn.onclick=async()=>{
  if(!ta.value){setMsg('内容为空','err');return;}
  if(new Blob([ta.value]).size>__MAXN__){setMsg('内容太大了','err');return;}
  setMsg('保存中…');
  try{
    const r=await fetch(endpoint+q(),{method:'POST',headers:hdr({'Content-Type':'text/plain','X-Requested-With':'fetch'}),body:ta.value});
    const j=await r.json();
    if(!r.ok){setMsg(j.error||('失败 '+r.status),'err');return;}
    setMsg('已保存 '+j.bytes+' 字节','ok');
    saveBtn.classList.remove('pulse');void saveBtn.offsetWidth;saveBtn.classList.add('pulse');
  }catch(e){setMsg('保存失败：'+e,'err');}
};
document.getElementById('clr').onclick=async()=>{if(!confirm('清空内容？'))return;await fetch(endpoint+q(),{method:'DELETE',headers:hdr()});ta.value='';setMsg('已清空','ok');};
ta.addEventListener('keydown',e=>{if((e.metaKey||e.ctrlKey)&&e.key==='Enter'){e.preventDefault();saveBtn.click();}});
ta.addEventListener('dragover',e=>{e.preventDefault();ta.classList.add('drop');});
ta.addEventListener('dragleave',e=>{ta.classList.remove('drop');});
ta.addEventListener('drop',async e=>{e.preventDefault();ta.classList.remove('drop');const f=e.dataTransfer.files[0];if(f){ta.value=await f.text();setMsg('已读入 '+f.name+'，点保存上传','ok');}});
</script>
</body></html>"""


def human_ttl():
    if TTL <= 0:
        return "不过期"
    if TTL % 86400 == 0:
        return "%d 天" % (TTL // 86400)
    if TTL % 3600 == 0:
        return "%d 小时" % (TTL // 3600)
    return "%d 秒" % TTL


def human_delta(sec):
    if sec >= 86400:
        return "%d 天 %d 小时" % (sec // 86400, sec % 86400 // 3600)
    if sec >= 3600:
        return "%d 小时 %d 分" % (sec // 3600, sec % 3600 // 60)
    return "%d 分" % (sec // 60)


def fmt_size(n):
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return "%dB" % n if unit == "B" else "%.1f%s" % (n, unit)
        n /= 1024.0
    return "%.1fGB" % n


def page(base, path, record):
    body, content_type, mtime = record or (b"", "text/plain", 0)
    content = body.decode("utf-8", "replace")
    meta = ""
    if body:
        parts = ["%s 字符" % format(len(content), ",")]
        parts.append(
            "保存于 %s" % time.strftime("%m-%d %H:%M", time.localtime(mtime))
        )
        if TTL > 0:
            parts.append(
                "剩余 %s" % human_delta(max(int(mtime + TTL - time.time()), 0))
            )
        meta = " · ".join(parts)
    chip = "保留 %s" % human_ttl() if TTL > 0 else "永不过期"
    return (
        PAGE.replace("__CSS__", CSS)
        .replace("__CONTENT__", html.escape(content))
        .replace("__CHIP__", html.escape(chip))
        .replace("__META__", html.escape(meta))
        .replace("__B__", base)
        .replace("__PATH_JSON__", json.dumps(path, ensure_ascii=False))
        .replace("__PREVIEW__", '<img class="preview" src="%s">' % html.escape(path, quote=True) if content_type.startswith("image/") and body else "")
        .replace("__TOK__", TOKEN)
        .replace("__MAXN__", str(MAX))
        .replace("__FILE_NAME__", html.escape(os.path.basename(FILE)))
        .replace("__MAXS__", html.escape(fmt_size(MAX)))
        .replace("__VER__", VER)
    )


# ---------------- HTTP ----------------


class Handler(BaseHTTPRequestHandler):
    server_version = VER
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def handle_one_request(self):
        """兜底：任何未捕获异常都回 500，不让请求挂死。"""
        try:
            super().handle_one_request()
        except Exception:
            import traceback

            traceback.print_exc()
            self.close_connection = True
            try:
                self._send(500, "internal error\n")
            except Exception:
                pass

    def _send(self, code, body=b"", ctype="text/plain; charset=utf-8"):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _json(self, code, obj):
        self._send(
            code, json.dumps(obj, ensure_ascii=False), "application/json; charset=utf-8"
        )

    def _base(self):
        return "https://" + (self.headers.get("Host") or "HOST")

    def _authorized(self):
        if not TOKEN:
            return True
        if self.headers.get("X-Slip-Token") == TOKEN:
            return True
        return (parse_qs(urlparse(self.path).query).get("token") or [""])[0] == TOKEN

    def _wants_html(self):
        """浏览器要网页，curl 要纯文本（curl 默认发 Accept: */*）。"""
        return "text/html" in (self.headers.get("Accept") or "")

    def _name(self, allow_raw=False):
        path = unquote(urlparse(self.path).path)
        if path == "/":
            return ""
        if allow_raw and path == "/raw":
            return ""
        parts = [p for p in path.split("/") if p]
        if len(parts) != 1 or parts[0] in (".", "..", "raw", "healthz", "favicon.ico", "index.html"):
            return None
        return parts[0]

    # ---- 读 ----
    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        path = urlparse(self.path).path
        if path == "/healthz":
            return self._send(200, "ok\n")
        if path == "/favicon.ico":
            return self._send(204, b"")
        if not self._authorized():
            return self._send(401, "unauthorized\n")

        name = self._name(allow_raw=True)
        if name is None:
            return self._send(404, "not found\n")
        record = load(name)
        if path == "/raw" or not self._wants_html():
            body, content_type, _ = record or (b"", "text/plain", 0)
            return self._send(200, body, content_type)
        if record and record[1].startswith("image/"):
            return self._send(200, record[0], record[1])
        return self._send(200, page(self._base() + path, path, record), "text/html; charset=utf-8")

    # ---- 写 ----
    def do_POST(self):
        name = self._name()
        if name is None:
            return self._send(404, "not found\n")
        return self._write(name)

    def do_PUT(self):
        """curl -T 会把路径改写成 /文件名，这里对「根」或「任意单段名」都照单全收。"""
        name = self._name()
        if name is None:
            return self._send(404, "not found\n")
        return self._write(name)

    def _write(self, name):
        if not self._authorized():
            return self._send(401, "unauthorized\n")

        try:
            n = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            n = 0
        if n > MAX:  # 超限也把 body 读完，避免连接被重置
            rest = n
            while rest > 0:
                chunk = self.rfile.read(min(rest, 65536))
                if not chunk:
                    break
                rest -= len(chunk)
            if rest > 0:
                self.close_connection = True
            return self._json(413, {"error": "内容超过 %s 上限" % fmt_size(MAX)})

        body = self.rfile.read(n) if n else b""
        ctype = (self.headers.get("Content-Type") or "").split(";")[0].strip()
        if ctype == "application/json":
            try:
                content = json.loads(body.decode("utf-8")).get("content", "").encode("utf-8")
            except Exception:
                return self._json(400, {"error": "JSON 解析失败"})
        else:
            content = body

        if not content:
            return self._json(400, {"error": "内容为空"})
        if len(content) > MAX:
            return self._json(413, {"error": "内容超过 %s 上限" % fmt_size(MAX)})

        save(name, content, ctype or mimetypes.guess_type(name or FILE)[0] or "text/plain; charset=utf-8")
        if self._wants_html() or self.headers.get("X-Requested-With") == "fetch":
            return self._json(200, {"ok": True, "bytes": len(content)})
        self._send(
            200,
            "saved %s\n" % fmt_size(len(content)),
            "text/plain; charset=utf-8",
        )

    # ---- 清空 ----
    def do_DELETE(self):
        name = self._name()
        if name is None:
            return self._send(404, "not found\n")
        if not self._authorized():
            return self._send(401, "unauthorized\n")
        clear(name)
        self._send(200, "cleared\n")


def main():
    srv = ThreadingHTTPServer((HOST, PORT), Handler)
    sys.stderr.write(
        "剪贴板已启动: https://%s:%d/  文件 %s  有效期 %s%s\n"
        % (
            HOST if HOST != "0.0.0.0" else "127.0.0.1",
            PORT,
            FILE,
            human_ttl(),
            "  已启用令牌" if TOKEN else "",
        )
    )
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        srv.shutdown()


if __name__ == "__main__":
    main()
