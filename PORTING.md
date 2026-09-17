# PORTING.md —— 从 Electron 外壳到 Tauri 2 外壳

这份文档记录**每一个坑的实际处理方式、实测结论、以及没做到的事**。
原则：**不静默降级**。凡是做不到的，这里都有一条写着「没做到」。

- 参考实现：`D:\variFlight_work\pi-web-desktop`（Electron 33 + electron-builder）
- 本实现：`D:\variFlight_work\pi-web-tauri`（Tauri 2.11 + Rust 1.98，WebView2 153）
- 实测平台：Windows 11 Pro 26200，x64；pi-web `0.9.0` → 自更新到 `0.9.1`

---

## 0. 架构决策：为什么是端口模式

Electron 版做到了**零端口**：FD 3/4 匿名管道 + `pi-app://` 特权协议，`protocol.handle()` 返回 `ReadableStream`。

**这条路在 WebView2 上走不通。** 自定义协议的响应体必须在 deferral 完成前**全部就绪**，无法流式返回（WebView2Feedback#3519、wry#1022 至今 open；Tauri 的协议 API 签名是 `Cow<'static, [u8]>`，类型上就没有流）。而 pi-web 有**永不结束**的 SSE 端点，前端用原生 `EventSource`：

- `/api/agent/[id]/events`（`Content-Type: text/event-stream`，`X-Accel-Buffering: no`）
- `/api/terminal/[id]/events`
- 文件监视

直译成自定义协议 = 对话框一个字都不出。

**结论**：整套 FD 3/4 管道层（`host-bridge.js` / `host-process.js` / `host-protocol.js` / `http-response-parser.js`，约 640 行）**不移植，直接丢弃**；改用 `127.0.0.1:<port>`。

**代价（诚实记录）**：机器上多了一个监听端口。Electron 版「完全不开放任何 127.0.0.1 端口」这条卖点**没有了**。补偿措施见坑 4。

---

## 坑 1 —— 端口必须稳定

**为什么**：`localStorage` 绑定在 `http://127.0.0.1:PORT` 这个 origin 上。pi-web 把主题（`pi-theme`）、未读会话、隐藏目录（`pi-hidden-cwds`）、kiosk、声音开关都存在里面。端口一变，用户偏好**全部重置** —— 而且这种 bug 报上来，第一反应是去查主题同步，不会想到端口。

**实现**（`src-tauri/src/server.rs::resolve_port`）：

1. 读 `%APPDATA%\Pi\port.json`，能 bind 上就复用；
2. 否则从 **30000–44999** 随机取一个空闲端口并写回。

两个刻意的选择：

- **端口池避开 49152–65535**。那是 Windows 的动态端口范围，系统会把里面的端口临时派给别的程序。选在范围外，"下次启动它还空着"的概率才高 —— 这正是持久化端口的意义。
- **不用 pi-web 的默认端口 30141**。固定的知名端口 = 敌意网页可以直接猜。首次随机 + 之后固定，两头都占。

**实测**：首启 `port: selected 35566 and persisted it`；之后每次启动都是 `port: reusing persisted 35566`，跨「自更新 + 服务重启」与跨「强杀进程」都保持不变。

`localStorage` 跨重启的直接验证（页面内探针写标记、重启后读回）：

```
第 1 次运行   LOCALSTORAGE previous=(none — first run on this origin)
第 2 次运行   LOCALSTORAGE previous=run@2026-09-17T02:45:35.272Z   ← 上一次运行写的
```

中间那次是被 `Stop-Process -Force` 强杀的，偏好照样活下来了。

**还有一条相关的失败路径也堵上了**：服务起来了但外壳登录失败时，早先的写法直接丢弃 `Server` 返回错误 —— 而 Rust 的 `Child` 没有 Drop 时 kill 的语义，那个 node 进程会继续占着端口，于是重试时 `resolve_port` 只能另选一个端口，**偏好照样被重置**。现在每条失败路径都显式 `server.stop()`。

---

## 坑 2 —— 远程 origin 的 `window.__TAURI__` 与注入面

**做了什么**：

- `src-tauri/capabilities/remote.json` 声明 `"remote": { "urls": ["http://127.0.0.1:*"] }`。**没有它，远程页面里的 `__TAURI_INTERNALS__.invoke` 全废**，目录选择器 / 主题同步 / 更新 toast 一起死。
- `build.rs` 用 `AppManifest::commands([...])` 声明那 4 个应用命令，`remote.json` 再逐个 `allow-*` 授权。`withGlobalTauri` 设为 `false`。

### 这里第一版做错了，而且错得很隐蔽

我原以为「应用命令不走 ACL，只要 `remote` 白名单放行 origin 就行」，于是把 `permissions` 写成了空数组。**错的**：

```
PICKER rejected -> select_directory not allowed. Plugin not found
```

Tauri 的规则是：**没有 app manifest 时，应用命令在本地页面放行、在远程 origin 一律拒绝**。而外壳自己的状态页是本地页面 —— 所以主题同步在启动页上「工作正常」，一进 pi-web 就全哑了，日志里也只有沉默。判据是 `tauri-utils` 的 `has_app_manifest()`（`APP_ACL_KEY = "__app-acl__"`）。

修法是在 `build.rs` 里声明命令，让 Tauri 生成 `allow-<命令名>`（下划线换成短横线）权限，再在 capability 里逐个列出。副作用正好是想要的：**现在连本地页面也必须被逐个授权**，而外壳自己的状态页一个命令都不需要，所以 `default.json` 里一个都没给。

**为什么权限清单只有这 4 个**：pi-web 页面里渲染的是**智能体产出的内容**，和我们的注入脚本在同一个世界。任何 ACL 能授予的能力，都等于顺手授给了那段内容。所以：

- **页面 → 外壳**：只有 `src-tauri/src/commands.rs` 里的 4 个应用命令。三个不收参数，第四个只收一个立刻收窄成 `dark|light` 的字符串。
- **外壳 → 页面**：全部走 `webview.eval()`，**不需要任何权限**。所以更新 toast、npm 进度行、会话 cookie 注入都没有在 capability 里留下任何口子。

**四个命令的最坏情况**：

| 命令 | 参数 | 被恶意调用时最坏会怎样 |
|---|---|---|
| `select_directory` | 无 | 弹一个用户必须自己点的原生目录对话框 |
| `reload_page` | 无 | 页面重载 |
| `theme_changed` | `dark`\|`light` | 标题栏变色 |
| `apply_update` | 无 | 从**固定 registry** 装**固定包** `@agegr/pi-web@latest`（包名、版本、registry 都在 Rust 侧写死） |

**实测**（都在远程 origin `http://127.0.0.1:35566` 的页面上）：

```
PAGE   origin=http://127.0.0.1:35566 tauri=true piDesktop=function cookie=true
theme-changed: page reported "light" -> Some(Light)          ← theme_changed 到达 Rust
select_directory: dialog callback fired (Some(Path("D:\\variFlight_work\\pi-web-tauri")))
PICKER resolved -> "D:\\variFlight_work\\pi-web-tauri"        ← 路径回到页面
TOAST  host=true cards=1 text=已是最新版本 ✕ pi-web 0.9.1 …    ← eval 方向的 toast
```

**没做到的**：Electron 的 `contextIsolation` 隔离世界**没有等价物**。这是真实的安全面扩大，不是可以靠配置补回来的。唯一的缓解就是上面那张表 —— 加新命令前必须先填这张表。

---

## 坑 3 —— `--no-open` / `PI_WEB_NO_OPEN=1`

`bin/pi-web.js` 在 next 打出 `Ready` 时会 `spawn(cmd.exe, ["/c","start","",url])` 弹系统浏览器。

**两个都设了**（`server.rs`）：命令行 `--no-open` + 环境变量 `PI_WEB_NO_OPEN=1`。任一单独生效即可，两个一起是为了防 pi-web 哪天不再认那个 flag。

**实测**：多次启动，没有浏览器窗口弹出。

---

## 坑 4 —— 口令怎么进 WebView2 ★ 结论与原计划不同

这是唯一一个**实测推翻了原设计**的地方。

### 原计划：顶层导航内嵌凭据

`http://pi:<pw>@127.0.0.1:<port>/`，靠 Chromium 缓存凭据，之后同源 XHR/EventSource 复用。

### 实测结果：**不可用，而且失败方式比预期严重**

在 WebView2 153 上实测（页面内探针，结果回传到一个本地收集器）：

```
origin=http://127.0.0.1:35566  href-has-creds=false
fetch /api/sessions THREW TypeError: Failed to execute 'fetch' on 'Window':
    Request cannot be constructed from a URL that includes credentials: /api/sessions
```

关键点：`location.href` 里看不到凭据了，**但文档的 base URL 里还在**。于是页面里每一个**相对路径** `fetch()` 在**构造阶段**就抛 `TypeError` —— 连请求都发不出去，不是 401。pi-web 的客户端代码全是相对路径请求，等于整个应用瘫痪。

所以这条路不是"降级可用"，是**直接不可用**。

### 实际采用：Rust 侧换 cookie，页面只拿到 token

pi-web **0.9.1 新增了 `POST /api/web-auth`**（0.9.0 没有），用口令换一个 HMAC 签名的 `pi_web_session` cookie。cookie 是外壳**能**安装的东西，于是：

1. 每次启动生成随机口令，只作为 `PI_WEB_PASSWORD` 传给子进程（**不落盘**）；
2. 服务就绪后，**Rust 直接在 loopback 上 `POST /api/web-auth`**（`server.rs::login`，一个几十行的裸 HTTP 请求，不引 HTTP 客户端依赖），拿回 `Set-Cookie`；
3. 导航到**不带凭据**的 `http://127.0.0.1:<port>/`；
4. 首次页面加载完成时 `eval` 一句 `document.cookie=...; location.reload()`（`app.rs::inject_session_cookie`，每个服务实例只做一次）。

**这样口令从头到尾没进过页面**，页面只拿到一个会话 token —— 而那个 token 本来就是它该持有的东西。外壳也**不复刻 token 的格式**，只是把服务器给的值原样回塞，所以 pi-web 改格式不会波及这里。

**实测**：

```
PAGE  origin=http://127.0.0.1:35566  tauri=true  piDesktop=function  cookie=true
API   /api/sessions -> 200
DOM   children=8  text=Pi 新建 选择项目… 模型 技能 设置 开始使用 …
```

即：cookie 生效、鉴权后的 API 返回 200、渲染的是**真正的 pi-web 界面**而不是登录页。

**SSE 也一并验证过**（同一套探针，拿一个真实会话去连 `/api/agent/<id>/events`）：

```
SSE open after 348ms
SSE msg#1 at +1788ms data={"type":"connected","sessionId":"01a0ad35-…","isStreaming":false
SSE after 8s: readyState=1 opened=true msgs=1   ← readyState 1 = 仍在流
```

三个点都成立：**连得上**（cookie 鉴权在 EventSource 上同样生效）、**消息是增量到达的**（open 之后 1.4 秒才来，不是流结束时一次性给）、**流一直开着**（8 秒后仍是 `readyState=1`）。这正是自定义协议方案做不到的那件事。

### 明确写出来的降级

- **pi-web < 0.9.1 的运行时：不设口令。** 那些版本只有 Basic Auth，而 WebView2 既没有 `BasicAuthenticationRequested` 钩子（wry 没暴露），内嵌凭据又会打死 `fetch`。此时端口**只靠 loopback 绑定 + pi-web middleware 的 Host 白名单**保护。判据是运行时里有没有 `.next/server/app/api/web-auth/route.js`（`server.rs::supports_cookie_auth`），日志里会打一行醒目的说明。
- **登录失败时自动降级为无口令并重启一次**（`app.rs`，`password_disabled` 闩锁只允许一次，不会循环）。理由：登录失败的话，pi-web 会向用户索要一个**只有外壳知道**的口令，用户无法自救；无口令至少还是个能用的应用。
- **残留风险**：注入 cookie 用的是 `eval`，理论上页面脚本可以提前 hook `document.cookie` 的 setter 把 token 偷走。但这发生在**首屏、智能体还没产出任何内容之前**，且拿到 token 的攻击者也只能访问 127.0.0.1 —— 与它已经在同源页面里所拥有的权限相比没有增量。记录在此，不假装不存在。
- **和 Electron 版比，这仍是安全面的净扩大**：零端口时代，别的浏览器根本无从访问；现在只要 pi-web ≥ 0.9.1 就有口令挡着，低于该版本则只有 loopback + Host 白名单。

---

## 坑 5 —— 进程树回收 ★ 拿到了 Job Object，无 `unsafe`

**做法**（`src-tauri/src/job.rs`）：用 **`win32job` 2.0.3**（安全封装，无需 `unsafe`）建一个带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 job，每个 spawn 出来的服务进程一出生就 `assign_process`。

```rust
let mut info = ExtendedLimitInfo::new();
info.limit_kill_on_job_close();
let job = Job::create_with_limit_info(&info)?;
// ...
job.assign_process(child.as_raw_handle() as isize)?;
```

`Child::as_raw_handle()` 是安全访问器，把句柄值交给安全封装也只是普通调用 —— **全程没有 `unsafe`**。

**孙进程也被覆盖**：`bin/pi-web.js` 会再 spawn 一个 next 子进程。job 成员派生出的子进程**默认自动加入同一个 job**（我们没有设 `BREAKAWAY_OK`），所以那个 next **孙进程**不用我们知道它的 pid 就已经在网里。

**为什么不用 `taskkill /T` 兜底就够**：`taskkill` 要我们的退出处理跑起来才行。用户在任务管理器「结束任务」时什么都不会跑。job 把保证交给了内核 —— 最后一个 job 句柄关闭（进程对象销毁时必然发生），内核直接终结所有成员。

**两者都保留**：正常退出仍然显式 `taskkill /pid <pid> /t /f`，因为换运行时要求文件句柄**立刻**释放、下次启动要求端口**立刻**可用；job 是崩溃/强杀时的兜底。

**实测**（先用 WMI 父子关系枚举整棵树，再强杀外壳，最后逐个 pid 查存活）：

杀之前，外壳底下挂着 **15 个进程、最深 6 层**：

```
pi-web-tauri.exe (26160)
├── msedgewebview2.exe ×6                    ← WebView2 自己的进程组
└── node.exe (31180)                         ← 我们 spawn 的 bin/pi-web.js
    ├── conhost.exe
    └── node.exe (8128)                      ← next 孙进程
        ├── node.exe (2956)
        └── uvx.exe → uv.exe → mcp-server-tapd.exe → python.exe → python.exe
                                              ← 智能体拉起来的 MCP server 链
```

用 `Stop-Process -Force`（`TerminateProcess`，等价于任务管理器「结束任务」，**不跑任何退出处理**）杀掉外壳后：

```
--- survivors AFTER ---
NONE — every descendant was reclaimed
```

包括六层之下的 MCP server 和它的 python 子进程。这比 Electron 版的 `taskkill /T` 严格 —— 后者根本不会在强杀场景下被执行。

---

## 坑 6 —— 安装与数据迁移

### userData：**刻意不换目录**

Tauri 默认会用 `%APPDATA%\com.agegr.piwebdesktop`（identifier 派生）。**没有采用**。`paths.rs::default_user_data_dir` 写死 `%APPDATA%\Pi` —— 和 Electron 版 `productName` 派生出来的是同一个目录。

这样一来：

- `theme-state.json` 原地就在，**格式也保持不变**（`{"theme":"dark"}`），老用户主题不重置；
- 只读安装时那份约 900MB 的运行时回退目录 `%APPDATA%\Pi\runtime` 原地就在，**一个字节都不用重新复制**。

WebView2 自己的 profile 仍在 Tauri 的默认位置 —— 那两个目录本来就没必要是同一个。

### 老 productName 的一次性搬运（`migrate.rs`）

- 状态文件（`theme-state.json`、`extensions-state.json`）从 `Pi Agent` / `Pi Dsh` / `Pi&Dsh` / `pi-web-desktop` 搬过来，**从不覆盖**已存在的文件，落一个 `.migrated-user-data` 标记防止把用户已删的文件"复活"。
- **运行时回退目录的收养**：若本版还没有 `%APPDATA%\Pi\runtime`，而某个老目录下有一份**带 `.seeded` 标记**的（没标记 = 上次复制被中断，不能要），就 `rename` 搬过来 —— 同卷 rename 是毫秒级，替代了一次一到两分钟的复制。

### 安装器：检测 Electron 旧版

electron-builder 把卸载记录写在 `HKCU\...\Uninstall\{0872d550-066b-5742-b9b4-8eea0f9e1aac}`（appId 的 UUID5），Tauri NSIS 用的是另一套键 —— **两个安装器互相看不见**，不处理就会并排装两份，并且抢同一个 `~/.pi/agent`。

`src-tauri/nsis/hooks.nsh` 的 `NSIS_HOOK_PREINSTALL` 读那个键，找到就**询问**用户是否先卸载（不是直接卸载 —— 悄悄卸载别的应用不是安装器该自己决定的事），静默安装（`/S`）时跳过。

### 调用静默安装的正确姿势（测试脚本踩过）

自动化安装测试当时是这么调的：

```powershell
Start-Process -FilePath $setup -ArgumentList "/S","/D=$target" -Wait   # ← 错的
```

PowerShell 会**给每个参数加引号**。证据是安装完成后注册表里的 `InstallLocation` 变成了**带引号的** `"D:\PiTauriTest"` —— 引号被当成路径的一部分写了进去。NSIS 对这两个开关的要求很具体：`/S` 必须是裸的，`/D` 必须放在最后且不加引号，否则行为不可预期（`/S` 可能不被识别，安装就不是静默的）。

修法：命令行整条交给 `cmd.exe` 拼，不经过 PowerShell 的参数引用。

顺带把安装器那侧也调保守了：`MessageBox` 加 `MB_DEFBUTTON2`，默认按钮改成「否」。卸载**别人的**应用不该是一次误触回车就能触发的事 —— 静默路径由 `IfSilent` 挡着，交互路径现在由默认按钮挡着。

---

## 坑 6.5 —— 打包版才会出现的 `\\?\` verbatim 路径 ★ dev 永远看不到

不在原始清单里，是安装实测逼出来的。

打包版里 Tauri 的 `resource_dir()` 返回的是 **verbatim 路径**：

```
paths: dev=false base=\\?\D:\PiTauriTest
```

而 **node 无法用这种路径解析主模块**。把 `\\?\D:\...\guard\guard-cli.js` 当脚本参数传过去，node 在跑第一行代码之前就死了：

```
Error: EISDIR: illegal operation on a directory, lstat 'D:'
    at resolveMainPath (node:internal/modules/run_main:39:23)
```

于是 guard 根本没运行，启动直接落到 error 页。

**`tauri dev` 永远暴露不了这个** —— dev 的 base 来自 `CARGO_MANIFEST_DIR`，是一条普通路径。所以在 dev 里跑得再顺，也不能替代一次真实安装。

**修法**（`paths.rs`）：在 `Paths::resolve` 里用 `dunce::simplified()` 把前缀剥掉一次，下游所有路径（guard CLI、`bin/pi-web.js`、`--runtime` / `--node-dir` 参数、子进程 cwd）自然都变成普通路径。用 `dunce` 而不是自己 `strip_prefix`，是因为它只在**去掉前缀后仍然可用**时才去掉 —— 真正超长的路径需要那个前缀，不能一刀切。

---

## 坑 7 —— WebView2 Runtime 依赖

`tauri.conf.json` 配 `webviewInstallMode: { type: "downloadBootstrapper", silent: true }`。

- Windows 11 与打了近年更新的 Windows 10：**自带**，bootstrapper 不会做任何事。
- 老 Win10：安装时静默下载安装，**需要联网**。
- 完全离线的老机器：README 里写明需预装 Evergreen Standalone Installer。

选 bootstrapper 而不是 `embedBootstrapper`/`offlineInstaller`，是因为后者会把安装包撑大约 150MB，而目标用户群基本都是 Win11。

---

## 坑 8 —— preload → initialization_script

`electron/preload.js`（497 行）几乎 1:1 搬到 `src-tauri/inject/preload.js`：更新结果 toast、notice shelf 可读性修复、底部重载条 + viewport reserve、主题同步、品牌改名，逻辑与样式全部保留。

替换掉的只有三处通道：

| Electron | Tauri |
|---|---|
| `contextBridge.exposeInMainWorld` | 直接 `window.X =`（主世界，没有隔离） |
| `ipcRenderer.invoke/send` | `window.__TAURI_INTERNALS__.invoke` |
| `ipcRenderer.on("update-notice")` | `window.__piwdNotice()`，由 Rust 侧 `eval` 调用 |

`window.piDesktop.selectDirectory` 是上游 pi-web 0.7.13+ 认的桌面桥，**必须在页面脚本之前注入** —— Tauri 的 initialization script 就是 document-start 执行，这点天然满足。实测页面上 `piDesktop=function`。

### 这里踩了一个真的坑，值得记下来

Electron 原版用 `location.protocol !== "http:"` 区分"这是 pi-web 页面还是外壳自己的状态页"。Tauri 下外壳自己的页面**也是 http**，这个判据失效了。

第一版我改成判 `location.hostname === "127.0.0.1"` —— **错的**：`tauri dev` 把前端资源伺服在 `http://127.0.0.1:1430/`（打包后才是 `http://tauri.localhost/`）。结果启动页被当成 pi-web 页面，主题同步把用户存的 `dark` **覆写成了 `light`**。表现只有"主题莫名其妙被重置"，非常难查。

最终方案：外壳自己的四个页面都带 `<meta name="piwd-shell">`，注入脚本按**这个标记**判断，与 origin 无关，dev 和打包后行为一致。代价是这个判断必须等 `<head>` 解析完，所以所有相关逻辑统一挂在 `DOMContentLoaded` 之后（`onPiWeb()`）。

---

## 铁律的落实情况

| 铁律 | 状态 |
|---|---|
| 不 fork / 不修改 pi-web 与 pi-coding-agent | ✅ `runtime-seed` 只是 npm 生产安装；仓库里没有它们的源码，也没有任何补丁 |
| Rust 禁止 `unsafe` | ✅ `grep -rn "unsafe" src-tauri/src` 为空（含 Job Object 实现） |
| 不重写 `runtime-guard.js` / `updater.js` | ✅ 逐字节复制（md5 与参考实现一致），回归测试一并搬来 |
| `appId` 与数据目录不变 | ✅ `com.agegr.piwebdesktop` / `~/.pi/agent` |

### 铁律 3 的落地细节

两个文件放在 `src-tauri/resources/guard/`，另加**唯一一个新文件** `guard-cli.js` —— 它复刻的是 `main.js` 里的 `guardCtx()`（注入 `dbg` 和绑定到内置 npm 的 `installInto`），对外只暴露 5 个子命令，一行 JSON 进、一行 JSON 出：

```
recover | verify | installed | latest | provision
```

Rust 侧（`guard.rs`）只做三件事：拼命令行、解析那一行 JSON、把 npm 的 stderr 转成进度行。**任何看起来像完整性判断的代码出现在 `guard.rs` 里都是 bug。**

**一处行为差异（刻意，且是保守方向）**：Electron 版里 `provisionRuntime` 通过回调 `stopServer` 停服务，所以**下载期间旧服务照常运行**，只在换入前的几百毫秒停。跨进程调用是单向的，做不到回调，所以 Rust 侧**在调用 provision 之前就停掉服务**。代价是更新期间的离线窗口从"几百毫秒"变成"整个下载过程"（更新页会显示 npm 进度）。**换来的是零复杂度和零正确性风险** —— guard 自身的逻辑一行没改。

---

## 其它与 Electron 版的差异（逐条）

| 项 | Electron | Tauri | 说明 |
|---|---|---|---|
| `autoHideMenuBar` | 菜单默认隐藏，按 Alt 唤出 | **菜单常驻可见** | Tauri 有 `hide_menu()/show_menu()`，但没有能把 Alt 按键接回来的途径。**未达成**，纯观感差异。菜单加了 `&` 助记符（`Alt+A` / `Alt+E` / `Alt+V`），Electron 版原本没有 |
| `reloadIgnoringCache()` | 重载并绕过 HTTP 缓存 | **普通重载** | Tauri 没有绕缓存的变体。**未完全达成**：普通重载仍然会重新校验顶层文档（Chromium 用户重载会发 `Cache-Control: max-age=0`），且 Next.js 的静态资源走内容哈希 URL，所以缺口很窄 —— 但它确实不是原来的语义 |
| `app.setAppUserModelId()` | 显式调用 | 依赖打包身份 | Tauri 未暴露安全绑定，Win32 调用需要 `unsafe`。打包版由 NSIS 快捷方式与 bundle identity 保证；**仅 `tauri dev` 下任务栏分组身份缺失**，不影响发布产物 |
| 视图菜单的缩放项 | `resetZoom`/`zoomIn`/`zoomOut` | 只留全屏 | Tauri 的 `PredefinedMenuItem` 没有缩放项；页面内 Ctrl+ +/- 仍可用 |
| 服务崩溃弹窗 | `dialog.showErrorBox` | `tauri-plugin-dialog` | 行为一致（退出/重启/更新导致的停止一律不弹） |
| 调试日志 | `%TEMP%\pi-web-desktop-debug.log` | **同一个文件** | `guard-cli.js` 以 `[guard]` 前缀写同一份，一条时间线看完整个启动 |
| `ensureBridgeOnDisk()` | 从 asar 解出桥接文件 | **已删除** | Tauri 没有 asar，且管道层整体丢弃 |
| 首帧背景色 | `backgroundColor: "#0A0A0A"` | `background_color(...)` | 一致，避免白闪 |

---

## 验收状态

| # | 验收项 | 状态 |
|---|---|---|
| 1 | `tauri dev` 能起来，pi-web 正常加载 | ✅ 页面渲染的是真实 pi-web 界面（「新建 / 选择项目… / 模型 / 技能 / 设置」），鉴权后的 `/api/sessions` 返回 200，会话列表读到 14 个已有会话。**「能对话」需要模型 API Key，本机没有配，这一条见下方「未完成的验证」** |
| 2 | SSE 流式输出正常 | ✅ 真实会话的 `/api/agent/<id>/events`：348ms 打开，+1788ms 收到第一条消息（**增量到达**，不是流结束时一次性给），8 秒后 `readyState=1` **仍在流**。终端面板走同一套 SSE 端点 |
| 3 | 关窗 / **强杀外壳**后 node 与 next 子孙进程全部消失 | ✅ 15 个进程、6 层深的整棵树（含 MCP server 与其 python 子进程），`TerminateProcess` 后**无一存活** |
| 4 | 重启后主题 / 未读 / 隐藏目录等偏好保持不变 | ✅ `localStorage` 标记跨「强杀 + 重启」读回；端口全程复用 35566 |
| 5 | 目录选择器弹原生对话框并回传路径 | ✅ 取消分支返回 `null`，确定分支返回 `"D:\\variFlight_work\\pi-web-tauri"`，两条都端到端到达页面 |
| 6 | 「检查更新」走通：查版本 → 安装 → 服务重启 → 页面刷新 | ✅ 自动静默更新端到端：`0.9.0 → 0.9.1`，staging 安装 → 校验 → 原子换入 → 复用同一端口重启 → 页面刷新。结果 toast 也实测渲染（`已是最新版本 · pi-web 0.9.1 · pi-coding-agent 0.85.1`）。**交互式菜单项的确认弹窗未点过，见下方** |
| 7 | 能打出 NSIS 安装包并在干净目录安装运行 | ✅ `Pi_0.4.0_x64-setup.exe`（86MB）静默装到干净目录 `D:\PiTauriTest`，装出 30,976 文件 / 521.6MB；启动后 guard 校验通过（6 个原生模块 0 失败）、复用已有端口 35566、服务起来、cookie 鉴权成功、页面侧注入脚本工作。装好的这份也做了强杀测试：**21 个子孙进程全部回收** |
| 8 | 全程无 `unsafe` | ✅ `grep -rn "unsafe" src-tauri/src` 为空 |

附带：`npm run test:guard`（从 Electron 版原样搬来的运行时守卫回归测试）**69 passed, 0 failed**。

## 未完成的验证

明确列出来，不当作通过：

1. **真实对话的逐字输出**（验收 1 的后半句）。本机 `~/.pi/agent` 里没有配模型提供商的 API Key，没法真的发一轮对话。已验证的是它下面那层：SSE 端点连得上、消息增量到达、流持续打开 —— 也就是 Electron 版必须靠 FD 3/4 管道才能做到、而 WebView2 自定义协议做不到的那件事。补验只需在应用里配一个 Key 然后发一句话。
2. **交互式「检查更新…」菜单项**。菜单是原生菜单栏，脚本点不动（`SetForegroundWindow` 在后台进程里会被前台锁静默拒绝）。已验证的是它调用的整条链路：`check_for_updates` → `guard.latest()` → `notify_update` → `eval` → toast 渲染（自动检查走的是同一条，只差一个确认弹窗）。
3. **只读目录安装的首启复制路径**（装到 `C:\Program Files` 时 robocopy 种子到 `%APPDATA%\Pi\runtime`）。默认安装目录是可写的 `%LOCALAPPDATA%\Pi`，没有走到这条分支。代码与 Electron 版同构，但**没有实测过**。
4. **老版本迁移的实机验证**。本机 `%APPDATA%\Pi\.migrated-user-data` 是 Electron 版早就写下的，所以状态文件迁移正确地**跳过**了；`adopt_legacy_runtime()` 也因为本机是就地运行而没有触发。两条路径都没有在真实的「老用户升级」场景下跑过。
