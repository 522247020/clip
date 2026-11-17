# clip

极简网页剪贴板：**一个地址，一块内容**。文本可编辑、图片可预览、二进制可下载。
Rust 单二进制实现（`tiny_http` + `rusqlite`），自带后台、SQLite 记录与亮/暗主题。

- **前台**：根路径一块内容，浏览器看网页、`curl` 拿原文。
- **后台**：可自定义路径（`CLIP_ADMIN_PATH`）+ 登录口令，管理文件、长期下载链接与访问记录。
- **数据库**：SQLite 记录文件元信息、长期链接、访问日志、后台口令。
- **长期下载**：为文件生成 `/d/<token>` 快照链接，可设有效期与次数上限。
- **阅后即焚**：可让内容被读取一次后自动清空（`?once=1` 或 `CLIP_ONCE=1`）。
- **主题**：右上角一键切换亮色 / 暗色，记忆在浏览器本地。
- **单文件**：HTML/CSS/JS 与数据库都用 `include_str!` 编进一个可执行文件。

当前版本：`Clip/5.1`。

---

## 构建

```bash
cargo build --release
# 产物：target/release/clip
```

## 运行

```bash
./target/release/clip

# 自定义端口、内容令牌、后台路径与口令：
CLIP_PORT=80 CLIP_TTL=86400 \
CLIP_TOKEN=mysecret \
CLIP_ADMIN_PATH=/secret-panel \
CLIP_ADMIN_PASSWORD=adminsecret \
./target/release/clip
```

- 前台：`http://HOST/`
- 后台：`http://HOST/secret-panel`（默认 `/admin`）

---

## 用法

```bash
# 文本：保存（覆盖）/ 取回
curl -d "要传的内容" https://HOST/
curl -sL https://HOST/

# 具名槽位：上传 / 取回
curl -T ./photo.png https://HOST/photo.png
curl -sL https://HOST/photo.png

# 下载 / 清空
curl -OJ "https://HOST/?download=1"
curl -X DELETE https://HOST/

# 阅后即焚：读取一次后自动清空
curl -d "一次性内容" "https://HOST/?once=1"
curl -T ./file -H 'X-Clip-Once: 1' https://HOST/file
```

浏览器打开同一地址即可：

- **文本**：编辑框内直接改，`Ctrl / ⌘ + Enter` 保存。
- **图片**（`image/*`）：直接显示预览。
- **其它二进制**：显示文件卡片和「下载」按钮。
- **拖拽**：文本文件读入编辑框，图片 / 二进制直接上传。
- **阅后即焚**：勾选编辑器下方的「阅后即焚」再保存 / 上传；上传后**不会自动打开**，点「打开（仅一次）」查看，读取一次后自动清空。
- **主题**：右上角 `☀ / ☾` 切换亮色 / 暗色。

> `curl` 和浏览器访问同一地址，靠 `Accept` 头自动区分返回纯文本还是网页。

---

## 后台

后台路径由 `CLIP_ADMIN_PATH` 指定（默认 `/admin`）。

### 登录与口令

- 设置 `CLIP_ADMIN_PASSWORD`（或作为回退的 `CLIP_ADMIN_TOKEN` / `CLIP_TOKEN`）后，
  打开后台只会显示登录表单。
- 登录成功写入 `HttpOnly; SameSite=Lax` 会话 Cookie（有效期 7 天），
  HTTPS 环境自动加 `Secure`。
- 后台**仅**通过登录会话鉴权，不接受令牌头 / 查询串绕过。
- 「退出登录」清 Cookie 并回到登录页。

**修改口令（两种方式）**

1. **后台页面**：最下方「后台口令」表单，填当前口令 + 新口令 + 确认即可。
   新口令以 PBKDF2-SHA256 加盐散列存于数据库 `settings` 表，非明文；修改后旧会话全部失效。
2. **环境变量**：修改 `CLIP_ADMIN_PASSWORD` 并重启。

忘记自定义口令时：环境变量口令仍可登录（找回用）；或删除 `settings` 表中 `admin_pwd` 记录重置。

> 若三者（`CLIP_ADMIN_PASSWORD` / `CLIP_ADMIN_TOKEN` / `CLIP_TOKEN`）全为空，
> 后台将公开可访问，启动时会打印警告，后台页面也会提示。

### 文件

列出所有内容（名称、类型、大小、更新时间、下载次数），可打开或删除。

### 长期下载

选择一个文件创建 `/d/<token>` 链接，可设有效期（默认永久）与下载次数上限，
可复制或撤销。链接指向创建时的**快照**，不受原文件 TTL 影响；过期或超限返回 `410`。

### 访问记录

记录时间、IP、方法、路径、状态、大小与 User-Agent，可刷新 / 清空。
所有确认操作（删除、清空）使用全屏遮罩对话框。

---

## 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `CLIP_HOST` | `0.0.0.0` | 监听地址 |
| `CLIP_PORT` | `8080` | 监听端口 |
| `CLIP_FILE` | `./clip.txt` | 根路径内容文件 |
| `CLIP_STORE` | `clip.txt.files` | `/name` 内容目录 |
| `CLIP_TTL` | `86400` | 有效期秒数，默认 1 天；`0` = 不过期 |
| `CLIP_MAX_BYTES` | `2097152` | 文本上限（2MB） |
| `CLIP_MAX_BINARY` | `10485760` | 二进制上限（10MB） |
| `CLIP_ONCE` | `0` | 默认阅后即焚；`1` 时新内容读取一次后清空 |
| `CLIP_TOKEN` | 空 | 内容访问令牌，留空则公开；需 `X-Clip-Token` 头或 `?token=` |
| `CLIP_ADMIN_PATH` | `/admin` | 后台路径 |
| `CLIP_ADMIN_PASSWORD` | 取 `CLIP_ADMIN_TOKEN` | 后台登录口令 |
| `CLIP_ADMIN_TOKEN` | 取 `CLIP_TOKEN` | 后台口令回退（不作为令牌绕过） |
| `CLIP_DB` | `<CLIP_STORE>/.meta/clip.db` | SQLite 数据库路径 |
| `CLIP_SHARE_DIR` | `<CLIP_STORE>/.shares` | 长期下载快照目录 |

---

## 接口

后台接口路径前缀为 `CLIP_ADMIN_PATH`（下表以 `/admin` 为例）。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/` | 浏览器→网页；curl→原文 |
| `GET` | `/raw` | 强制原始内容；`?download=1` 强制下载 |
| `GET` | `/<name>` | 具名内容；图片内联、二进制附件、文本渲染网页 |
| `POST` / `PUT` | `/` 或 `/<name>` | 保存（请求体即内容） |
| `DELETE` | `/` 或 `/<name>` | 清空 |
| `GET` | `/d/<token>` | 长期下载 |
| `GET` | `/healthz` | 健康检查 |
| `GET` | `/admin` | 后台页面（未登录显示登录表单） |
| `POST` | `/admin/login` | 登录（表单 `password=...`，成功 303） |
| `GET` | `/admin/logout` | 退出登录 |
| `GET` | `/admin/api/files` | 文件列表 |
| `DELETE` | `/admin/api/files?name=<name>` | 删除文件（`name` 省略即根内容） |
| `GET` | `/admin/api/shares` | 长期链接列表 |
| `POST` | `/admin/api/shares` | 创建链接（JSON：`name`、`expires_in`、`max_downloads`、`note`） |
| `DELETE` | `/admin/api/shares?token=<token>` | 撤销链接 |
| `GET` | `/admin/api/logs?limit=` | 访问记录 |
| `DELETE` | `/admin/api/logs` | 清空访问记录 |
| `POST` | `/admin/api/password` | 修改后台口令（JSON：`current`、`new`） |

- 保存接口支持 `Content-Type: application/json`，取其中的 `content` 字段。
- 上传时可用 `X-Clip-Filename` 头（percent 编码，避免非 ASCII 原始头）指定原始文件名。

---

## 存储

| 位置 | 内容 |
| --- | --- |
| `CLIP_FILE` | 根路径内容（纯文本文件，可直接 `cat` / 手工编辑） |
| `CLIP_STORE/<name>` | 具名内容 |
| `CLIP_STORE/.meta/<name>.json` | 元数据：`Content-Type`、原始文件名、`once`（阅后即焚） |
| `CLIP_DB` | SQLite：`files` / `shares` / `access_log` / `settings` 四张表 |
| `CLIP_SHARE_DIR/<token>` | 长期下载快照 |

启动及打开后台时，会把数据库与磁盘文件对齐（补齐新增、清理已删除）。

---

## 项目结构

```
Cargo.toml           # 依赖：tiny_http, rusqlite(bundled), sha2, pbkdf2
src/main.rs          # 全部后端逻辑
assets/style.css     # 前台 + 后台 + 登录页样式（含亮/暗主题）
assets/admin.css     # 后台专用样式（表格、表单、模态框）
assets/home.html     # 前台页面
assets/admin.html    # 后台页面
assets/login.html    # 登录页
deploy/clip.service  # systemd 服务示例
deploy/clip.env.example  # 环境变量示例
README.md
```

## 部署到 Linux 服务器（替换 Python 版）

推荐构建**静态 musl** 二进制，不依赖服务器 glibc，旧版 CentOS / Alibaba Cloud Linux 也能直接跑：

```bash
# 本机用 Docker 构建（需要 Docker）
docker run --rm \
  -v "$PWD":/src \
  -v "$HOME/.cargo/config.toml":/usr/local/cargo/config.toml:ro \
  -w /src rust:1.90-alpine \
  sh -c "apk add --no-cache musl-dev && cargo build --release --target x86_64-unknown-linux-musl"

# 产物
target/x86_64-unknown-linux-musl/release/clip
file target/x86_64-unknown-linux-musl/release/clip   # 应为 statically linked
```

上传并安装：

```bash
scp clip root@SERVER:/opt/clip/clip
ssh root@SERVER

chmod +x /opt/clip/clip
groupadd -r clip 2>/dev/null || true
useradd -r -g clip -d /opt/clip -s /usr/sbin/nologin clip 2>/dev/null || true
mkdir -p /opt/clip/data /etc/clip
chown -R clip:clip /opt/clip

cp deploy/clip.env.example /etc/clip/clip.env
chmod 600 /etc/clip/clip.env
vi /etc/clip/clip.env          # 至少修改 CLIP_ADMIN_PASSWORD

cp deploy/clip.service /etc/systemd/system/clip.service
systemctl daemon-reload
systemctl enable --now clip
systemctl status clip --no-pager
```

**从旧 Python 版迁移**：把 `CLIP_FILE` / `CLIP_STORE` 指向旧数据路径即可
（两边都是 `clip.txt` + `clip.txt.files` 布局），首次启动会自动扫描旧文件并写入 SQLite。
先停掉旧 Python 进程，再用相同端口启动 `clip`。

> 静态 musl 二进制不依赖服务器 glibc；若服务器是 arm64，请改用对应 target 重新构建。

## 安全建议

- 至少设置一个后台口令，并在反向代理后启用 HTTPS（Cookie 会自动加 `Secure`）。
- 需要限制内容访问时设置 `CLIP_TOKEN`。
- 定期在后台清理访问记录与不再需要的长期链接。
