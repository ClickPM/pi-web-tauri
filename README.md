# pi-web-tauri（Pi）

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
├── dist/                       # 外壳自己的状态页（Tauri 的 frontendDist，无打包器）
│   ├── index.html              #   启动中（窗口第一帧）
│   ├── updating.html           #   更新中（带 npm 进度行）
│   ├── healing.html            #   修复运行时中（带 npm 进度行）
│   └── error.html              #   启动失败（带服务日志）
├── src-tauri/
│   ├── src/
│   │   ├── main.rs             # 入口
│   │   ├── lib.rs              # 插件/窗口/菜单/注入脚本装配
│   │   ├── app.rs              # 启动与更新状态机、运行时锁、CTA 投递
│   │   ├── server.rs           # 端口持久化、起 pi-web、等就绪、停服务
│   │   ├── guard.rs            # 驱动 guard-cli.js（不含任何完整性逻辑）
│   │   ├── job.rs              # Job Object（win32job，安全封装）
│   │   ├── paths.rs            # 运行时/资源/userData 路径解析
│   │   ├── migrate.rs          # 老 userData 与老运行时回退目录的一次性收养
│   │   ├── theme.rs            # theme-state.json ↔ 窗口主题
│   │   ├── commands.rs         # 页面能调的全部 3 个命令
│   │   ├── menu.rs             # 应用菜单（常显，末尾是「刷新」按钮）
│   │   └── log.rs              # %TEMP%/pi-web-desktop-debug.log
│   ├── inject/preload.js       # 注入页面的自定义 UI（Electron preload.js 的移植）
│   ├── resources/guard/
│   │   ├── runtime-guard.js    # ★ 从 Electron 版逐字节搬运，禁止改写
│   │   ├── updater.js          # ★ 同上
│   │   └── guard-cli.js        # 唯一的新胶水：把上面两个包成 JSON CLI
│   ├── capabilities/
│   │   ├── default.json        #   本地状态页（不给任何命令）
│   │   └── remote.json         #   http://127.0.0.1:* + 逐个 allow 的 3 个命令
│   ├── nsis/hooks.nsh          # 安装器钩子：检测并卸载 Electron 旧版
│   ├── build.rs                # 声明 3 个命令 → 生成 allow-* 权限（必需，见 PORTING 坑 2）
│   ├── icons/                  # tauri icon 生成的全套图标
│   └── tauri.conf.json
├── vendor/node/                # 内置 Node.js 运行时 → resources/node        ← 构建输入
├── runtime-seed/               # @agegr/pi-web 的 npm 生产安装 → resources/runtime-seed ← 构建输入
├── scripts/
│   ├── seed-node.ps1           # 供给 vendor/node（从 Electron 版原样复用）
│   ├── test-runtime-guard.js   # 运行时守卫回归测试（npm run test:guard）
│   └── prune-seed.js           # 打包前删掉 runtime-seed 里的 .map（约 157MB）
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
   env：`NODE_ENV=production`、`PATH` 前置内置 node 目录、`PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT`、`PI_WEB_NO_OPEN=1`、`PI_WEB_PASSWORD=<每次启动随机、不落盘>`。子进程一出生就被塞进 Job Object。
6. **等就绪**：stdout 的 `Ready` 行 + HTTP 探针，任一先到即可（401 也算就绪 —— 那正是带口令时的正确回答）。
7. **换会话 cookie**：Rust 侧直接在 loopback 上 `POST /api/web-auth` 把口令换成 `pi_web_session` cookie。**口令自始至终不进页面**，页面只拿到那个 token。
8. **导航**：窗口先显示 `index.html`，就绪后 navigate 到 `http://127.0.0.1:<port>/`（**不带内嵌凭据** —— 带了会让页面里所有相对路径 `fetch()` 直接抛异常，见 PORTING.md 坑 4），首帧加载完成时把 cookie 注入页面并重载一次。
9. **检查更新**：启动 5 秒后自动静默检查（刚自愈过 2 分钟内跳过），或菜单手动触发。装进兄弟目录 `.runtime-seed.staging` → 同一套校验验收 → `rename` 原子换入 → 重启服务并刷新页面。

## 目标机器需要什么

- **不需要 Node / npm**（已全部内置）。
- **WebView2 Runtime**：Windows 11 与打了近年更新的 Windows 10 自带。安装包配了 `downloadBootstrapper`，缺失时静默下载安装（需要联网）；完全离线的老 Win10 机器请预装 [Evergreen Standalone Installer](https://developer.microsoft.com/microsoft-edge/webview2/)。
- 需要在 App 内配置一个**模型提供商的 API Key** 才能真正对话。
- 仅 **Windows x64**；未签名，SmartScreen 提示「未知发布者」点「仍要运行」。

## 从零准备构建输入

```powershell
# 1. 外壳依赖（只有 Tauri CLI）
npm install

# 2. 运行时种子（@agegr/pi-web 生产安装，含预构建 .next）
mkdir runtime-seed; cd runtime-seed; npm init -y
npm install @agegr/pi-web@latest --omit=dev --registry=https://registry.npmmirror.com
cd ..

# 3. 内置 Node 运行时（win-x64）—— 全自动
npm run seed:node

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

所以 `src-tauri/src/commands.rs` 里只有 3 个命令，且两个不收参数、一个只收一个立刻收窄成 `dark|light` 的字符串。加新命令前先问：**如果这个命令被页面里一段恶意字符串调用，最坏会发生什么？** 答案必须是「弹个用户还得确认的对话框」这个量级。

反方向（Rust → 页面）走 `webview.eval()`，**不需要任何 ACL 权限** —— 更新 toast、npm 进度行、会话 cookie 注入都没有在 capability 里留下口子。所以 `capabilities/remote.json` 的 `permissions` 里只有那 3 个 `allow-*`，没有任何 `core:*` / `plugin:*`；别往里加。
