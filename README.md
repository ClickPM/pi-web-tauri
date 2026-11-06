# pi-web-tauri（Pi）

> [!WARNING]
> **⚠️ 项目已停止维护 / ARCHIVED & DEPRECATED**
>
> 本项目已停止更新并归档，后续开发已迁移至 **[ClickPM/Pi-Hosts](https://github.com/ClickPM/Pi-Hosts)**（基于 .NET 10 + WPF + WebView2 重写的桌面外壳）。仓库代码仅保留作为历史参考，不再接受 Issue、PR 或功能更新。

把 [pi-web](https://github.com/agegr/pi-web)（pi 编程智能体的网页界面，npm 包 **`@agegr/pi-web`**）打包成一个**开箱即用的桌面应用**：双击即用，没有浏览器、没有地址栏、没有常驻终端窗口，**目标机器无需预装任何运行时**。

这是 [pi-web-desktop](../pi-web-desktop)（Electron 版）的 **Tauri 2 + Rust 重写**。外壳换了，但运行时供给、自更新、完整性自愈这三块的**行为和文件格式保持一致** —— 老用户升级不重装运行时、不丢偏好。

**核心特性**

- 🧳 **内置 Node.js 运行时** —— 目标机器无需 Node/npm，拷到空电脑双击即用。
- ⚡ **就地运行，首启秒开** —— 直接从（可写的）安装目录跑 pi-web，不做首启复制。
- 🔄 **运行时自更新** —— App 内「检查更新」直接装 `@agegr/pi-web@latest`，走 **staging + 校验通过才原子换入**，更新失败/断网/中途被杀都不会损坏正在用的运行时。
- 🩺 **启动自检与自愈** —— 每次启动验证运行时原生模块是否真的能加载；发现是安装被中断留下的残缺文件，自动按锁定版本重装修复（不趁机升级）。
- 🔒 **回环 + 每次启动随机口令** —— 服务只听 `127.0.0.1`；口令每次启动重新生成、不落盘，并由外壳在回环上换成会话 cookie，**口令本身从不进入页面**（需要 pi-web ≥ 0.9.1，更低版本的降级见 PORTING.md 坑 4）。
- 🪟 **进程树由内核连坐回收** —— Job Object + `KILL_ON_JOB_CLOSE`：外壳被**任务管理器强杀**，node 与 next 子孙进程一并消失。
- 🦀 **外壳无 `unsafe`** —— `grep -rn "unsafe" src-tauri/src` 为空。

与 Electron 版逐条的差异、每个坑的实测结论，见 **[PORTING.md](PORTING.md)**。

## 目录结构

```
pi-web-tauri/
├── dist/                       # 外壳自己的状态页与配置页（Tauri 的 frontendDist，无打包器）
│   ├── index.html              #   启动中（窗口第一帧）
│   ├── updating.html           #   更新中（带 npm 进度行）
│   ├── healing.html            #   修复运行时中（带 npm 进度行）
│   ├── error.html              #   启动失败（带服务日志）
│   └── hosts.html              #   远程主机管理器（独立窗口 hosts-manager 专用）
├── src-tauri/
│   ├── src/
│   │   ├── main.rs             # 入口
│   │   ├── lib.rs              # 插件/窗口/菜单/注入脚本装配
│   │   ├── app.rs              # 启动与更新状态机、运行时锁、CTA 投递
│   │   ├── server.rs           # 端口持久化、起 pi-web、等就绪、停服务
│   │   ├── hosts.rs            # %APPDATA%\Pi\hosts.json 解析/校验/原子保存与 ~/.ssh/config 别名提取
│   │   ├── hosts_ui.rs         # hosts-manager 独立窗口与 7 个主机管理/探活命令（主窗口不可调）
│   │   ├── remote.rs           # 远端主机：SSH 本地端口转发 + 就绪探测 + 换 cookie + 三步连接诊断
│   │   ├── guard.rs            # 驱动 guard-cli.js（不含任何完整性逻辑）
│   │   ├── job.rs              # Job Object（win32job，安全封装）
│   │   ├── paths.rs            # 运行时/资源/userData 路径解析
│   │   ├── migrate.rs          # 老 userData 与老运行时回退目录的一次性收养
│   │   ├── theme.rs            # theme-state.json ↔ 窗口主题
│   │   ├── commands.rs         # pi-web 页面能调的全部 3 个命令
│   │   ├── menu.rs             # 应用菜单（常显；「主机」切换器与管理器入口；末尾是「刷新」按钮）
│   │   └── log.rs              # %TEMP%/pi-web-desktop-debug.log
│   ├── inject/preload.js       # 注入页面的自定义 UI（Electron preload.js 的移植）
│   ├── resources/guard/
│   │   ├── runtime-guard.js    # ★ 从 Electron 版逐字节搬运，禁止改写
│   │   ├── updater.js          # ★ 同上
│   │   └── guard-cli.js        # 唯一的新胶水：把上面两个包成 JSON CLI
│   ├── capabilities/
│   │   ├── default.json        #   主窗口本地状态页（不给任何应用命令）
│   │   ├── remote.json         #   主窗口 http://127.0.0.1:* + 逐个 allow 的 3 个命令
│   │   └── hosts-manager.json  #   仅 hosts-manager 本地窗口可调的 7 个主机管理命令（无 remote.urls）
│   ├── nsis/hooks.nsh          # 安装器钩子：检测并卸载 Electron 旧版
│   ├── build.rs                # 声明应用命令 → 生成按窗口隔离的 allow-* 权限（必需，见 PORTING 坑 2）
│   ├── icons/                  # tauri icon 生成的全套图标
│   └── tauri.conf.json
├── vendor/node/                # 内置 Node.js 运行时 → resources/node        ← 构建输入
├── runtime-seed/               # @agegr/pi-web 的 npm 生产安装 → resources/runtime-seed ← 构建输入
├── scripts/
│   ├── seed-node.ps1           # 供给 vendor/node（从 Electron 版原样复用）
│   ├── seed-runtime.js         # 供给/更新 runtime-seed 至上游最新 @agegr/pi-web（npm run seed）
│   ├── test-runtime-guard.js   # 运行时守卫回归测试（npm run test:guard）
│   ├── prune-seed.js           # 打包前删掉 runtime-seed 里的 .map
│   └── remote/                 # 远端常驻模板：systemd --user / launchd / hosts.example.json
├── build/                      # 图标 SVG 源 + _make_icons.js
└── PORTING.md                  # 每个坑的实测结论、未达成项与已知降级
```

> **构建输入 vs 入库源码**：`vendor/`、`runtime-seed/` 体积大、已 gitignore，需按下文准备。

## 应用身份

`appId`（Tauri 的 `identifier`）保持 **`com.agegr.piwebdesktop` 不变**，`productName` 保持 **`Pi`**，数据目录沿用 pi 的 `~/.pi/agent`。

外壳自己的状态（`port.json`、`theme-state.json`）写在 **`%APPDATA%\Pi`** —— 和 Electron 版**同一个目录**。这是刻意的：Tauri 默认会用 `%APPDATA%\com.agegr.piwebdesktop`，那样老用户的主题会重置，只读安装时那份约 900MB 的运行时还得重新复制一遍。详见 PORTING.md 坑 6。

## 运行架构

1. **收敛上次中断的原子切换**（`guard-cli.js recover`）—— 必须在任何人问「运行时在哪」之前跑。
2. **解析运行时目录**：安装目录里的 `resources/runtime-seed` 可写 → **就地运行**（秒开）；只读（如装到 Program Files）→ 用 robocopy 复制到 `%APPDATA%\Pi\runtime`，写 `.seeded` 标记，只复制一次。
3. **运行时完整性预检**（`guard-cli.js verify`）：结构文件 + **本平台**原生模块能否在内置 node 的子进程里 `require` + npm 残留临时目录。判定可修复 → 显示 `healing.html` 并按锁定版本重装。
4. **决定端口**：读 `%APPDATA%\Pi\port.json` 优先复用；被占用才另选 30000–44999 的空闲端口并写回。
5. **起服务**：
   ```
   <node>\node.exe  <runtime>\node_modules\@agegr\pi-web\bin\pi-web.js \
       -H 127.0.0.1 -p <port> --no-open
   ```
   env：`NODE_ENV=production`、`PATH` 前置内置 node 目录、`PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT`、`PI_WEB_NO_OPEN=1`、`PI_WEB_SKIP_VERSION_CHECK=1`（关闭网页端自身的全局 npm 更新检查，统一由外壳原子热更新托管）、`PI_WEB_APP_NAME=Pi`、`PI_WEB_PASSWORD=<每次启动随机、不落盘>`。子进程一出生就被塞进 Job Object。
6. **等就绪**：stdout 的 `Ready` 行 + HTTP 探针，任一先到即可（401 也算就绪 —— 那正是带口令时的正确回答）。
7. **换会话 cookie**：Rust 侧直接在 loopback 上 `POST /api/web-auth` 把口令换成 `pi_web_session` cookie。**口令自始至终不进页面**，页面只拿到那个 token。
8. **导航**：窗口先显示 `index.html`，就绪后 navigate 到 `http://127.0.0.1:<port>/`（**不带内嵌凭据** —— 带了会让页面里所有相对路径 `fetch()` 直接抛异常，见 PORTING.md 坑 4），首帧加载完成时把 cookie 注入页面并重载一次。
9. **检查更新**：启动 5 秒后自动静默检查（刚自愈过 2 分钟内跳过），或菜单手动触发。装进兄弟目录 `.runtime-seed.staging` → 同一套校验验收 → `rename` 原子换入 → 重启服务并刷新页面。

## 多主机切换（Windows / macOS / Linux）

同一个窗口里随时在 **Windows 本地（内嵌服务）**、**Mac mini**、**Arch Linux (130)** 等主机之间切换：菜单栏 **主机(&H)**，或 `Ctrl+Alt+1`（本地）/ `Ctrl+Alt+2`、`3`…（按 `hosts.json` 顺序）。

**架构**：远端的 pi-web 只监听它自己的 `127.0.0.1:30141`；外壳用 **Windows 自带的** `C:\Windows\System32\OpenSSH\ssh.exe` 建 `-L` 本地端口转发到本机 `127.0.0.1:<localPort>`（45000–48999），WebView 只和本机回环说话。口令由外壳在回环上 `POST /api/web-auth` 换成 cookie，**不进页面 URL、不进前端 JS**。`ssh.exe` 进 Job Object，外壳被强杀时隧道一并回收。

### 1. Windows 端（本机）

1. 确认 `C:\Windows\System32\OpenSSH\ssh.exe` 存在（「设置 → 系统 → 可选功能 → OpenSSH 客户端」）。**不会**使用 Git Bash / MSYS 的 ssh。
2. 在 `%USERPROFILE%\.ssh\config` 里配好主机别名与密钥，并**手动连一次**确认指纹（外壳以 `BatchMode=yes` 运行，不会弹密码框或指纹确认）：
   ```powershell
   C:\Windows\System32\OpenSSH\ssh.exe mac  'echo ok'
   C:\Windows\System32\OpenSSH\ssh.exe 130  'echo ok'
   ```
3. 在菜单 **`主机(&H) → 管理远程主机…`**（`Ctrl+Shift+H`）中可视化添加、排序、测试并保存远程主机（无需手改 JSON）：
   - 自动提取 `%USERPROFILE%\.ssh\config` 中的非通配 `Host` 别名供点击/补全，也可直接填 `user@ip`；
   - 新增主机自动从 `45000–48999` 分配当前未占用的最小本机端口，修改已有主机端口会给出 Origin 偏好重置提示；
   - **一键「测试连接」**：非破坏性建立临时 OpenSSH 隧道（若目标端口正被当前激活会话占用，自动借用空闲端口探活并立即回收），分三步输出耗时与原因（`1. SSH 隧道建立` → `2. 远端 pi-web 响应` → `3. 口令鉴权验证`）；
   - **口令最小暴露**：已存口令与环境变量值绝不回传前端（仅显示「环境变量 / 已设口令 / 无口令」状态与 `••••••••（保持不变）` 占位），未改口令框时保存自动保留原口令；
   - 点击 **「保存并生效」** 原子写入 `%APPDATA%\Pi\hosts.json`（标准 UTF-8 无 BOM），顶部 `主机(&H)` 菜单立即热更新，正在连接的远端参数变更自动重连、被删平滑切回 `Windows 本地`。
   - 也可按模板手动编辑 `%APPDATA%\Pi\hosts.json`（`Copy-Item scripts\remote\hosts.example.json $env:APPDATA\Pi\hosts.json`），改完点「主机 → 重新读取 hosts.json」。
4. 远端设了口令时，推荐把口令放进**用户环境变量**（`passwordEnv` 指向它，优先于文件里的 `password`；管理界面会实时提示当前进程环境中该变量是否已生效）：
   ```powershell
   [Environment]::SetEnvironmentVariable('PI_WEB_REMOTE_PASSWORD_130', '<与远端 PI_WEB_PASSWORD 相同>', 'User')
   ```
   改完系统用户环境变量需重启 Pi 使新进程继承。

`hosts.json` 字段：

| 字段 | 说明 |
|---|---|
| `active` | 上次激活的主机（`"local"` 或某个 `id`），切换成功后自动写回（只改这一个值，其余内容原样保留） |
| `keepLocalRunning` | 默认 `true`：切去远端时本地服务继续后台运行，切回秒开；启动即远端时本地服务懒启动 |
| `id` | 唯一，不能为 `local`，仅限字母数字 `- _ .` |
| `sshTarget` | `~/.ssh/config` 别名或 `user@host`（不能以 `-` 开头） |
| `sshPort` | 可省略（交给 ssh config） |
| `localPort` | **必须在 45000–48999** 且互不重复；每台主机一个固定端口 = 一个独立 origin = 独立持久的 `localStorage` |
| `remoteHost` / `remotePort` | 远端 pi-web 的监听地址，默认 `127.0.0.1:30141` |
| `passwordEnv` / `password` | 口令来源；都为空 = 远端无口令 |

不合法的条目会被跳过（原因写进调试日志，「重新读取」时也会弹窗列出），不影响其余主机和本地模式。文件不存在时菜单里只有「Windows 本地」。

### 2. Arch Linux（systemd --user）

```bash
npm install -g @agegr/pi-web --allow-scripts=@agegr/pi-web,node-pty  # 需要 Node ≥ 22.19
mkdir -p ~/.config/pi-web ~/.config/systemd/user
printf 'PI_WEB_PASSWORD=%s\n' "$(openssl rand -hex 16)" > ~/.config/pi-web/env && chmod 600 ~/.config/pi-web/env
cp scripts/remote/pi-web.service ~/.config/systemd/user/
command -v pi-web                     # 不在 /usr/bin 或 ~/.local/bin（如 nvm）时改 service 里的 PATH
systemctl --user daemon-reload && systemctl --user enable --now pi-web
loginctl enable-linger "$USER"        # 无人登录时也常驻
curl -sI http://127.0.0.1:30141/      # 200，或带口令时 307 → /login
```

### 3. macOS（launchd LaunchAgent）

```bash
npm install -g @agegr/pi-web --allow-scripts=@agegr/pi-web,node-pty
cp scripts/remote/com.agegr.piweb.plist ~/Library/LaunchAgents/
/usr/libexec/PlistBuddy -c "Set :EnvironmentVariables:PI_WEB_PASSWORD $(openssl rand -hex 16)" \
    ~/Library/LaunchAgents/com.agegr.piweb.plist        # 不要口令就跳过这步
chmod 600 ~/Library/LaunchAgents/com.agegr.piweb.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agegr.piweb.plist
curl -sI http://127.0.0.1:30141/
```

还需在「系统设置 → 通用 → 共享」打开「远程登录」。模板里的 PATH 已包含 `/opt/homebrew/bin:/usr/local/bin`。

### 远端模式下的差异

- 窗口标题变为 `Pi — <label>`；本地为 `Pi`。
- **不注入 `window.piDesktop`**：原生目录对话框浏览的是 Windows 本机、返回 `D:\...`，对远端无意义；pi-web 自动回退为网页内目录浏览（列的是远端文件系统）。主题同步原生标题栏照常工作。
- 「检查更新…」禁用（它只管本机内嵌运行时）；「重启内嵌服务」变为「重连当前远端」（重建隧道并重新认证）。启动 5 秒后的静默更新检查只在本地模式执行，更新结果 toast 在切回本地后才显示。
- 连接失败或隧道中途断开会显示错误页（含 SSH stderr 与排障提示），随时 `Ctrl+Alt+1` 切回本地。
## 目标机器需要什么

- **不需要 Node / npm**（已全部内置）。
- **WebView2 Runtime**：Windows 11 与打了近年更新的 Windows 10 自带。安装包配了 `downloadBootstrapper`，缺失时静默下载安装（需要联网）；完全离线的老 Win10 机器请预装 [Evergreen Standalone Installer](https://developer.microsoft.com/microsoft-edge/webview2/)。
- 需要在 App 内配置一个**模型提供商的 API Key** 才能真正对话。
- 仅 **Windows x64**；未签名，SmartScreen 提示「未知发布者」点「仍要运行」。

## 从零准备构建输入

```powershell
# 1. 外壳依赖（只有 Tauri CLI）
npm install

# 2. 内置 Node 运行时（win-x64）—— 全自动
npm run seed:node

# 3. 运行时种子（自动拉取上游最新 @agegr/pi-web 生产安装，含预构建 .next）
npm run seed

# 4. 图标（已入库，只有改了 SVG 才需要重跑）
npm run icons
npx tauri icon build/icon.png -o src-tauri/icons
```

> 之后日常只需 `npm run seed` 把运行时种子升到最新发布版再打包。
> 也可以直接从 `../pi-web-desktop` 复制这两个目录 —— 它们是同一份东西。

## 开发 / 运行

```bash
npm run dev
```

开发态直接就地从项目里的 `runtime-seed` 运行，**秒开**；关窗自动结束后台服务。

首次 `cargo` 编译约 2–3 分钟，之后增量约 40 秒。

排障：外壳把关键步骤写到 `%TEMP%\pi-web-desktop-debug.log`（`guard-cli.js` 用 `[guard]` 前缀写同一个文件，所以一条时间线能看完整个启动）。

可选环境变量：

- `PI_WEB_REGISTRY` —— 自更新使用的 npm registry（默认 `https://registry.npmmirror.com`）。
- `PI_WEB_AUTO_UPDATE_CHECK=0` —— 关闭启动后的自动检查更新。
- `PI_CODING_AGENT_DIR` —— 指定 pi 会话数据目录（默认 `~/.pi/agent`，直接透传给子进程）。

运行时守卫回归测试（和 Electron 版同一份）：

```bash
npm run test:guard
```

## 打包安装程序

```bash
npm run build       # 生成 <target>/release/bundle/nsis/Pi_<ver>_x64-setup.exe
npm run build:dir   # 只编译不打包（调试更快）
```

> `<target>` 是 cargo 的输出目录；本机设了 `CARGO_TARGET_DIR=D:\target`，所以产物在 `D:\target\release\bundle\nsis\`，**不在项目里**。没设这个变量时才是 `src-tauri/target/`。
>
> 打包前会自动跑 `scripts/prune-seed.js`（`tauri.conf.json` 的 `beforeBundleCommand`），把 `runtime-seed` 里的 `.map` 源码映射删掉 —— 这是 electron-builder 里那条 `"!**/*.map"` 过滤的等价物，Tauri 的 `bundle.resources` 没有排除语法所以只能做成一步。pi-web 0.9.1 上是 8,977 个文件、约 157MB。`npm run seed` 会把它们带回来，应用内更新也会（它装的是全新的 npm 树），这一步只影响**装进安装包**的内容。

安装器默认装到 `%LOCALAPPDATA%\Pi`（可写 → 就地运行 → 首启秒开），可在安装页改。
安装器会检测 Electron 旧版（electron-builder 的卸载注册表项）并**询问**是否先卸载 —— 两者的 NSIS 记录互不相认，不处理就会并排装两份、抢同一个 `~/.pi/agent`。

---

## 开发约束（加新能力必读）

> **本仓库只开发桌面外壳层。pi-web 和 pi-coding-agent 一律以上游 npm 包形式获取，本仓库不包含、不修改它们的源码。**

| 归属 | 职责 | 改动流向 |
|---|---|---|
| **上游 [agegr/pi-web](https://github.com/agegr/pi-web)** | pi-web 网页端本身的功能/页面/接口 | 给上游提 PR → 上游合并发版 → 本项目 `runtime-seed` / 自更新从 npm 拉到 |
| **本仓库 pi-web-tauri** | 桌面外壳：窗口、内置运行时、自更新、命令、注入 UI | 在 `src-tauri/` 改 → 重新打包安装程序 |

**四条铁律：**

1. **pi-web 的任何修改不在本仓库做** —— 绝不改 `runtime-seed` 里的 pi-web 源码 / `.next`，那会被下一次 `npm install @agegr/pi-web@latest` 冲掉，而应用内「检查更新」就是干这个的。
2. **Rust 代码禁止 `unsafe`。** 撞到需要 unsafe 的场景先找带 safe wrapper 的 crate；找不到就换方案并在 PORTING.md 写明代价。
3. **`runtime-guard.js` 和 `updater.js` 不重写。** 它们是两次生产事故换来的，且有回归测试。Rust 侧只负责：用内置 node 起子进程执行 → 解析 JSON → 编排状态机。
4. **`appId` 与数据目录不变**（`com.agegr.piwebdesktop` / `~/.pi/agent`）。

### 加一个页面能调的能力，要先想清楚

Electron preload 跑在**隔离世界**，`contextIsolation` 挡住了页面脚本。Tauri 的注入脚本跑在**页面主世界** —— 而 pi-web 页面里渲染的是智能体产出的内容。**这层隔离没有了。**

所以主窗口（`capabilities/remote.json`，覆盖本地或远端 `http://127.0.0.1:*`）暴露给 `pi-web` 页面的命令只有 `src-tauri/src/commands.rs` 里的 3 个，且两个不收参数、一个只收一个立刻收窄成 `dark|light` 的字符串。加新命令前先问：**如果这个命令被页面里一段恶意字符串调用，最坏会发生什么？** 答案必须是「弹个用户还得确认的对话框」这个量级。

而应用内配置界面（如「管理远程主机」`hosts-manager` 窗口，加载打包静态页 `dist/hosts.html`，不注入 `preload.js`、不加载任何外部 URL）则走**窗口级 ACL 硬隔离**：在 `capabilities/hosts-manager.json` 中仅授权 `"windows": ["hosts-manager"]` 且不配 `remote.urls`。主窗口里的任何 `127.0.0.1:*` 页面调用 `hosts_*` 命令都会在进入 Rust 处理器前直接被 Tauri ACL 拦截拒绝。

反方向（Rust → 页面）走 `webview.eval()`，**不需要任何 ACL 权限** —— 更新 toast、npm 进度行、会话 cookie 注入都没有在 capability 里留下口子。所以 `capabilities/remote.json` 的 `permissions` 里只有那 3 个 `allow-*`，没有任何 `core:*` / `plugin:*`；别往里加。
