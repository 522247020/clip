//! 极简网页剪贴板 —— 私人自用版，单文件 Rust 实现。
//!
//! 没有 ID、没有槽位，就一块内容，地址永远是域名本身：
//!
//!     curl -d "要传的内容" https://HOST/     存进去（覆盖）
//!     curl -sL https://HOST/                取回来（只有这一条命令）
//!
//! 浏览器打开同一个域名，看到的就是这块内容，改完点保存即可。
//! curl 和浏览器访问同一地址，靠 Accept 头自动区分返回纯文本还是网页。
//! 图片可直接预览，二进制文件在网页上提供下载（上限 10MB）。
//!
//! 另有后台、SQLite 记录文件与访问日志，以及公告、逐文件只读 / 长期等功能。
//!
//! 环境变量：
//!     CLIP_HOST           监听地址，默认 0.0.0.0
//!     CLIP_PORT           监听端口，默认 8080
//!     CLIP_FILE           根路径内容文件，默认当前目录的 clip.txt
//!     CLIP_STORE          /a、/b 等路径文件目录，默认 clip.txt.files
//!     CLIP_TTL            有效期秒数，默认 1 天；0 = 不过期
//!     CLIP_MAX_BYTES      文本大小上限，默认 2MB
//!     CLIP_MAX_BINARY     二进制大小上限，默认 10MB
//!     CLIP_ONCE           默认阅后即焚（读取一次后清空），默认 0
//!     CLIP_TOKEN          可选访问令牌，留空则公开
//!     CLIP_ADMIN_PATH     后台路径，默认 /admin
//!     CLIP_ADMIN_PASSWORD 后台登录口令，默认取 CLIP_ADMIN_TOKEN
//!     CLIP_ADMIN_TOKEN    后台口令回退，默认取 CLIP_TOKEN；都空则后台公开
//!     CLIP_DB             SQLite 数据库路径，默认 <CLIP_STORE>/.meta/clip.db
//!     CLIP_SHARE_DIR      旧长期链接快照目录，默认 <CLIP_STORE>/.shares（仅兼容读取）
//!
//! 接口：
//!     GET  /              浏览器→网页；curl→纯文本原文
//!     GET  /raw           强制原始内容（?download=1 强制下载）
//!     GET  /<name>        具名内容
//!     POST /  PUT /       保存（请求体即内容，覆盖旧内容；?once=1 阅后即焚）
//!     DELETE /            清空
//!     GET  /d/<token>     兼容旧长期下载链接（不再新建）
//!     GET  <后台路径>      后台页面（未登录时显示登录表单）
//!     POST <后台路径>/login   后台登录
//!     GET  /healthz       健康检查

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use pbkdf2::pbkdf2_hmac;
use rusqlite::{params, Connection};
use sha2::Sha256;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const VER: &str = "Clip/5.1";

const CSS: &str = include_str!("../assets/style.css");
const ADMIN_CSS: &str = include_str!("../assets/admin.css");
const PAGE: &str = include_str!("../assets/home.html");
const ADMIN_PAGE: &str = include_str!("../assets/admin.html");
const LOGIN_PAGE: &str = include_str!("../assets/login.html");

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

struct Config {
    host: String,
    port: u16,
    file: PathBuf,
    store: PathBuf,
    ttl: i64,
    max_text: u64,
    max_binary: u64,
    token: String,
    admin_password: String,
    admin_path: String,
    once_default: bool,
    db_path: PathBuf,
    share_dir: PathBuf,
}

impl Config {
    fn from_env() -> Config {
        let file = env::var("CLIP_FILE").map(PathBuf::from).unwrap_or_else(|_| {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("clip.txt")
        });
        let store = env::var("CLIP_STORE").map(PathBuf::from).unwrap_or_else(|_| {
            let mut s = file.clone().into_os_string();
            s.push(".files");
            PathBuf::from(s)
        });
        let token = env::var("CLIP_TOKEN").unwrap_or_default();
        let admin_password = env::var("CLIP_ADMIN_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| env::var("CLIP_ADMIN_TOKEN").ok().filter(|s| !s.is_empty()))
            .or_else(|| {
                if token.is_empty() {
                    None
                } else {
                    Some(token.clone())
                }
            })
            .unwrap_or_default();
        let admin_path = normalize_admin_path(
            &env::var("CLIP_ADMIN_PATH").unwrap_or_else(|_| "/admin".to_string()),
        );
        let db_path = env::var("CLIP_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| store.join(".meta").join("clip.db"));
        let share_dir = env::var("CLIP_SHARE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| store.join(".shares"));

        Config {
            host: env::var("CLIP_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("CLIP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8080),
            file,
            store,
            ttl: env::var("CLIP_TTL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(24 * 3600),
            max_text: env::var("CLIP_MAX_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2 * 1024 * 1024),
            max_binary: env::var("CLIP_MAX_BINARY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10 * 1024 * 1024),
            token,
            admin_password,
            admin_path,
            once_default: env::var("CLIP_ONCE")
                .ok()
                .and_then(|v| parse_switch(&v))
                .unwrap_or(false),
            db_path,
            share_dir,
        }
    }

    /// 后台路径的第一段，内容文件名要避开它。
    fn admin_segment(&self) -> String {
        self.admin_path
            .trim_start_matches('/')
            .split('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("admin")
            .to_string()
    }
}

fn normalize_admin_path(raw: &str) -> String {
    let mut p = raw.trim().to_string();
    if p.is_empty() {
        p = "/admin".to_string();
    }
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    // 不能占用根路径或内容 / 分享保留路径，否则会互相冲突。
    let seg = p.trim_start_matches('/').split('/').next().unwrap_or("");
    if p == "/"
        || matches!(
            seg,
            "" | "raw" | "d" | "healthz" | "favicon.ico" | "index.html" | "download" | "dl"
        )
    {
        return "/admin".to_string();
    }
    p
}

// ---------------------------------------------------------------------------
// 存储：内容文件 + 一份 JSON 元数据（Content-Type / 原始文件名）
// ---------------------------------------------------------------------------

struct Record {
    body: Vec<u8>,
    content_type: String,
    mtime: i64,
    filename: Option<String>,
    once: bool,
    permanent: bool,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn target(cfg: &Config, name: &str) -> PathBuf {
    if name.is_empty() {
        cfg.file.clone()
    } else {
        cfg.store.join(name)
    }
}

fn meta_path(cfg: &Config, path: &Path) -> PathBuf {
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());
    cfg.store.join(".meta").join(format!("{}.json", base))
}

/// 读取内容文件旁边的元数据（Content-Type / 原始文件名 / 一次性 / 永久）。
fn meta_of(cfg: &Config, path: &Path) -> (String, Option<String>, bool, bool) {
    let mut content_type = mime_guess(&path.to_string_lossy());
    let mut filename = None;
    let mut once = false;
    let mut permanent = false;
    if let Ok(s) = fs::read_to_string(meta_path(cfg, path)) {
        if let Some(v) = json_get(&s, "content_type")
            && !v.is_empty() {
                content_type = v;
            }
        filename = json_get(&s, "filename").filter(|f| !f.is_empty());
        once = s.contains("\"once\":true");
        permanent = s.contains("\"permanent\":true");
    }
    (content_type, filename, once, permanent)
}

fn write_meta(
    cfg: &Config,
    path: &Path,
    ctype: &str,
    filename: Option<&str>,
    once: bool,
    permanent: bool,
) -> std::io::Result<()> {
    let meta = meta_path(cfg, path);
    if let Some(parent) = meta.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut json = String::from("{\"content_type\":");
    json.push_str(&json_escape(ctype));
    if let Some(f) = filename
        && !f.is_empty() {
            json.push_str(",\"filename\":");
            json.push_str(&json_escape(f));
        }
    if once {
        json.push_str(",\"once\":true");
    }
    if permanent {
        json.push_str(",\"permanent\":true");
    }
    json.push('}');
    let meta_tmp = with_suffix(&meta, ".tmp");
    fs::write(&meta_tmp, json)?;
    fs::rename(&meta_tmp, &meta)?;
    Ok(())
}

fn save(
    cfg: &Config,
    name: &str,
    body: &[u8],
    ctype: &str,
    filename: Option<&str>,
    once: bool,
) -> std::io::Result<()> {
    let path = target(cfg, name);
    // 覆盖内容时保留“永久”标记。
    let (_, _, _, permanent) = meta_of(cfg, &path);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    let tmp = with_suffix(&path, ".tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &path)?;
    write_meta(cfg, &path, ctype, filename, once, permanent)
}

fn set_meta_permanent(cfg: &Config, name: &str, on: bool) -> std::io::Result<()> {
    let path = target(cfg, name);
    let (ctype, filename, once, _) = meta_of(cfg, &path);
    write_meta(cfg, &path, &ctype, filename.as_deref(), once, on)
}

fn load(cfg: &Config, name: &str) -> Option<Record> {
    let path = target(cfg, name);
    let md = fs::metadata(&path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (content_type, filename, once, permanent) = meta_of(cfg, &path);
    // 永久文件不受 TTL 清理（时长视为 0）。
    if cfg.ttl > 0 && !permanent && now() - mtime > cfg.ttl {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(meta_path(cfg, &path));
        return None;
    }

    let body = fs::read(&path).ok()?;
    Some(Record {
        body,
        content_type,
        mtime,
        filename,
        once,
        permanent,
    })
}

fn clear(cfg: &Config, name: &str) -> bool {
    let path = target(cfg, name);
    let removed = fs::remove_file(&path).is_ok();
    let _ = fs::remove_file(meta_path(cfg, &path));
    removed
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// 文本 / 二进制 / 图片判断
// ---------------------------------------------------------------------------

/// 能否当作可编辑文本显示：合法 UTF-8 且没有奇怪的控制字符。
fn looks_like_text(body: &[u8]) -> bool {
    match std::str::from_utf8(body) {
        Ok(s) => !s
            .chars()
            .any(|c| (c as u32) < 0x20 && c != '\n' && c != '\r' && c != '\t'),
        Err(_) => false,
    }
}

fn ct_base(ct: &str) -> String {
    ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn is_image_ct(ct: &str) -> bool {
    ct_base(ct).starts_with("image/")
}

/// 声明为这些类型的按文本处理（用文本上限）。
fn is_text_ct(ct: &str) -> bool {
    let ct = ct_base(ct);
    if ct.is_empty() {
        return false;
    }
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct == "application/javascript"
        || ct == "application/x-javascript"
        || ct == "application/x-www-form-urlencoded"
        || ct == "application/x-yaml"
        || ct == "application/yaml"
        || ct == "application/toml"
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

fn mime_guess(name: &str) -> String {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ct = match ext.as_str() {
        "txt" | "text" | "log" => "text/plain; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "yaml" | "yml" => "application/yaml; charset=utf-8",
        "toml" => "application/toml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    };
    ct.to_string()
}

fn ext_for(ct: &str) -> Option<&'static str> {
    let ct = ct_base(ct);
    Some(match ct.as_str() {
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/html" => "html",
        "text/css" => "css",
        "text/javascript" | "application/javascript" => "js",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "audio/mpeg" => "mp3",
        "video/mp4" => "mp4",
        _ => return None,
    })
}

/// 根据 Content-Type / 名字给出一个带扩展名的下载文件名。
fn download_name(filename: Option<&str>, name: &str, content_type: &str) -> String {
    let mut base = filename
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if name.is_empty() {
                "clip".to_string()
            } else {
                name.to_string()
            }
        });
    if !base.contains('.')
        && let Some(ext) = ext_for(content_type) {
            base.push('.');
            base.push_str(ext);
        }
    base
}

// ---------------------------------------------------------------------------
// 小工具：URL / JSON / HTML / 时间 / 随机
// ---------------------------------------------------------------------------

fn split_url(url: &str) -> (String, String) {
    match url.find('?') {
        Some(i) => (url[..i].to_string(), url[i + 1..].to_string()),
        None => (url.to_string(), String::new()),
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if percent_decode(k) == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 解析 1/0、true/false、yes/no、on/off。
fn parse_switch(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// 记录日志前把 `token` 查询参数打码，避免令牌泄露到访问记录里。
fn redact_url(url: &str) -> String {
    let (path, query) = split_url(url);
    if query.is_empty() {
        return url.to_string();
    }
    let mut parts = Vec::new();
    for pair in query.split('&') {
        let key = pair.split('=').next().unwrap_or("");
        if percent_decode(key) == "token" {
            parts.push(format!("{}=***", key));
        } else {
            parts.push(pair.to_string());
        }
    }
    format!("{}?{}", path, parts.join("&"))
}

fn json_escape(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '<' => o.push_str("\\u003c"),
            '>' => o.push_str("\\u003e"),
            '&' => o.push_str("\\u0026"),
            '\u{2028}' => o.push_str("\\u2028"),
            '\u{2029}' => o.push_str("\\u2029"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// 从一个简单 JSON 对象里取字符串字段（够用即可，不引入 serde）。
fn json_get(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let i = s.find(&pat)?;
    let rest = &s[i + pat.len()..];
    let colon = rest.find(':')?;
    let mut chars = rest[colon + 1..].trim_start().strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(n) = u32::from_str_radix(&hex, 16)
                        && let Some(c) = char::from_u32(n) {
                            out.push(c);
                        }
                }
                other => out.push(other),
            },
            '"' => break,
            other => out.push(other),
        }
    }
    Some(out)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn mk_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes().to_vec(), value.as_bytes().to_vec()).unwrap_or_else(|_| {
        Header::from_bytes(
            name.as_bytes().to_vec(),
            b"application/octet-stream".to_vec(),
        )
        .unwrap()
    })
}

fn header_value<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn header_value_owned(req: &Request, name: &'static str) -> Option<String> {
    header_value(req, name).map(|s| s.to_string())
}

/// 由代理头 / Host 头拼出当前访问源，例如 `https://copy.mhwy.work`。
fn build_origin(forwarded_proto: Option<&str>, secure: bool, host: Option<&str>) -> String {
    let scheme = forwarded_proto
        .map(|v| v.split(',').next().unwrap_or(v).trim().to_ascii_lowercase())
        .filter(|s| s == "http" || s == "https")
        .unwrap_or_else(|| {
            if secure {
                "https".to_string()
            } else {
                "http".to_string()
            }
        });
    let host = host
        .map(|v| v.split(',').next().unwrap_or(v).trim().to_string())
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    format!("{}://{}", scheme, host)
}

fn request_origin(req: &Request) -> String {
    let proto = header_value(req, "X-Forwarded-Proto");
    let host = header_value(req, "Host").or_else(|| header_value(req, "X-Forwarded-Host"));
    build_origin(proto, req.secure(), host)
}

fn fmt_size(n: u64) -> String {
    let mut v = n as f64;
    let units = ["B", "KB", "MB", "GB"];
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}B", n)
    } else {
        format!("{:.1}{}", v, units[i])
    }
}

fn human_ttl(ttl: i64) -> String {
    if ttl <= 0 {
        return "不过期".to_string();
    }
    if ttl % 86400 == 0 {
        format!("{} 天", ttl / 86400)
    } else if ttl % 3600 == 0 {
        format!("{} 小时", ttl / 3600)
    } else {
        format!("{} 秒", ttl)
    }
}

fn human_delta(sec: i64) -> String {
    if sec >= 86400 {
        format!("{} 天 {} 小时", sec / 86400, sec % 86400 / 3600)
    } else if sec >= 3600 {
        format!("{} 小时 {} 分", sec / 3600, sec % 3600 / 60)
    } else {
        format!("{} 分", sec / 60)
    }
}

fn fmt_time(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let secs = epoch.rem_euclid(86400);
    let (_, m, d) = civil_from_days(days);
    format!(
        "{:02}-{:02} {:02}:{:02} UTC",
        m,
        d,
        secs / 3600,
        secs % 3600 / 60
    )
}

/// Howard Hinnant 的 days -> civil date 算法。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    let ok = fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let seed = now() as u128 ^ (std::process::id() as u128) << 32;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((seed >> ((i % 16) * 8)) & 0xff) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
    hex_encode(&buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        out.push(hex_val(b[i])? * 16 + hex_val(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

const ADMIN_PWD_KEY: &str = "admin_pwd";
const PBKDF2_ROUNDS: u32 = 120_000;

/// PBKDF2-SHA256 加盐散列，格式：pbkdf2$rounds$salt_hex$hash_hex。
fn hash_password(password: &str) -> String {
    let salt = random_hex(16);
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), PBKDF2_ROUNDS, &mut out);
    format!("pbkdf2${}${}${}", PBKDF2_ROUNDS, salt, hex_encode(&out))
}

fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2" {
        return false;
    }
    let rounds: u32 = match parts[1].parse::<u32>() {
        Ok(r) => r.max(1),
        Err(_) => return false,
    };
    let want = match hex_decode(parts[3]) {
        Some(w) => w,
        None => return false,
    };
    if want.len() < 16 {
        return false;
    }
    let mut out = vec![0u8; want.len()];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), parts[2].as_bytes(), rounds, &mut out);
    ct_eq(&out, &want)
}

/// 常量时间比较，避免通过响应时间猜测口令。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FileRow {
    name: String,
    content_type: String,
    filename: Option<String>,
    size: i64,
    created_at: i64,
    updated_at: i64,
    last_access: Option<i64>,
    downloads: i64,
    readonly: bool,
    permanent: bool,
}

#[derive(Clone)]
struct ShareRow {
    name: String,
    content_type: String,
    filename: Option<String>,
    expires_at: Option<i64>,
    max_downloads: Option<i64>,
    downloads: i64,
}

#[derive(Clone)]
struct LogRow {
    ts: i64,
    ip: String,
    method: String,
    path: String,
    status: i64,
    bytes: i64,
    user_agent: String,
    referer: String,
}

struct Db {
    conn: Mutex<Connection>,
    /// 写入计数，用于周期性裁剪访问日志。
    log_writes: AtomicU64,
}

const LOG_MAX: i64 = 50_000;
const LOG_PRUNE_EVERY: u64 = 500;

fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({})", table);
    if let Ok(mut stmt) = conn.prepare(&sql)
        && let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1))
    {
        return rows.flatten().any(|c| c == column);
    }
    false
}

impl Db {
    fn open(path: &Path) -> rusqlite::Result<Db> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS files(
               name TEXT PRIMARY KEY,
               path TEXT NOT NULL,
               content_type TEXT NOT NULL,
               filename TEXT,
               size INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL,
               last_access INTEGER,
               downloads INTEGER NOT NULL DEFAULT 0,
               readonly INTEGER NOT NULL DEFAULT 0,
               permanent INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS shares(
               token TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               content_type TEXT NOT NULL,
               filename TEXT,
               size INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL,
               expires_at INTEGER,
               max_downloads INTEGER,
               downloads INTEGER NOT NULL DEFAULT 0,
               last_access INTEGER,
               note TEXT
             );
             CREATE TABLE IF NOT EXISTS access_log(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               ts INTEGER NOT NULL,
               ip TEXT,
               method TEXT,
               path TEXT,
               status INTEGER,
               bytes INTEGER,
               user_agent TEXT,
               referer TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_access_ts ON access_log(ts DESC);
             CREATE INDEX IF NOT EXISTS idx_access_path ON access_log(path);
             CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        // 兼容旧库：补上 readonly 列。
        if !has_column(&conn, "files", "readonly") {
            let _ = conn.execute(
                "ALTER TABLE files ADD COLUMN readonly INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }
        if !has_column(&conn, "files", "permanent") {
            let _ = conn.execute(
                "ALTER TABLE files ADD COLUMN permanent INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }
        Ok(Db {
            conn: Mutex::new(conn),
            log_writes: AtomicU64::new(0),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_file(
        &self,
        name: &str,
        path: &str,
        content_type: &str,
        filename: Option<&str>,
        size: i64,
        created_at: i64,
        updated_at: i64,
    ) {
        let c = self.conn();
        if let Err(e) = c.execute(
            "INSERT INTO files(name,path,content_type,filename,size,created_at,updated_at,downloads) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,0) \
             ON CONFLICT(name) DO UPDATE SET \
               path=?2, content_type=?3, filename=?4, size=?5, updated_at=?7",
            params![name, path, content_type, filename, size, created_at, updated_at],
        ) {
            eprintln!("db upsert_file: {e}");
        }
    }

    fn remove_file(&self, name: &str) {
        let c = self.conn();
        let _ = c.execute("DELETE FROM files WHERE name=?1", params![name]);
    }

    fn touch_file(&self, name: &str, download: bool, ts: i64) {
        let c = self.conn();
        let _ = c.execute(
            "UPDATE files SET last_access=?1, downloads=downloads+?2 WHERE name=?3",
            params![ts, if download { 1i64 } else { 0i64 }, name],
        );
    }

    fn file_readonly(&self, name: &str) -> bool {
        let c = self.conn();
        c.query_row(
            "SELECT readonly FROM files WHERE name=?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false)
    }

    fn set_file_readonly(&self, name: &str, on: bool) -> bool {
        let c = self.conn();
        match c.execute(
            "UPDATE files SET readonly=?2 WHERE name=?1",
            params![name, if on { 1i64 } else { 0i64 }],
        ) {
            Ok(n) => n > 0,
            Err(e) => {
                eprintln!("db set_file_readonly: {e}");
                false
            }
        }
    }

    fn set_file_permanent(&self, name: &str, on: bool) -> bool {
        let c = self.conn();
        match c.execute(
            "UPDATE files SET permanent=?2 WHERE name=?1",
            params![name, if on { 1i64 } else { 0i64 }],
        ) {
            Ok(n) => n > 0,
            Err(e) => {
                eprintln!("db set_file_permanent: {e}");
                false
            }
        }
    }

    fn list_files(&self) -> Vec<FileRow> {
        let c = self.conn();
        let mut out = Vec::new();
        let prepared = c.prepare(
            "SELECT name,content_type,filename,size,created_at,updated_at,last_access,downloads,readonly,permanent \
             FROM files ORDER BY (name='') DESC, updated_at DESC",
        );
        match prepared {
            Ok(mut stmt) => match stmt.query_map([], |r| {
                Ok(FileRow {
                    name: r.get(0)?,
                    content_type: r.get(1)?,
                    filename: r.get(2)?,
                    size: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                    last_access: r.get(6)?,
                    downloads: r.get(7)?,
                    readonly: r.get::<_, i64>(8)? != 0,
                    permanent: r.get::<_, i64>(9)? != 0,
                })
            }) {
                Ok(rows) => out.extend(rows.flatten()),
                Err(e) => eprintln!("db list_files: {e}"),
            },
            Err(e) => eprintln!("db list_files: {e}"),
        }
        out
    }

    fn prune_files(&self, keep: &[String]) {
        let existing: Vec<String> = {
            let c = self.conn();
            let mut names = Vec::new();
            if let Ok(mut stmt) = c.prepare("SELECT name FROM files")
                && let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                    names.extend(rows.flatten());
                }
            names
        };
        for name in existing {
            if !keep.iter().any(|k| k == &name) {
                self.remove_file(&name);
            }
        }
    }

    /// 扫描磁盘，把文件和数据库对齐。
    fn sync_files(&self, cfg: &Config) {
        let now = now();
        let mut disk: Vec<(String, PathBuf, u64, i64)> = Vec::new();

        // 数据库自身的文件（含 WAL/SHM）不算作内容。
        let mut excluded: Vec<PathBuf> = vec![cfg.db_path.clone()];
        for suffix in ["-wal", "-shm", "-journal"] {
            excluded.push(with_suffix(&cfg.db_path, suffix));
        }

        if let Ok(entries) = fs::read_dir(&cfg.store) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if fname.starts_with('.') {
                    continue;
                }
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                if excluded.iter().any(|e| e == &p) {
                    continue;
                }
                if p.extension().map(|e| e == "tmp").unwrap_or(false) {
                    continue;
                }
                let md = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(now);
                disk.push((fname, p, md.len(), mtime));
            }
        }
        if let Ok(md) = fs::metadata(&cfg.file) {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(now);
            disk.push((String::new(), cfg.file.clone(), md.len(), mtime));
        }

        let mut keep = Vec::with_capacity(disk.len());
        for (name, path, size, mtime) in &disk {
            let (ct, filename, _once, permanent) = meta_of(cfg, path);
            self.upsert_file(
                name,
                &path.to_string_lossy(),
                &ct,
                filename.as_deref(),
                *size as i64,
                *mtime,
                *mtime,
            );
            self.set_file_permanent(name, permanent);
            keep.push(name.clone());
        }
        self.prune_files(&keep);
    }

    #[allow(clippy::too_many_arguments)]
    fn log_access(
        &self,
        ip: &str,
        method: &str,
        path: &str,
        status: i64,
        bytes: i64,
        user_agent: &str,
        referer: &str,
    ) {
        let path: String = path.chars().take(1024).collect();
        let ua: String = user_agent.chars().take(512).collect();
        let referer: String = referer.chars().take(512).collect();
        let c = self.conn();
        let _ = c.execute(
            "INSERT INTO access_log(ts,ip,method,path,status,bytes,user_agent,referer) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![now(), ip, method, path, status, bytes, ua, referer],
        );

        // 控制日志表体量，避免无限增长。
        let n = self.log_writes.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(LOG_PRUNE_EVERY) {
            let _ = c.execute(
                "DELETE FROM access_log WHERE id NOT IN \
                 (SELECT id FROM access_log ORDER BY id DESC LIMIT ?1)",
                params![LOG_MAX],
            );
        }
    }

    fn list_logs(&self, limit: i64) -> Vec<LogRow> {
        let c = self.conn();
        let mut out = Vec::new();
        if let Ok(mut stmt) = c.prepare(
            "SELECT ts,ip,method,path,status,bytes,user_agent,referer \
             FROM access_log ORDER BY id DESC LIMIT ?1",
        )
            && let Ok(rows) = stmt.query_map(params![limit], |r| {
                Ok(LogRow {
                    ts: r.get(0)?,
                    ip: r.get(1)?,
                    method: r.get(2)?,
                    path: r.get(3)?,
                    status: r.get(4)?,
                    bytes: r.get(5)?,
                    user_agent: r.get(6)?,
                    referer: r.get(7)?,
                })
            }) {
                out.extend(rows.flatten());
            }
        out
    }

    fn clear_logs(&self) {
        let c = self.conn();
        let _ = c.execute("DELETE FROM access_log", []);
    }

    fn log_count(&self) -> i64 {
        let c = self.conn();
        c.query_row("SELECT COUNT(*) FROM access_log", [], |r| r.get(0))
            .unwrap_or(0)
    }

    fn get_setting(&self, key: &str) -> Option<String> {
        let c = self.conn();
        c.query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![key],
            |r| r.get(0),
        )
        .ok()
    }

    fn set_setting(&self, key: &str, value: &str) -> bool {
        let c = self.conn();
        match c.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        ) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("db set_setting: {e}");
                false
            }
        }
    }

    fn get_share(&self, token: &str) -> Option<ShareRow> {
        let c = self.conn();
        c.query_row(
            "SELECT name,content_type,filename,expires_at,max_downloads,downloads \
             FROM shares WHERE token=?1",
            params![token],
            |r| {
                Ok(ShareRow {
                    name: r.get(0)?,
                    content_type: r.get(1)?,
                    filename: r.get(2)?,
                    expires_at: r.get(3)?,
                    max_downloads: r.get(4)?,
                    downloads: r.get(5)?,
                })
            },
        )
        .ok()
    }

    fn bump_share(&self, token: &str) {
        let c = self.conn();
        let _ = c.execute(
            "UPDATE shares SET downloads=downloads+1, last_access=?2 WHERE token=?1",
            params![token, now()],
        );
    }

    fn delete_share(&self, token: &str) {
        let c = self.conn();
        let _ = c.execute("DELETE FROM shares WHERE token=?1", params![token]);
    }
}

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

struct AppState {
    cfg: Config,
    db: Db,
    /// 后台登录会话：session id -> 过期时间戳
    sessions: Mutex<HashMap<String, i64>>,
    /// 登录失败节流：ip -> (次数, 最后失败时间)
    login_fails: Mutex<HashMap<String, (u32, i64)>>,
}

// ---------------------------------------------------------------------------
// 页面样式（前台 + 后台共用）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 前台页面
// ---------------------------------------------------------------------------

fn disposition_header(filename: &str) -> Header {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let value = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii,
        percent_encode(filename)
    );
    mk_header("Content-Disposition", &value)
}

fn render_page(
    cfg: &Config,
    origin: &str,
    path: &str,
    name: &str,
    rec: Option<&Record>,
    readonly: bool,
    announcement: &str,
) -> String {
    let empty: Vec<u8> = Vec::new();
    let body_bytes: &[u8] = rec.map(|r| r.body.as_slice()).unwrap_or(&empty);
    let content_type = rec.map(|r| r.content_type.clone()).unwrap_or_default();
    let mtime = rec.map(|r| r.mtime).unwrap_or(0);
    let is_text = rec.map(|r| looks_like_text(&r.body)).unwrap_or(true);
    let is_image = rec
        .map(|r| is_image_ct(&r.content_type) && !r.body.is_empty())
        .unwrap_or(false);

    // 原始内容地址（根路径的原始内容在 /raw）
    let raw_base = if name.is_empty() {
        "/raw".to_string()
    } else {
        path.to_string()
    };
    let tq = if cfg.token.is_empty() {
        String::new()
    } else {
        format!("?token={}", percent_encode(&cfg.token))
    };
    let inline_url = format!("{}{}", raw_base, tq);
    let dl_url = format!(
        "{}{}download=1",
        raw_base,
        if tq.is_empty() { "?" } else { "&" }
    );
    // 示例命令里也带上 token，否则复制出去执行会得到 401。
    let display_base = format!(
        "{}{}{}",
        origin,
        if name.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", name)
        },
        tq
    );
    let display_raw = format!("{}{}{}", origin, raw_base, tq);
    let readonly_attr = if readonly { " readonly" } else { "" };
    let readonly_hidden = if readonly { "hidden" } else { "" };
    let readonly_banner = if readonly {
        r#"<div class="notice">该内容为只读，暂不接受保存、上传或删除。</div>"#
    } else {
        ""
    };
    let announcement_html = if announcement.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="announce">{}</div>"#,
            html_escape(announcement)
        )
    };

    let body_html = if is_image {
        format!(
            r#"<figure class="media"><img class="preview" src="{}" alt=""><figcaption class="fmeta">{} · {}</figcaption></figure>"#,
            html_escape(&inline_url),
            html_escape(&content_type),
            fmt_size(body_bytes.len() as u64)
        )
    } else if !is_text {
        let fname = download_name(rec.and_then(|r| r.filename.as_deref()), name, &content_type);
        format!(
            r#"<div class="file-card"><div class="ficon" aria-hidden="true">&#8681;</div><div class="fwrap"><div class="fname">{}</div><div class="fmeta">{} · {}</div></div><a class="btn primary" href="{}" download>下载</a></div>"#,
            html_escape(&fname),
            html_escape(if content_type.is_empty() {
                "application/octet-stream"
            } else {
                &content_type
            }),
            fmt_size(body_bytes.len() as u64),
            html_escape(&dl_url)
        )
    } else {
        format!(
            r#"<textarea id="c" spellcheck="false" placeholder="空的。粘贴或拖入内容，点保存。"{}>{}</textarea>"#,
            readonly_attr,
            html_escape(&String::from_utf8_lossy(body_bytes))
        )
    };

    let actions = if is_text {
        let mut a = String::from(r#"<button id="cp" type="button">复制全文</button>"#);
        if !readonly {
            a.push_str(
                r#"
  <button id="save" type="button" class="primary">保存</button>
  <button class="danger" id="clr" type="button">清空</button>"#,
            );
        }
        a
    } else {
        let mut a = format!(
            r#"<a class="btn primary" href="{}" download>下载</a>"#,
            html_escape(&dl_url)
        );
        if !readonly {
            a.push_str(
                r#"
  <button class="danger" id="clr" type="button">清空</button>"#,
            );
        }
        a
    };

    let hint = if readonly {
        "只读模式"
    } else if is_text {
        "Ctrl / ⌘ + Enter 保存"
    } else {
        "拖入文件或点「上传文件」可替换"
    };

    let mut meta_parts: Vec<String> = Vec::new();
    if !body_bytes.is_empty() {
        if is_text {
            meta_parts.push(format!(
                "{} 字符",
                format_number(String::from_utf8_lossy(body_bytes).chars().count() as u64)
            ));
        } else {
            meta_parts.push(fmt_size(body_bytes.len() as u64));
        }
        meta_parts.push(format!("保存于 {}", fmt_time(mtime)));
        let is_permanent = rec.map(|r| r.permanent).unwrap_or(false);
        if is_permanent {
            meta_parts.push("永久保留".to_string());
        } else if cfg.ttl > 0 {
            meta_parts.push(format!(
                "剩余 {}",
                human_delta((mtime + cfg.ttl - now()).max(0))
            ));
        }
    }
    let meta = meta_parts.join(" · ");
    let is_once = rec.map(|r| r.once).unwrap_or(false);
    let is_permanent = rec.map(|r| r.permanent).unwrap_or(false);
    let chip = if is_once {
        "阅后即焚 · 读取后清空".to_string()
    } else if is_permanent {
        "永久保留".to_string()
    } else if cfg.ttl > 0 {
        format!("保留 {}", human_ttl(cfg.ttl))
    } else {
        "永不过期".to_string()
    };
    let once_checked = if cfg.once_default || is_once {
        "checked"
    } else {
        ""
    };
    let title = if name.is_empty() {
        "根内容".to_string()
    } else {
        format!("/{}", name)
    };

    PAGE.replace("__CSS__", CSS)
        .replace("__TITLE__", &html_escape(&title))
        .replace("__BODY__", &body_html)
        .replace("__ACTIONS__", &actions)
        .replace("__HINT__", hint)
        .replace("__CHIP__", &html_escape(&chip))
        .replace("__ONCE_CHECKED__", once_checked)
        .replace("__READONLY_BANNER__", readonly_banner)
        .replace("__ANNOUNCEMENT__", &announcement_html)
        .replace("__READONLY_HIDDEN__", readonly_hidden)
        .replace("__READONLY_JSON__", if readonly { "true" } else { "false" })
        .replace("__META__", &html_escape(&meta))
        .replace("__B__", &html_escape(&display_base))
        .replace("__D__", &html_escape(&display_raw))
        .replace("__PATH_JSON__", &json_escape(path))
        .replace("__TOK__", &js_escape(cfg.token.as_str()))
        .replace("__MAXN__", &cfg.max_text.to_string())
        .replace("__MAXBN__", &cfg.max_binary.to_string())
        .replace(
            "__FILE_NAME__",
            &html_escape(
                &cfg.file
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "clip.txt".to_string()),
            ),
        )
        .replace("__MAXS__", &html_escape(&fmt_size(cfg.max_text)))
        .replace("__MAXBS__", &html_escape(&fmt_size(cfg.max_binary)))
        .replace("__VER__", VER)
}

/// JS 单引号字符串里的转义。
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\x3c"),
            '>' => out.push_str("\\x3e"),
            '&' => out.push_str("\\x26"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 后台页面
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn respond(
    state: &AppState,
    req: Request,
    code: u16,
    body: Vec<u8>,
    ctype: &str,
    extra: Vec<Header>,
) {
    let method = req.method().to_string();
    let url = req.url().to_string();
    let ip = req
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let ua = header_value_owned(&req, "User-Agent").unwrap_or_default();
    let referer = header_value_owned(&req, "Referer").unwrap_or_default();
    let len = body.len();

    let mut resp = Response::from_data(body).with_status_code(StatusCode(code));
    resp.add_header(mk_header("Content-Type", ctype));
    resp.add_header(mk_header("Cache-Control", "no-store"));
    resp.add_header(mk_header("X-Content-Type-Options", "nosniff"));
    resp.add_header(mk_header("Referrer-Policy", "no-referrer"));
    resp.add_header(mk_header("X-Frame-Options", "DENY"));
    for h in extra {
        resp.add_header(h);
    }
    let _ = req.respond(resp);

    if url != "/favicon.ico" && !url.starts_with("/healthz") {
        state.db.log_access(
            &ip,
            &method,
            &redact_url(&url),
            code as i64,
            len as i64,
            &ua,
            &referer,
        );
    }
}

fn json_error(msg: &str) -> Vec<u8> {
    format!("{{\"error\":{}}}", json_escape(msg)).into_bytes()
}

fn json_response(state: &AppState, req: Request, code: u16, body: Vec<u8>) {
    respond(state, req, code, body, "application/json; charset=utf-8", vec![]);
}

/// 读取请求体。优先按 Content-Length 截断：某些反代会给所有请求加
/// `Connection: upgrade`，此时 tiny_http 会把整条长连接当作 body，
/// `read_to_end` 会一直等到连接关闭。按 Content-Length 读取可避免卡死。
fn read_body_limited(req: &mut Request, limit: u64) -> std::io::Result<Vec<u8>> {
    let cap = match req.body_length() {
        Some(n) => (n as u64).min(limit),
        None => limit,
    };
    let mut body = Vec::new();
    if cap > 0 {
        let mut reader = req.as_reader().take(cap);
        reader.read_to_end(&mut body)?;
    }
    Ok(body)
}

fn authorized(cfg: &Config, req: &Request, query: &str) -> bool {
    if cfg.token.is_empty() {
        return true;
    }
    if header_value(req, "X-Clip-Token") == Some(cfg.token.as_str()) {
        return true;
    }
    query_get(query, "token").as_deref() == Some(cfg.token.as_str())
}

const SESSION_COOKIE: &str = "clip_sid";
const SESSION_TTL: i64 = 7 * 24 * 3600;
const LOGIN_MAX_FAILS: u32 = 5;
const LOGIN_LOCK_SECS: i64 = 300;
const ANNOUNCEMENT_KEY: &str = "announcement";
const ANNOUNCEMENT_MAX_CHARS: usize = 2000;

fn get_announcement(state: &AppState) -> String {
    state.db.get_setting(ANNOUNCEMENT_KEY).unwrap_or_default()
}

/// 内容为只读时拒绝修改请求。
fn deny_readonly(state: &AppState, req: Request) {
    let is_fetch = wants_html(&req) || header_value(&req, "X-Requested-With") == Some("fetch");
    if is_fetch {
        json_response(state, req, 403, json_error("该内容为只读，禁止修改"));
    } else {
        respond(
            state,
            req,
            403,
            b"readonly: this item cannot be modified\n".to_vec(),
            "text/plain; charset=utf-8",
            vec![],
        );
    }
}

/// 后台是否已配置口令（环境变量或数据库中已保存）。
fn admin_enabled(state: &AppState) -> bool {
    state.db.get_setting(ADMIN_PWD_KEY).is_some() || !state.cfg.admin_password.is_empty()
}

/// 校验后台口令：优先数据库中的自定义口令，其次环境变量口令（便于找回）。
fn verify_admin_password(state: &AppState, password: &str) -> bool {
    if let Some(hash) = state.db.get_setting(ADMIN_PWD_KEY)
        && verify_password(password, &hash) {
            return true;
        }
    !state.cfg.admin_password.is_empty() && ct_eq(password.as_bytes(), state.cfg.admin_password.as_bytes())
}

fn cookie_value(req: &Request, name: &str) -> Option<String> {
    let cookies = header_value(req, "Cookie")?;
    for part in cookies.split(';') {
        if let Some((k, v)) = part.trim().split_once('=')
            && k == name {
                return Some(v.to_string());
            }
    }
    None
}

fn session_valid(state: &AppState, req: &Request) -> bool {
    let sid = match cookie_value(req, SESSION_COOKIE) {
        Some(s) => s,
        None => return false,
    };
    let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    match sessions.get(&sid) {
        Some(exp) if *exp > now() => true,
        Some(_) => {
            sessions.remove(&sid);
            false
        }
        None => false,
    }
}

fn new_session(state: &AppState) -> String {
    let sid = random_hex(24);
    let ts = now();
    let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|_, exp| *exp > ts);
    sessions.insert(sid.clone(), ts + SESSION_TTL);
    sid
}

fn cookie_header(req: &Request, path: &str, value: &str, max_age: i64) -> Header {
    let secure = req.secure()
        || header_value(req, "X-Forwarded-Proto")
            .map(|v| v.eq_ignore_ascii_case("https"))
            .unwrap_or(false);
    let mut v = format!(
        "{}={}; Path={}; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_COOKIE, value, path, max_age
    );
    if secure {
        v.push_str("; Secure");
    }
    mk_header("Set-Cookie", &v)
}

/// 后台仅通过登录会话鉴权（不再接受令牌绕过，确保必须输入口令）。
fn admin_authorized(state: &AppState, req: &Request) -> bool {
    session_valid(state, req)
}

fn wants_html(req: &Request) -> bool {
    header_value(req, "Accept")
        .map(|a| a.to_ascii_lowercase().contains("text/html"))
        .unwrap_or(false)
}

/// 解析路径：返回内容名；根路径与 /raw 返回空字符串，非法路径返回 None。
fn name_of(cfg: &Config, path: &str, allow_raw: bool) -> Option<String> {
    if path == "/" {
        return Some(String::new());
    }
    if allow_raw && path == "/raw" {
        return Some(String::new());
    }
    let decoded = percent_decode(path);
    let parts: Vec<&str> = decoded.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() != 1 {
        return None;
    }
    let name = parts[0];
    if name.starts_with('.') || name.contains('\0') {
        return None;
    }
    if is_reserved_name(cfg, name) {
        return None;
    }
    Some(name.to_string())
}

fn is_reserved_name(cfg: &Config, name: &str) -> bool {
    matches!(
        name,
        "raw" | "healthz" | "favicon.ico" | "index.html" | "download" | "dl" | "admin" | "d"
    ) || name == cfg.admin_segment()
}

/// 阅后即焚：内容已被本次响应带走，删除文件与数据库记录。
fn consume_once(state: &AppState, name: &str, rec: &Option<Record>) {
    if rec.as_ref().map(|r| r.once).unwrap_or(false) {
        clear(&state.cfg, name);
        state.db.remove_file(name);
    }
}

fn do_get(state: &AppState, req: Request, path: String, query: String) {
    let cfg = &state.cfg;
    if path == "/healthz" {
        return respond(state, req, 200, b"ok\n".to_vec(), "text/plain; charset=utf-8", vec![]);
    }
    if path == "/favicon.ico" {
        return respond(state, req, 204, Vec::new(), "text/plain; charset=utf-8", vec![]);
    }
    if !authorized(cfg, &req, &query) {
        return respond(
            state,
            req,
            401,
            b"unauthorized\n".to_vec(),
            "text/plain; charset=utf-8",
            vec![],
        );
    }

    let name = match name_of(cfg, &path, true) {
        Some(n) => n,
        None => {
            return respond(
                state,
                req,
                404,
                b"not found\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            )
        }
    };

    let rec = load(cfg, &name);
    let is_root = name.is_empty();
    let download = query_get(&query, "download").is_some();
    let raw = path == "/raw" || !wants_html(&req) || download;

    if raw {
        let (body, ct) = match &rec {
            Some(r) => (r.body.clone(), r.content_type.clone()),
            None => (Vec::new(), "text/plain; charset=utf-8".to_string()),
        };
        let mut extra = Vec::new();
        let binary = rec
            .as_ref()
            .map(|r| !looks_like_text(&r.body))
            .unwrap_or(false);
        if download || binary {
            let fname = download_name(rec.as_ref().and_then(|r| r.filename.as_deref()), &name, &ct);
            extra.push(disposition_header(&fname));
        }
        if rec.is_some() {
            state.db.touch_file(&name, download || binary, now());
        }
        consume_once(state, &name, &rec);
        return respond(state, req, 200, body, &ct, extra);
    }

    // 根路径的一次性图片/二进制：直接返回原始内容，保证一次请求完成。
    if is_root
        && let Some(r) = &rec
        && r.once
        && !looks_like_text(&r.body)
    {
        let ct = r.content_type.clone();
        let body = r.body.clone();
        let extra = if is_image_ct(&ct) {
            vec![]
        } else {
            vec![disposition_header(&download_name(
                r.filename.as_deref(),
                &name,
                &ct,
            ))]
        };
        state.db.touch_file(&name, true, now());
        consume_once(state, &name, &rec);
        return respond(state, req, 200, body, &ct, extra);
    }

    // 浏览器：/name 直接内联图片或下载二进制；根路径渲染网页。
    if !is_root
        && let Some(r) = &rec {
            if is_image_ct(&r.content_type) {
                let body = r.body.clone();
                let ct = r.content_type.clone();
                state.db.touch_file(&name, false, now());
                consume_once(state, &name, &rec);
                return respond(state, req, 200, body, &ct, vec![]);
            }
            if !looks_like_text(&r.body) {
                let fname = download_name(r.filename.as_deref(), &name, &r.content_type);
                let extra = vec![disposition_header(&fname)];
                let ct = r.content_type.clone();
                let body = r.body.clone();
                state.db.touch_file(&name, true, now());
                consume_once(state, &name, &rec);
                return respond(state, req, 200, body, &ct, extra);
            }
        }

    if rec.is_some() {
        state.db.touch_file(&name, false, now());
    }
    let origin = request_origin(&req);
    let html = render_page(
        cfg,
        &origin,
        &path,
        &name,
        rec.as_ref(),
        state.db.file_readonly(&name),
        &get_announcement(state),
    );
    consume_once(state, &name, &rec);
    respond(
        state,
        req,
        200,
        html.into_bytes(),
        "text/html; charset=utf-8",
        vec![],
    );
}

fn do_write(state: &AppState, mut req: Request, path: String, query: String) {
    let cfg = &state.cfg;
    let name = match name_of(cfg, &path, false) {
        Some(n) => n,
        None => {
            return respond(
                state,
                req,
                404,
                b"not found\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            )
        }
    };
    if !authorized(cfg, &req, &query) {
        return respond(
            state,
            req,
            401,
            b"unauthorized\n".to_vec(),
            "text/plain; charset=utf-8",
            vec![],
        );
    }
    if state.db.file_readonly(&name) {
        return deny_readonly(state, req);
    }

    let declared = header_value_owned(&req, "Content-Type")
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let resolved = if declared.is_empty() {
        mime_guess(&if name.is_empty() {
            cfg.file.to_string_lossy().into_owned()
        } else {
            name.clone()
        })
    } else {
        declared.clone()
    };
    let limit = if is_text_ct(&resolved) {
        cfg.max_text
    } else {
        cfg.max_binary
    };
    let hard = cfg.max_text.max(cfg.max_binary);
    let declared_len = req.body_length().unwrap_or(0) as u64;

    if declared_len > hard {
        return json_response(
            state,
            req,
            413,
            json_error(&format!("内容超过 {} 上限", fmt_size(hard))),
        );
    }

    let body = match read_body_limited(&mut req, hard + 1) {
        Ok(b) => b,
        Err(_) => return json_response(state, req, 400, json_error("读取请求体失败")),
    };
    if body.len() as u64 > hard {
        return json_response(
            state,
            req,
            413,
            json_error(&format!("内容超过 {} 上限", fmt_size(hard))),
        );
    }

    let content = if declared == "application/json" {
        match std::str::from_utf8(&body)
            .ok()
            .and_then(|s| json_get(s, "content"))
        {
            Some(c) => c.into_bytes(),
            None => return json_response(state, req, 400, json_error("JSON 解析失败")),
        }
    } else {
        body
    };

    if content.is_empty() {
        return json_response(state, req, 400, json_error("内容为空"));
    }
    if content.len() as u64 > limit {
        return json_response(
            state,
            req,
            413,
            json_error(&format!("内容超过 {} 上限", fmt_size(limit))),
        );
    }

    let stored_ct = if declared.is_empty() { resolved } else { declared.clone() };
    let filename = header_value_owned(&req, "X-Clip-Filename")
        .map(|f| percent_decode(&f))
        .filter(|f| !f.is_empty());
    // 阅后即焚：请求头 / 查询参数优先，否则用全局默认。
    let once = header_value_owned(&req, "X-Clip-Once")
        .and_then(|v| parse_switch(&v))
        .or_else(|| query_get(&query, "once").and_then(|v| parse_switch(&v)))
        .unwrap_or(cfg.once_default);

    if let Err(e) = save(cfg, &name, &content, &stored_ct, filename.as_deref(), once) {
        eprintln!("写入失败: {e}");
        return json_response(state, req, 500, json_error("写入失败"));
    }

    let path_buf = target(cfg, &name);
    let ts = now();
    state
        .db
        .upsert_file(&name, &path_buf.to_string_lossy(), &stored_ct, filename.as_deref(), content.len() as i64, ts, ts);

    let is_fetch =
        wants_html(&req) || header_value(&req, "X-Requested-With") == Some("fetch");
    if is_fetch {
        json_response(
            state,
            req,
            200,
            format!("{{\"ok\":true,\"bytes\":{},\"once\":{}}}", content.len(), once).into_bytes(),
        );
    } else {
        respond(
            state,
            req,
            200,
            format!("saved {}\n", fmt_size(content.len() as u64)).into_bytes(),
            "text/plain; charset=utf-8",
            vec![],
        );
    }
}

fn do_delete(state: &AppState, req: Request, path: String, query: String) {
    let cfg = &state.cfg;
    let name = match name_of(cfg, &path, false) {
        Some(n) => n,
        None => {
            return respond(
                state,
                req,
                404,
                b"not found\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            )
        }
    };
    if !authorized(cfg, &req, &query) {
        return respond(
            state,
            req,
            401,
            b"unauthorized\n".to_vec(),
            "text/plain; charset=utf-8",
            vec![],
        );
    }
    if state.db.file_readonly(&name) {
        return deny_readonly(state, req);
    }
    clear(cfg, &name);
    state.db.remove_file(&name);
    respond(
        state,
        req,
        200,
        b"cleared\n".to_vec(),
        "text/plain; charset=utf-8",
        vec![],
    );
}

// ---- 旧长期链接（兼容读取） ----

fn do_share_download(state: &AppState, req: Request, token: String) {
    let share = match state.db.get_share(&token) {
        Some(s) => s,
        None => {
            return respond(
                state,
                req,
                404,
                b"not found\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            )
        }
    };

    let ts = now();
    if let Some(exp) = share.expires_at
        && ts > exp {
            remove_share(state, &token);
            return respond(
                state,
                req,
                410,
                b"link expired\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            );
        }
    if let Some(max) = share.max_downloads
        && share.downloads >= max {
            return respond(
                state,
                req,
                410,
                b"download limit reached\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            );
        }

    let path = state.cfg.share_dir.join(&token);
    let body = match fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            return respond(
                state,
                req,
                410,
                b"file gone\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            )
        }
    };

    let fname = download_name(share.filename.as_deref(), &share.name, &share.content_type);
    state.db.bump_share(&token);
    respond(
        state,
        req,
        200,
        body,
        &share.content_type,
        vec![disposition_header(&fname)],
    );
}

fn remove_share(state: &AppState, token: &str) {
    let _ = fs::remove_file(state.cfg.share_dir.join(token));
    state.db.delete_share(token);
}

// ---- 后台 ----

fn do_admin(state: &AppState, req: Request, method: &Method, path: String, query: String) {
    let base = state.cfg.admin_path.clone();
    let rest = if path == base {
        String::new()
    } else {
        path.strip_prefix(&format!("{}/", base))
            .unwrap_or("")
            .to_string()
    };
    let is_get = matches!(method, Method::Get | Method::Head);

    if rest == "logout" {
        return admin_logout(state, req);
    }

    // 设置了口令才要求登录；未设置时按原先行为公开（启动时告警并在页面提示）。
    if admin_enabled(state) {
        if rest == "login" {
            if is_get {
                return admin_login_page(state, req, "");
            }
            return admin_login_submit(state, req);
        }
        if !admin_authorized(state, &req) {
            if is_get && wants_html(&req) {
                return admin_login_page(state, req, "");
            }
            return json_response(state, req, 401, json_error("未登录"));
        }
    }

    if rest.is_empty() && is_get {
        state.db.sync_files(&state.cfg);
        let files = state.db.list_files();
        let chip = format!("{} 文件 · {} 条日志", files.len(), state.db.log_count());
        let home = if state.cfg.token.is_empty() {
            "/".to_string()
        } else {
            format!("/?token={}", percent_encode(&state.cfg.token))
        };
        let pwd_note = if state.db.get_setting(ADMIN_PWD_KEY).is_some() {
            "当前使用自定义口令（存于数据库），留空当前口令无法修改。".to_string()
        } else if state.cfg.admin_password.is_empty() {
            "当前未设置口令，任何能访问此地址的人都可管理，建议设置一个。".to_string()
        } else {
            "当前使用环境变量口令（CLIP_ADMIN_PASSWORD / CLIP_ADMIN_TOKEN / CLIP_TOKEN）。".to_string()
        };
        let announcement = get_announcement(state);
        let html = ADMIN_PAGE
            .replace("__CSS__", &format!("{}\n{}", CSS, ADMIN_CSS))
            .replace("__CHIP__", &html_escape(&chip))
            .replace("__HOME__", &html_escape(&home))
            .replace("__BASE_JSON__", &json_escape(&base))
            .replace("__LOGOUT__", &html_escape(&format!("{}/logout", base)))
            .replace("__PWDNOTE__", &html_escape(&pwd_note))
            .replace("__ANNOUNCEMENT__", &html_escape(&announcement))
            .replace("__DB__", &html_escape(&state.cfg.db_path.to_string_lossy()))
            .replace("__VER__", VER);
        return respond(
            state,
            req,
            200,
            html.into_bytes(),
            "text/html; charset=utf-8",
            vec![],
        );
    }

    if rest == "api/files" {
        if is_get {
            state.db.sync_files(&state.cfg);
            let files = state.db.list_files();
            let to = if state.cfg.token.is_empty() {
                String::new()
            } else {
                format!("?token={}", percent_encode(&state.cfg.token))
            };
            let mut s = String::from("[");
            for (i, f) in files.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let url = if f.name.is_empty() {
                    format!("/raw{}", to)
                } else {
                    format!("/{}{}", f.name, to)
                };
                s.push_str(&format!(
                    "{{\"name\":{},\"content_type\":{},\"filename\":{},\"size\":{},\"created_at\":{},\"updated_at\":{},\"last_access\":{},\"downloads\":{},\"readonly\":{},\"permanent\":{},\"url\":{}}}",
                    json_escape(&f.name),
                    json_escape(&f.content_type),
                    f.filename.as_deref().map(json_escape).unwrap_or_else(|| "null".into()),
                    f.size,
                    f.created_at,
                    f.updated_at,
                    f.last_access.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                    f.downloads,
                    f.readonly,
                    f.permanent,
                    json_escape(&url)
                ));
            }
            s.push(']');
            return json_response(state, req, 200, s.into_bytes());
        }
        if *method == Method::Delete {
            let name = query_get(&query, "name").unwrap_or_default();
            let valid = name.is_empty() || name_of(&state.cfg, &format!("/{}", name), false).is_some();
            if !valid {
                return json_response(state, req, 400, json_error("非法名称"));
            }
            if state.db.file_readonly(&name) {
                return json_response(state, req, 403, json_error("该文件为只读，禁止删除"));
            }
            clear(&state.cfg, &name);
            state.db.remove_file(&name);
            return json_response(state, req, 200, b"{\"ok\":true}".to_vec());
        }
    }

    if rest == "api/logs" {
        if is_get {
            let limit = query_get(&query, "limit")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(200)
                .clamp(1, 2000);
            let logs = state.db.list_logs(limit);
            let mut s = String::from("[");
            for (i, l) in logs.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    "{{\"ts\":{},\"ip\":{},\"method\":{},\"path\":{},\"status\":{},\"bytes\":{},\"user_agent\":{},\"referer\":{}}}",
                    l.ts,
                    json_escape(&l.ip),
                    json_escape(&l.method),
                    json_escape(&l.path),
                    l.status,
                    l.bytes,
                    json_escape(&l.user_agent),
                    json_escape(&l.referer)
                ));
            }
            s.push(']');
            return json_response(state, req, 200, s.into_bytes());
        }
        if *method == Method::Delete {
            state.db.clear_logs();
            return json_response(state, req, 200, b"{\"ok\":true}".to_vec());
        }
    }

    if rest == "api/password" && *method == Method::Post {
        return admin_change_password(state, req);
    }

    if rest == "api/files/readonly" && *method == Method::Post {
        return admin_set_file_readonly(state, req);
    }

    if rest == "api/files/permanent" && *method == Method::Post {
        return admin_set_file_permanent(state, req);
    }

    if rest == "api/announcement" && *method == Method::Post {
        return admin_set_announcement(state, req);
    }

    respond(
        state,
        req,
        404,
        b"not found\n".to_vec(),
        "text/plain; charset=utf-8",
        vec![],
    );
}

fn admin_login_html(state: &AppState, error: &str) -> String {
    let hint = if error.is_empty() {
        "请输入后台口令".to_string()
    } else {
        html_escape(error)
    };
    LOGIN_PAGE
        .replace("__CSS__", CSS)
        .replace(
            "__ACTION__",
            &html_escape(&format!("{}/login", state.cfg.admin_path)),
        )
        .replace("__ERROR__", &hint)
        .replace("__VER__", VER)
}

fn admin_login_page(state: &AppState, req: Request, error: &str) {
    let html = admin_login_html(state, error);
    respond(
        state,
        req,
        200,
        html.into_bytes(),
        "text/html; charset=utf-8",
        vec![],
    );
}

fn admin_login_submit(state: &AppState, mut req: Request) {
    let ip = req
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();

    // 登录失败节流：同一 IP 连续失败过多时暂时锁定。
    {
        let fails = state
            .login_fails
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((n, last)) = fails.get(&ip)
            && *n >= LOGIN_MAX_FAILS
            && now() - *last < LOGIN_LOCK_SECS
        {
            drop(fails);
            let html = admin_login_html(state, "尝试次数过多，请 5 分钟后再试");
            return respond(
                state,
                req,
                429,
                html.into_bytes(),
                "text/html; charset=utf-8",
                vec![mk_header("Retry-After", "300")],
            );
        }
    }

    let declared = header_value_owned(&req, "Content-Type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = read_body_limited(&mut req, 4096).unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let password = if declared.contains("application/json") {
        json_get(&text, "password").unwrap_or_default()
    } else {
        query_get(&text, "password").unwrap_or_default()
    };

    if verify_admin_password(state, &password) {
        state
            .login_fails
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ip);
        let sid = new_session(state);
        let cookie = cookie_header(&req, &state.cfg.admin_path, &sid, SESSION_TTL);
        let loc = state.cfg.admin_path.clone();
        respond(
            state,
            req,
            303,
            Vec::new(),
            "text/plain; charset=utf-8",
            vec![mk_header("Location", &loc), cookie],
        );
    } else {
        let ts = now();
        let count = {
            let mut fails = state
                .login_fails
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if fails.len() > 1000 {
                fails.retain(|_, (_, last)| ts - *last < LOGIN_LOCK_SECS);
            }
            let entry = fails.entry(ip.clone()).or_insert((0, ts));
            entry.0 += 1;
            entry.1 = ts;
            entry.0
        };
        std::thread::sleep(std::time::Duration::from_millis(400));
        let msg = if count >= LOGIN_MAX_FAILS {
            "口令错误，尝试次数过多，请稍后再试"
        } else {
            "口令错误，请重试"
        };
        admin_login_page(state, req, msg);
    }
}

fn admin_logout(state: &AppState, req: Request) {
    if let Some(sid) = cookie_value(&req, SESSION_COOKIE) {
        state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&sid);
    }
    // 同时按后台路径与根路径清理，兼容旧 Cookie。
    let cookies = vec![
        cookie_header(&req, &state.cfg.admin_path, "", 0),
        cookie_header(&req, "/", "", 0),
    ];
    let loc = if admin_enabled(state) {
        format!("{}/login", state.cfg.admin_path)
    } else {
        state.cfg.admin_path.clone()
    };
    let mut extra = vec![mk_header("Location", &loc)];
    extra.extend(cookies);
    respond(
        state,
        req,
        303,
        Vec::new(),
        "text/plain; charset=utf-8",
        extra,
    );
}

fn admin_change_password(state: &AppState, mut req: Request) {
    let body = read_body_limited(&mut req, 8192).unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let current = json_get(&text, "current").unwrap_or_default();
    let new = json_get(&text, "new").unwrap_or_default();

    if admin_enabled(state) && !verify_admin_password(state, &current) {
        std::thread::sleep(std::time::Duration::from_millis(300));
        return json_response(state, req, 403, json_error("当前口令不正确"));
    }
    if new.chars().count() < 4 {
        return json_response(state, req, 400, json_error("新口令至少 4 位"));
    }
    if !state.db.set_setting(ADMIN_PWD_KEY, &hash_password(&new)) {
        return json_response(state, req, 500, json_error("保存失败"));
    }
    // 口令变更后让所有旧会话失效，要求重新登录。
    state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    json_response(state, req, 200, b"{\"ok\":true}".to_vec());
}

fn admin_set_file_readonly(state: &AppState, mut req: Request) {
    let body = read_body_limited(&mut req, 4096).unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let name = json_get(&text, "name").unwrap_or_default();
    let valid = name.is_empty() || name_of(&state.cfg, &format!("/{}", name), false).is_some();
    if !valid {
        return json_response(state, req, 400, json_error("非法名称"));
    }
    let on = json_get(&text, "readonly")
        .and_then(|v| parse_switch(&v))
        .unwrap_or(false);
    if !state.db.set_file_readonly(&name, on) {
        return json_response(state, req, 404, json_error("文件不存在"));
    }
    json_response(
        state,
        req,
        200,
        format!("{{\"ok\":true,\"readonly\":{}}}", on).into_bytes(),
    )
}

fn admin_set_file_permanent(state: &AppState, mut req: Request) {
    let body = read_body_limited(&mut req, 4096).unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let name = json_get(&text, "name").unwrap_or_default();
    let valid = name.is_empty() || name_of(&state.cfg, &format!("/{}", name), false).is_some();
    if !valid {
        return json_response(state, req, 400, json_error("非法名称"));
    }
    let on = json_get(&text, "permanent")
        .and_then(|v| parse_switch(&v))
        .unwrap_or(false);
    let path = target(&state.cfg, &name);
    if fs::metadata(&path).is_err() {
        return json_response(state, req, 404, json_error("文件不存在"));
    }
    if let Err(e) = set_meta_permanent(&state.cfg, &name, on) {
        eprintln!("写入长期标记失败: {e}");
        return json_response(state, req, 500, json_error("保存失败"));
    }
    // 数据库标记尽力更新；下次同步会根据 meta 补齐。
    state.db.set_file_permanent(&name, on);
    json_response(
        state,
        req,
        200,
        format!("{{\"ok\":true,\"permanent\":{}}}", on).into_bytes(),
    )
}

fn admin_set_announcement(state: &AppState, mut req: Request) {
    let body = read_body_limited(&mut req, 16 * 1024).unwrap_or_default();
    let text = String::from_utf8_lossy(&body).into_owned();
    let announcement: String = json_get(&text, "text")
        .unwrap_or_default()
        .chars()
        .take(ANNOUNCEMENT_MAX_CHARS)
        .collect();
    if !state
        .db
        .set_setting(ANNOUNCEMENT_KEY, &announcement)
    {
        return json_response(state, req, 500, json_error("保存失败"));
    }
    json_response(state, req, 200, b"{\"ok\":true}".to_vec());
}

fn handle(state: Arc<AppState>, req: Request) {
    let method = req.method().clone();
    let (path, query) = split_url(req.url());

    let admin_path = state.cfg.admin_path.clone();
    if path == admin_path || path.starts_with(&format!("{}/", admin_path)) {
        return do_admin(&state, req, &method, path, query);
    }
    if let Some(rest) = path.strip_prefix("/d/") {
        let token = percent_decode(rest);
        if token.is_empty() || token.contains('/') {
            return respond(
                &state,
                req,
                404,
                b"not found\n".to_vec(),
                "text/plain; charset=utf-8",
                vec![],
            );
        }
        return do_share_download(&state, req, token);
    }

    match method {
        Method::Get | Method::Head => do_get(&state, req, path, query),
        Method::Post | Method::Put => do_write(&state, req, path, query),
        Method::Delete => do_delete(&state, req, path, query),
        _ => respond(
            &state,
            req,
            405,
            b"method not allowed\n".to_vec(),
            "text/plain; charset=utf-8",
            vec![],
        ),
    }
}

fn main() {
    let cfg = Config::from_env();
    let db = match Db::open(&cfg.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("数据库打开失败 {}: {e}", cfg.db_path.display());
            std::process::exit(1);
        }
    };
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let admin_path = cfg.admin_path.clone();
    let state = Arc::new(AppState {
        cfg,
        db,
        sessions: Mutex::new(HashMap::new()),
        login_fails: Mutex::new(HashMap::new()),
    });
    state.db.sync_files(&state.cfg);

    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("启动失败 {addr}: {e}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "剪贴板已启动: http://{}:{}/  文件 {}  有效期 {}{}",
        if state.cfg.host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &state.cfg.host
        },
        state.cfg.port,
        state.cfg.file.display(),
        human_ttl(state.cfg.ttl),
        if state.cfg.token.is_empty() {
            ""
        } else {
            "  已启用令牌"
        }
    );
    eprintln!("后台: {}  数据库: {}", admin_path, state.cfg.db_path.display());
    if !admin_enabled(&state) {
        eprintln!("警告: 未设置后台口令，后台当前公开访问；建议设置 CLIP_ADMIN_PASSWORD，或在后台页面自定义口令");
    } else if state.db.get_setting(ADMIN_PWD_KEY).is_some() {
        eprintln!("后台需登录（使用数据库中保存的自定义口令）");
    } else if std::env::var("CLIP_ADMIN_PASSWORD").is_ok() {
        eprintln!("后台需登录（口令取自 CLIP_ADMIN_PASSWORD）");
    } else {
        eprintln!("后台需登录（口令取自 CLIP_ADMIN_TOKEN / CLIP_TOKEN）");
    }

    loop {
        match server.recv() {
            Ok(req) => {
                let state = state.clone();
                std::thread::spawn(move || handle(state, req));
            }
            Err(e) => {
                eprintln!("连接错误: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_path_normalization() {
        assert_eq!(normalize_admin_path(""), "/admin");
        assert_eq!(normalize_admin_path("admin"), "/admin");
        assert_eq!(normalize_admin_path("/secret/"), "/secret");
        assert_eq!(normalize_admin_path("/a/b/"), "/a/b");
        // 不能占用根路径或内容/分享保留路径
        assert_eq!(normalize_admin_path("/"), "/admin");
        assert_eq!(normalize_admin_path("/raw"), "/admin");
        assert_eq!(normalize_admin_path("/d"), "/admin");
        assert_eq!(normalize_admin_path("/healthz"), "/admin");
    }

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("s3cret");
        assert!(hash.starts_with("pbkdf2$"));
        assert!(verify_password("s3cret", &hash));
        assert!(!verify_password("wrong", &hash));
        // 损坏/空散列必须拒绝
        assert!(!verify_password("x", "pbkdf2$1$salt$"));
        assert!(!verify_password("x", "not-a-hash"));
    }

    #[test]
    fn token_is_redacted_from_logs() {
        assert_eq!(redact_url("/x?token=abc&a=1"), "/x?token=***&a=1");
        assert_eq!(redact_url("/x?a=1&token=abc"), "/x?a=1&token=***");
        assert_eq!(redact_url("/x?tok%65n=abc"), "/x?tok%65n=***");
        assert_eq!(redact_url("/x"), "/x");
        assert_eq!(redact_url("/d/abc"), "/d/abc");
    }

    #[test]
    fn text_detection() {
        assert!(looks_like_text("hello".as_bytes()));
        assert!(looks_like_text("中文\n第二行".as_bytes()));
        assert!(!looks_like_text(&[0x00, 0x01, 0xff]));
        assert!(!looks_like_text(b"\x01\x02"));
    }

    #[test]
    fn hex_roundtrip() {
        let raw = [0u8, 1, 15, 16, 255];
        let s = hex_encode(&raw);
        assert_eq!(s, "00010f10ff");
        assert_eq!(hex_decode(&s).unwrap(), raw);
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn switch_parsing() {
        assert_eq!(parse_switch("1"), Some(true));
        assert_eq!(parse_switch("ON"), Some(true));
        assert_eq!(parse_switch("yes"), Some(true));
        assert_eq!(parse_switch("0"), Some(false));
        assert_eq!(parse_switch("off"), Some(false));
        assert_eq!(parse_switch("maybe"), None);
    }

    #[test]
    fn origin_building() {
        assert_eq!(
            build_origin(Some("https"), false, Some("copy.mhwy.work")),
            "https://copy.mhwy.work"
        );
        assert_eq!(
            build_origin(None, false, Some("127.0.0.1:28080")),
            "http://127.0.0.1:28080"
        );
        assert_eq!(
            build_origin(None, true, Some("example.com")),
            "https://example.com"
        );
        assert_eq!(
            build_origin(Some("https, http"), false, Some("a.com, b.com")),
            "https://a.com"
        );
        assert_eq!(build_origin(None, false, None), "http://localhost");
        assert_eq!(
            build_origin(Some("javascript"), false, Some("<x>")),
            "http://x"
        );
    }
}
