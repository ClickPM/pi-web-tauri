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
- `build.rs` 用 `AppManifest::commands([...])` 声明那 3 个应用命令，`remote.json` 再逐个 `allow-*` 授权。`withGlobalTauri` 设为 `false`。

### 这里第一版做错了，而且错得很隐蔽

我原以为「应用命令不走 ACL，只要 `remote` 白名单放行 origin 就行」，于是把 `permissions` 写成了空数组。**错的**：

```
PICKER rejected -> select_directory not allowed. Plugin not found
```

Tauri 的规则是：**没有 app manifest 时，应用命令在本地页面放行、在远程 origin 一律拒绝**。而外壳自己的状态页是本地页面 —— 所以主题同步在启动页上「工作正常」，一进 pi-web 就全哑了，日志里也只有沉默。判据是 `tauri-utils` 的 `has_app_manifest()`（`APP_ACL_KEY = "__app-acl__"`）。

修法是在 `build.rs` 里声明命令，让 Tauri 生成 `allow-<命令名>`（下划线换成短横线）权限，再在 capability 里逐个列出。副作用正好是想要的：**现在连本地页面也必须被逐个授权**，而外壳自己的状态页一个命令都不需要，所以 `default.json` 里一个都没给。

**为什么权限清单只有这 3 个**：pi-web 页面里渲染的是**智能体产出的内容**，和我们的注入脚本在同一个世界。任何 ACL 能授予的能力，都等于顺手授给了那段内容。所以：

- **页面 → 外壳**：只有 `src-tauri/src/commands.rs` 里的 3 个应用命令。两个不收参数，第三个只收一个立刻收窄成 `dark|light` 的字符串。
- **外壳 → 页面**：全部走 `webview.eval()`，**不需要任何权限**。所以更新 toast、npm 进度行、会话 cookie 注入都没有在 capability 里留下任何口子。

**三个命令的最坏情况**：

| 命令 | 参数 | 被恶意调用时最坏会怎样 |
|---|---|---|
| `select_directory` | 无 | 弹一个用户必须自己点的原生目录对话框 |
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

`electron/preload.js`（497 行）几乎 1:1 搬到 `src-tauri/inject/preload.js`：更新结果 toast、notice shelf 可读性修复、主题同步、品牌改名，逻辑与样式全部保留。**唯一删掉的是底部重载条 + 那 30px viewport reserve** —— 按所有者要求退役，重载改成菜单栏上的「刷新」按钮（见下方 `autoHideMenuBar` 一行）。

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
| `autoHideMenuBar` | 菜单默认隐藏，按 Alt 唤出 | **菜单常驻可见** | **写出来了，实测后撤掉，见下条**。菜单加了 `&` 助记符（`Alt+A` / `Alt+E` / `Alt+V`），Electron 版原本没有；菜单栏末尾多了一个顶级「刷新(&R)」按钮，接管原来页面底部那条重载条 |
| `reloadIgnoringCache()` | 重载并绕过 HTTP 缓存 | **普通重载** | Tauri 没有绕缓存的变体。**未完全达成**：普通重载仍然会重新校验顶层文档（Chromium 用户重载会发 `Cache-Control: max-age=0`），且 Next.js 的静态资源走内容哈希 URL，所以缺口很窄 —— 但它确实不是原来的语义 |
| `app.setAppUserModelId()` | 显式调用 | 依赖打包身份 | Tauri 未暴露安全绑定，Win32 调用需要 `unsafe`。打包版由 NSIS 快捷方式与 bundle identity 保证；**仅 `tauri dev` 下任务栏分组身份缺失**，不影响发布产物 |
| 视图菜单的缩放项 | `resetZoom`/`zoomIn`/`zoomOut` | 只留全屏 | Tauri 的 `PredefinedMenuItem` 没有缩放项；页面内 Ctrl+ +/- 仍可用 |
| 服务崩溃弹窗 | `dialog.showErrorBox` | `tauri-plugin-dialog` | 行为一致（退出/重启/更新导致的停止一律不弹） |
| 调试日志 | `%TEMP%\pi-web-desktop-debug.log` | **同一个文件** | `guard-cli.js` 以 `[guard]` 前缀写同一份，一条时间线看完整个启动 |
| `ensureBridgeOnDisk()` | 从 asar 解出桥接文件 | **已删除** | Tauri 没有 asar，且管道层整体丢弃 |
| 首帧背景色 | `backgroundColor: "#0A0A0A"` | `background_color(...)` | 一致，避免白闪 |

### 关于 `autoHideMenuBar`：为什么最后是常显

这条做过完整实现，也实测过，最后由所有者决定撤回。记在这里是因为**中间踩到的两件事对以后动菜单的人有用**，且撤回不是"没做"而是"做了不成立"。

实现过的版本：Rust 侧 `window.hide_menu()` 起手隐藏 + 一个 `menu_bar(show: bool)` 命令；注入脚本监听裸 Alt 的 keyup 调 `show: true`，点回内容区调 `show: false`。

两个非显而易见的坑：

1. **`hide_menu()` 必须在 `set_menu()` 之后调**。反过来写不报错 —— `set_menu` 会把菜单重新挂到每个窗口上，先前的隐藏被静默撤销。判据只能从 Win32 侧看：`GetMenu(hwnd)` 非 NULL。
2. **菜单栏出现时 webview 被 resize，光标底下随即收到一个 mousedown**（实测 6–7ms 后到），正好被"点内容区收起"那条逻辑吃掉 —— 日志里是 `menu bar shown` 紧跟 `menu bar hidden`，表现出来是"按 Alt 什么都没发生"。加了 400ms 窗口 + `detail === 0` 合成事件过滤才稳。同一轮还把命令从 toggle 改成显式 `show: bool`，因为 toggle 不幂等，而 Alt 语义上只可能是"显示"。

**撤回的原因**：Alt 的 keyup 要进到页面，取决于 WebView2 那个子窗口当时有没有键盘焦点。这条链路我没有可靠办法验证（合成 Alt 四次只进去三次），而所有者实际用下来判定"Alt 这条路有问题"。既然菜单是**唯一**能到「检查更新 / 重启内嵌服务 / 开发者工具 / 刷新」的入口，一个不保证能唤出来的入口不可接受 —— 所以整条撤掉，菜单栏常显，`menu_bar` 命令一并删除（页面 → 外壳的命令面因此从 4 个回到 3 个）。

**与 Electron 的差异因此是确定的**：菜单栏常占顶部一行，这是纯观感差异，功能无损。


---

## 多主机切换（Host Switcher）—— 架构决策

同一个窗口在 **Windows 本地（内嵌服务）** 与局域网里的 **Mac mini / Arch Linux (130)** 之间切换。实现：`hosts.rs`（配置）、`remote.rs`（隧道会话）、`app.rs` 的 `ActiveBackend` 状态机、`menu.rs` 的「主机(&H)」。

### 为什么走外壳托管的 SSH 本地端口转发，而不是直连局域网 IP

- **ACL 一个字都不用改**：`capabilities/remote.json` 仍是 `http://127.0.0.1:*` + 那 3 个命令。放行 `http://192.168.x.x:*` 等于把命令面授给局域网里任何能被导航到的明文页面。
- **明文不上网线**：远端 pi-web 只监听它自己的 `127.0.0.1:30141`，口令和 cookie 只在 SSH 加密信道里走。
- **口令不进页面**：外壳在 `127.0.0.1:<localPort>` 上 `POST /api/web-auth` 换 cookie（复用 `server::web_auth_login`），与本地模式完全同构。

**实测**（Windows 内嵌的 pi-web 0.9.1 经 `-R` 反向挂到 130 的 `127.0.0.1:30141` 模拟远端，再走与外壳完全相同的 `-L 127.0.0.1:45002:127.0.0.1:30141`）：

```
GET / via 45002            -> 307 Location=/login     ← 视为就绪且需要认证
POST /api/web-auth         -> 200 cookie=pi_web_session=v1.…
GET /api/sessions + cookie -> 200
GET /api/sessions no cookie-> 401
POST 错误口令              -> 401
```

pi-web middleware 的 Host 白名单对 `127.0.0.1:45002` 放行（它对 IP 字面量一律放行，不比端口），所以隧道端口与远端监听端口不同**不需要** `PI_WEB_ALLOWED_HOSTS`。

### 端口分段：`30000..=44999` 本地，`45000..=48999` 远端

1. **每台远端一个固定端口 = 一个独立 origin = 独立持久的 `localStorage`**（坑 1 的逻辑，按主机复制一份）。主题、隐藏目录、未读标记不会在主机之间串，也不会因端口漂移被重置。
2. **低于 49152**：不落进 Windows 动态端口范围，下次启动大概率还空着。
3. **与本地池不重叠**：`preload.js` 在 document-start **同步**执行，只凭 `Number(location.port)` 就能知道自己在哪台机器上（见下）。
4. 启动隧道前先探测端口；被占用直接报「端口冲突」，不去猜另一个端口（那会静默换 origin）。

`hosts.json` 中 `localPort` 不在 45000–48999、与他人冲突、`id` 重复或为 `local`、`sshTarget` 以 `-` 开头（防 `-oProxyCommand=` 注入，`ssh_args` 另加 `--` 双保险）的条目逐条跳过并记日志，**不影响启动**。写回 `active` 时只替换顶层那一个值（字节级保留用户的其余内容、缩进、换行、BOM），临时文件 + rename 原子替换，且写前自检能否解析。

### 为什么远端屏蔽 `window.piDesktop`

pi-web 的侧边栏挂载时就探测 `window.piDesktop.selectDirectory`。远端模式下若仍暴露它，点击会弹出 **Windows 本机**的文件夹对话框并返回 `D:\...`，远端 macOS/Linux 上的智能体随即打不开这个路径。注入脚本没有「稍后再撤销」的机会（document-start 同步、挂载即探测），所以只能按端口决定：本地段挂、远端段不挂。不挂时 pi-web 自动回退到网页内目录浏览器 —— 它通过 API 列的正是**远端**文件系统，这才是对的行为。`theme_changed` 保留，原生标题栏跟随远端页面的亮暗色。

顺带：`apply_update` 命令在远端页面上也能被调用（ACL 按 origin 不按端口），Rust 侧在非本地目标时直接拒绝；「检查更新…」菜单项在远端禁用。

### SSH 子进程：System32 OpenSSH + Job Object

- **只用** `C:\Windows\System32\OpenSSH\ssh.exe`（不存在才回退 PATH，且跳过 `\Git\`、`msys`、`mingw`、`cygwin`、`\usr\bin\` 下的 ssh）。MSYS 系 ssh 用自己的 HOME 仿真解析 `~/.ssh`，config / 密钥 / known_hosts 全失效，`ssh 130` 甚至会被当成 `0.0.0.130`。
- 参数：`-N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ConnectTimeout=8 -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -L 127.0.0.1:<local>:<remoteHost>:<remotePort> [-p N] -- <target>`。`BatchMode` 意味着**必须**是密钥免密 + 已信任指纹，外壳不会弹任何交互。
- `.no_console()` + spawn 后立刻 `job.assign()`：外壳被任务管理器强杀时，内核连坐回收 `ssh.exe`，不留孤儿占端口。正常切换/退出时显式 kill 并等待端口释放（`stop()` 最多等 3 秒）。
- stderr 收进 20,000 字节的环形缓冲；ssh 在就绪前退出则立即中止 12 秒的轮询并把 stderr + 按关键字归类的排障提示（认证失败 / 指纹 / 解析失败 / 超时 / 远端无服务 / 端口占用）显示在 `error.html`。隧道中途断开同样走 `error.html`，但仅当该主机仍是当前目标且不是主动切换/退出（会话序号 `serial` 防止旧隧道迟到的退出事件误报）。

### 状态机要点（`app.rs`）

- `ActiveBackend::Local(Arc<Server>) | Remote(Arc<RemoteSession>)`；`is_internal_url`、`inject_session_cookie`、`show_pi_web` 都只认**当前激活**后端。别的主机的隧道端口在 `on_navigation` 里算外部链接。
- `capture_local_base` 按**端口段**排除（30000–48999 一律不是外壳状态页），而不是只比对当前后端 —— 切换途中上一台主机的页面可能在状态已指向别处之后才加载完。
- **cookie 是按主机名而不是按端口隔离的**：本地服务与所有隧道都是 `127.0.0.1`，共用同一个 `pi_web_session` 槽位，后登录的会覆盖前一个。所以「是否已注入」不再是每个后端一个布尔，而是比对「cookie 罐里当前装的是哪个 token」—— 来回切换时每次都会装回正确的 token 并 reload 一次。
- 切换顺序：目标/标题/菜单打勾立即更新 → 窗口先回 `index.html`（显示「正在连接 …」）→ 建隧道、等就绪、换 cookie → 再 navigate。切换期间绝不停留在上一台主机的可操作画面上。
- `keepLocalRunning`（默认 `true`）：切去远端时本地 `Server` 留在后台；启动时 `active` 就是远端则 recover/migrate/verify + `Server::start` 全部推迟到第一次切回本地（`local_prepared` 保证只跑一次，`runtime_dir()` 的缓存语义不变）。本地服务在后台崩溃时不弹模态框，只丢弃句柄，切回时重启。
- 启动 5 秒后的静默更新检查只在本地模式且本地服务已在跑时执行；后台更新完成后不会把窗口从远端拽回本地，结果 toast 等切回本地再投递。
- 窗口标题：本地 `Pi`，远端 `Pi — <label>`；`on_document_title_changed` + 每次 page-load 都把页面的 `<title>` 改回这个值。

### 实测与真机冒烟结论

在 Windows 11 本机（WebView2 CDP 探针 + 真实 `C:\Windows\System32\OpenSSH\ssh.exe` 到 `130` 的隧道）完成端到端窗口级实测：

- **编译与单测**：`cargo check`、`cargo clippy --all-targets` 零警告通过；`cargo test` **23 passed, 0 failed**（含 Windows 专属的 `prefers_the_system_openssh`）。
- **无 `hosts.json` 本地回归**：首启分配并持久化 `41674`，二次启动 749ms 内复用 `41674`，`localStorage` 标记原样读回；`typeof window.piDesktop.selectDirectory === "function"`、`window.piWebDesktop.remote === false`、`/api/sessions -> 200`、标题 `Pi`、`theme-state.json` 跨重启恢复。
- **远端 `45002`（带 UTF-8 BOM 的 `hosts.json`，`active: "arch-130"`）**：
  - 子进程为 `C:\WINDOWS\System32\OpenSSH\ssh.exe`，立即进 Job Object（`job object: assigned pid 51568`）；启动即远端时本地 `node.exe` 子进程数为 `0`（懒启动生效）；
  - 562ms 探到 `HTTP 307` → 自动换取 126 字节 `pi_web_session` Cookie 并注入 reload → `/api/sessions -> 200`；
  - 页面内 `typeof window.piDesktop === "undefined"`、`window.piWebDesktop.remote === true`；`theme_changed` 仍正常把 `theme-state.json` 同步为 `light`；
  - 页面内篡改 `document.title = "Attempted Tamper"` 后，Win32 顶层窗口标题仍保持 `Pi — Arch Linux (130)`；
  - **正常关窗**与 **`Stop-Process -Force` 强杀**两种场景下，子进程 `ssh.exe` 均立即退出（`alive=False`），`45002` 端口立即释放（`listening=False`）。
- **真机冒烟顺手抓到并修掉的一个首帧竞态**：本地模式首启要跑 ~1.5s 的 guard 预检，首帧 `index.html` 早就加载完并捕获了 `local_base`；而启动即连远端且瞬间失败（如 `45002` 已被占用，1ms 内报错）时，`show_error -> show_local("error.html")` 早于首帧 `on_page_load`（约 30ms），导致跳转曾被静默丢弃。现 `show_local` 在非 `index.html` 且 `local_base` 尚未就绪时短暂轮询等待，实测端口预占场景能稳定落到 `Pi — Arch Linux (130) 连接失败` 错误页。
- **两台远端常驻服务已真机部署并验证**：`130`（`systemd --user` + `linger`，仅监听 `127.0.0.1:30141`）与 `mac`（`launchd` LaunchAgent，仅监听 `127.0.0.1:30141`）均已上线，从 Windows 经 `45002` / `45001` 隧道实测 `POST /api/web-auth -> 200`、`GET /api/sessions -> 200`。

### 应用内远程主机管理器（Host Manager UI）与窗口级 ACL 隔离

在不触碰 `capabilities/remote.json` 一字的前提下，新增可视化管理窗口 `hosts-manager`（菜单 `主机(&H) → 管理远程主机…`，快捷键 `Ctrl+Shift+H`，前端为打包在 `dist/hosts.html` 的零打包器原生 Metro 深色页面）：

1. **严格的窗口级 ACL 隔离**：
   - `capabilities/remote.json` 保持原样（仅 `main` 窗口 + `http://127.0.0.1:*` + 原有 3 个 `allow-*` 命令）；
   - 新增的 7 个管理命令（`hosts_get_state`、`hosts_suggest_port`、`hosts_check_env`、`hosts_test_connection`、`hosts_save`、`hosts_open_dir`、`hosts_close_window`）在 `build.rs` 的 `AppManifest::commands` 中声明，并通过 `capabilities/hosts-manager.json` **仅授权给 `"windows": ["hosts-manager"]`（无 `remote.urls`）**，且 Rust 侧二次校验 `window.label() == "hosts-manager"`；
   - `hosts-manager` 不注入 `preload.js`，携带 `<meta name="piwd-shell" content="1" />` 与 `default-src 'none'` CSP，`on_navigation` / `on_new_window` 拒绝一切非外壳静态页跳转。
2. **口令最小暴露与无损保留**：
   - `hosts_get_state` 返回 `HostSummary` 时只含 `hasPassword: bool`、`passwordFromEnv: bool` 与环境变量**名称**，绝不向前端回传明文 `password` 或环境变量实际值；
   - 编辑已有主机时密码框默认处于 `keep` 占位态（`••••••••（保持不变）`），后端在写盘与「测试连接」时直接从原文件按 `originalId` 接续旧口令，即使顺手改了主机 `id` 也不会丢口令；仅在主动输入新口令（`set`）或点「清空口令」（`clear`）时覆盖。
3. **非破坏性三步连接诊断与端口借用**：
   - 复用 `C:\Windows\System32\OpenSSH\ssh.exe` + `ProcessJob` 建立临时隧道，分步测量并返回 `1. SSH 隧道建立`、`2. 远端 pi-web 响应`、`3. 口令鉴权验证`（明确区分「远端设了口令但未填」「口令错误」「鉴权通过」「无需口令」）；
   - 若表单 `localPort` 正被本应用当前激活会话占用，自动在 `45000..=48999` 借用空闲临时端口探活，`Drop` 守卫确保任意退出路径均立即终止 `ssh.exe` 并等待端口释放，不打断正在使用的远端会话。
4. **菜单重挂细节（`app.set_menu` 副作用）**：
   - 与前文 `autoHideMenuBar` 踩到的规律一致：每次保存触发 `app::reload_hosts` → `menu::install` 调 `app.set_menu(menu)` 时，Tauri 会把菜单栏重新挂到进程内**所有**已打开窗口上。因此在 `hosts-manager` 创建后及 `menu::install` 执行 `app.set_menu` 后均显式对 `hosts-manager` 调 `hide_menu()`，实测热更新菜单后子窗口仍保持无菜单栏。
5. **跟进上游 `@agegr/pi-web` `0.13.0`（含 `pi-coding-agent` `1.4.0`）**：
   - `scripts/seed-runtime.js`（`npm run seed`）优先使用 `vendor/node` 自动初始化并拉取最新 `@agegr/pi-web@latest`，`runtime-guard` 预检一次通过；
   - `pi-web` 0.13+ 新增了 `/api/app-update`（直接请求 `registry.npmjs.org` 提示全局 CLI 更新），内嵌 `Server::start` 现传入 `PI_WEB_SKIP_VERSION_CHECK=1` 与 `PI_WEB_APP_NAME=Pi`，将更新检查权完全收归外壳自身的 staging 原子自更新状态机。
6. **真机 CDP + Win32 实测结果**：
   - 主窗口 `http://127.0.0.1:35566/`（`pi-web` `0.13.0`）中通过 CDP 逐一调用 7 个 `hosts_*` 命令，全部被 Tauri ACL 拦截（`... not allowed on window "main", webview "main", URL: http://127.0.0.1:35566/`），原有 `theme_changed` 正常放行；
   - 通过 Win32 `WM_COMMAND` 点击 `主机(&H) → 管理远程主机…\tCtrl+Shift+H` 打开 `hosts-manager`，二次点击聚焦已有窗口不重复创建（窗口数保持 `1`，`GetMenu(hwnd) == 0`）；
   - 实测读取主机列表与 13 个 `~/.ssh/config` 别名无任何明文口令字段；修改已有主机 `localPort` 实时弹出黄色 Origin 重置提示；
   - 对真实远端主机点「测试连接」：空闲态 `45001` 三步全绿（总耗时 `534ms`：隧道 `399ms` / HTTP 307 `30ms` / `/api/web-auth` `99ms`）；故意填错口令精准返回 `口令错误`（HTTP 401）；切到远端占住 `45001` 后再测同一主机，自动借用临时端口 `45000` 三步全绿且不中断当前会话；
   - 新增主机自动分配最小空闲端口 `45000` 与 slug，保存为无 BOM UTF-8 JSON 后，主窗口 Win32 `主机(&H)` 菜单立即热更新出新主机项，未改口令框二次编辑保存后磁盘原口令完好保留。
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
