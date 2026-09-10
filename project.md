# ArchStore 技术设计文档

- 项目代号：ArchStore
- 定位：Arch Linux 图形化软件商店（pacman 仓库 / AUR / Flatpak 统一管理）
- 许可证：GPL-3.0-or-later
- 文档版本：v0.2.0（可落地修订版）
- 修订日期：2026-09-10

> 本版是对 v0.1.0 草案的重写。v0.1.0 中的若干内容经实测无法编译或与本机真实环境不符，属于"照做即失败"的阻塞项，
> 详见 [附录 D：v0.1.0 缺陷与修正记录](#附录-dv010-缺陷与修正记录)。

---

## 0. 文档约定

本文档面向实现者（人与 AI Agent），因此遵循以下规则：

1. **可验证优先**：所有版本号、命令行参数、API 名称均经过本机实测或官方文档核对。
   受污染的示例代码一律标注 `// 示意`；标注为 `rust` 的完整代码块必须能通过 `cargo build`。
2. **不含发明 API**：文中出现的 `backend::*`、`plan::*`、`cache::*` 是**本项目自己定义**的类型，
   它们需要被实现，而不是从某个 crate 直接 import。
3. **命令必须实测**：附录 A/B/C 中的命令行均已在本机执行并附真实输出特征。凡是无法在目标机器上
   验证的写法，要么给出降级方案，要么明确标为"待验证（Phase X 前置任务）"。
4. **失败路径与成功路径同等重要**：本文档中每个功能都必须写明"失败时用户看到什么"。

### 0.1 术语表

| 术语 | 含义 |
| --- | --- |
| 源（Source） | 软件来源：官方仓库、AUR、Flatpak 远程仓库 |
| 条目（Package 摘要） | 列表页展示的一行数据，字段少、可缓存、无网络依赖 |
| 详情（Detail） | 详情页展示的完整数据，可能来自网络，允许缺失 |
| 事务（Transaction） | 一次"用户确认后的系统变更"，是提权边界的最小单位 |
| 计划（TransactionPlan） | 事务的**声明式**描述（纯数据），可序列化为 JSON |
| 只读句柄 | 以普通用户身份打开的 libalpm 句柄，仅用于查询 |
| 卸载器（Remover） | `archstore-helper`，唯一以 root 运行的进程 |

### 0.2 环境基线与最低要求

本项目**只支持 Arch Linux 及其衍生发行版**（依赖 pacman、libalpm ABI、polkit）。实测基线：

| 组件 | 本机实测版本 | 项目最低要求 | 说明 |
| --- | --- | --- | --- |
| 发行版 | Arch Linux (rolling) | Arch 及其衍生 | 非 Arch 系统直接拒绝启动并给出提示 |
| gtk4 | 4.22.4 | **>= 4.18** | 与 `gtk4 = 0.11` + `v4_18` feature 对应 |
| libadwaita | 1.9.3 | **>= 1.8** | 与 `libadwaita = 0.9` + `v1_8` feature 对应 |
| pacman / libalpm | 7.1.0 / libalpm 16 | libalpm >= 15 | `alpm = 5` 绑定 libalpm 16 |
| flatpak | 1.18.2 | >= 1.14（可选） | 未安装则 Flatpak 源整体置灰 |
| rust | 1.98.1 | >= 1.92（gtk4 0.11 的 MSRV） | Edition 2024 |
| polkit / pkexec | polkit 127 | 运行时必需 | 提权唯一通道 |
| AUR 助手 | 本机仅有 `yay 13.0.1` | `paru` 或 `yay` 任一（可选） | 两者都无则 AUR 源置灰 |

启动时执行环境自检（见 §2.3 `--doctor`），不满足最低要求时给出**可操作**的错误信息，而不是崩溃。

---

## 1. 项目概述

### 1.1 定位

ArchStore 为 Arch Linux 提供类似 GNOME Software / KDE Discover 的图形化软件商店体验，统一管理三类软件源，
同时坚持 Arch 的"透明、可控"哲学：**任何系统变更都必须经过用户可见的审查与显式确认**。

### 1.2 参考项目与可借鉴点

| 项目 | 技术栈 | 借鉴内容 | 不借鉴内容 |
| --- | --- | --- | --- |
| Aurora | GTK4 + libadwaita | 事务队列 + 审查界面、实时日志 | — |
| PacHub | GTK4 + libadwaita (Python) | 侧栏分类、仓库徽章 | 其 Python/libalpm 混用方式 |
| Shelly | GTK (C++) | 多后端统一、虚拟依赖提供者选择 | C++ 绑定层 |
| xPackageManager | Rust + Slint | 设置页持久化、依赖树视图 | Slint（本项目统一 GTK4） |
| blossom-arc | Rust 库 | 后端统一抽象层思路 | 直接复用其内部 API |

### 1.3 核心设计原则（可检验）

| 原则 | 可检验的判据 |
| --- | --- |
| 透明可控 | 不存在任何"点击即静默执行"的按钮；执行前必然出现计划清单 |
| 绝不 root 跑 GUI | 进程树中 GUI 与 tokio 运行时永远是非 root；`pkexec` 之后不含 GTK |
| 主线程零阻塞 | 主线程不出现文件读写、数据库查询、网络与 `Command::output()` |
| 显式联网 | 网络请求只由用户动作或明确标注的定时器触发；每个请求可追溯到触发点 |
| 失败可用 | 任一后端不可用时，其余功能完整可用，UI 给出原因与修复建议 |
| 可离线启动 | 断网时应用照常启动、可查已安装、可读缓存 |

### 1.4 范围（Scope）

**v0.1.0 必须做到（MVP）**

- 查询：官方仓库搜索/分类浏览、已安装列表、可更新列表、软件详情
- 查询：AUR 搜索与详情（RPC，无需助手）
- 查询：Flatpak 已安装列表与远程搜索
- 事务：安装 / 卸载 / 更新官方仓库包（经 pkexec + `archstore-helper`）
- 事务：安装 / 卸载 / 更新 Flatpak 应用（经 pkexec + helper，调用 flatpak 的系统安装）
- 事务：AUR 包经用户已安装的助手（`yay` / `paru`）在**用户身份**下完成
- 缓存、代理、主题（浅色/深色/跟随系统）、中文界面

**明确不在 v0.1.0 范围（禁止在 MVP 阶段实现）**

- 平铺式 (flatpak) 应用与系统包的"同一软件去重合并"（复杂度高、收益低）
- 依赖树的可视化图形控件（MVP 用可展开列表 + 文本树）
- 在线机器翻译（默认关闭，见 §7.2）
- 评论/评分写回、ODRS 写操作
- 包构建（AUR 的 makepkg 流程全部交给助手）
- 多用户/多会话并发管理、远程仓库镜像管理界面
- AppImage / Snap / 自定义仓库

---

## 2. 技术栈与依赖

### 2.1 语言与框架

| 组件 | 选型 | 说明 |
| --- | --- | --- |
| 语言 | Rust 2024 Edition（MSRV 1.92） | gtk4 0.11 要求 Rust >= 1.92 |
| GUI | GTK4 + libadwaita | 通过 `gtk4`/`libadwaita` 官方 Rust 绑定 |
| 异步 | Tokio（**仅网络层**） | 通过 `glib::MainContext::spawn_local` 与主循环桥接 |
| 提权 | polkit + `pkexec` + 独立 helper 二进制 | 见 §5.2 |
| 构建 | Cargo workspace + PKGBUILD | **不使用 Meson**：纯 Rust 项目用 Cargo 足矣，多一层 Meson 只增加故障面 |
| 配置 | TOML（`toml` + `serde`） | 原子写盘，见 §6 |
| 缓存 | 自研文件缓存（JSON + 原子替换 + LRU 索引） | **不引入 sled/redb**：见 §2.3 决策记录 |
| 界面文本 i18n | gettext-rs 0.8 | `bindtextdomain` + `textdomain` + `gettext` |
| 日志 | `tracing` + `tracing-subscriber` | 输出到 stderr 与 `$XDG_CACHE_HOME/archstore/archstore.log` |

### 2.2 依赖清单（版本已核对 crates.io，2026-09-10）

下表的"实测版本"是本机 `Cargo.lock` 解析结果或 crates.io 当前稳定版；实现时**以 `Cargo.lock` 锁定**，
不要在文档里手写精确补丁号。

```toml
# 工作区根 Cargo.toml（依赖集中在 [workspace.dependencies]，成员用 workspace = true 引用）
[workspace]
resolver = "2"
members = ["crates/archstore-core", "crates/archstore-gui", "crates/archstore-helper"]

[workspace.package]
edition = "2024"
rust-version = "1.92"
license = "GPL-3.0-or-later"

[workspace.dependencies]
# --- GUI ---
# feature 名 = 需要的最低库版本（v4_18 ↔ gtk 4.18）；必须 >= 运行环境版本，否则运行期 panic（§2.4）
gtk4        = { version = "0.11", features = ["v4_18"] }   # 实测解析 0.11.4
libadwaita  = { version = "0.9",  features = ["v1_8"] }    # 实测解析 0.9.2（内部依赖 gtk4 0.11）
glib        = "0.22"                                        # 实测解析 0.22.9（由 gtk4 传递引入）
gio         = "0.22"                                        # 实测解析 0.22.9

# --- 包管理（仅 core / helper；alpm 是 GPL-3.0，见 §13）---
alpm = "5.0"                                                # 实测解析 5.0.2，绑定 libalpm 16
toml = "1"                                                  # 实测解析 1.1.x（配置解析）

# --- 网络 ---
tokio   = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "fs", "process", "io-util"] }
# 用 reqwest 的默认 rustls 提供者（aws-lc-rs）；不要手写 rustls-no-provider，否则必须自己装 process 级 CryptoProvider
reqwest = { version = "0.13", features = ["json", "gzip", "brotli", "socks", "system-proxy"] }
# raur 8 依赖 reqwest 0.13；rusttls-ring 让它用 ring 提供者（见下方"TLS 提供者"注意事项）
# 注意：default-features = false 会同时关掉 async，必须显式加回 "async"，否则 raur::Handle / raur::Raur 不存在
raur    = { version = "8", default-features = false, features = ["async", "rusttls-ring"] }

# --- 序列化 / 异步 trait / 错误 / 日志 / i18n ---
serde     = { version = "1", features = ["derive"] }
serde_json = "1"
async-trait = "0.1"
thiserror = "2"
anyhow    = "1"
tracing   = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
gettext-rs = "0.8"

# --- 单一实例保护：默认用 $XDG_RUNTIME_DIR 上的 flock（零依赖，见 §3.4）---
# 若将来需要"第二次启动激活已有窗口"，再引入 zbus（纯 D-Bus，无 libdbus 依赖）：
# zbus = { version = "5", default-features = false, features = ["blocking-api"] }
```

**TLS 提供者（实测结论：不必手动装 provider，但必须只用一个 `Client`）**

`rustls 0.23` 要求进程级存在一个 `CryptoProvider`。两个 crate 的默认选择不同：

- 本项目的 `reqwest`（开启默认 `rustls` feature）自带 `aws-lc-rs` 提供者；
- `raur` 的 `rusttls-ring` feature 启用 `reqwest/rustls-no-provider` + `rustls/ring`。

**实测（2026-09-10）**：用同一个 `reqwest::Client`（默认 rustls provider）同时请求 Flathub 与
AUR RPC，两者均 200 成功，且没有出现 "no process-level CryptoProvider available"。因此规则是：

1. **进程内只创建一个 `reqwest::Client`**，把它注入 `raur::Handle::new_with_client(client)`。
   不要 `Client::new()` 与 `raur::Handle::new()` 混用（那会创建两个不同 provider 配置的客户端）。
2. 代理、UA、超时统一在 `net::build_client()` 里设置一次（§6.2），所有后端共用。
3. 如果将来出现 provider 冲突，降级方案是给 `reqwest` 也改用 `rustls-no-provider` + `rustls/ring`，
   并在 `main()` 开头执行 `rustls::crypto::ring::default_provider().install_default().ok();`
   （`.ok()` 是刻意的：已安装时返回 `Err`，不是错误）。
4. 阶段 2 的第一个任务就是写一个连通性冒烟测试（Flathub + AUR 两个域名都打通）并纳入 CI 的手动测试项。

**实测解析版本（`/tmp` 验证工程，2026-09-10）**：`gtk4 0.11.4`、`libadwaita 0.9.2`、
`glib/gio/pango 0.22.9`、`gdk4 0.11.4`、`alpm 5.0.2`、`raur 8.0.0`。

**关键决策记录（ADR）**

| 决策 | 选择 | 理由 |
| --- | --- | --- |
| libalpm 绑定 | `alpm` crate | 官方 archlinux/alpm.rs 维护，`alpm = 5` 对应 libalpm 16 |
| AUR 客户端 | `raur` | 官方 aurweb RPC v5 客户端，API 稳定；`raur::Handle::new_with_client()` 可注入自定义代理客户端 |
| Flatpak 集成 | **`flatpak` CLI 子进程**，不用 `flatpak-rs`/libflatpak 绑定 | `flatpak` crate 最后发布 2022-08（0.18.1），依赖 ostree 0.20，已停止维护；CLI 是稳定契约 |
| 磁盘缓存 | **自研文件缓存**，不用 sled/redb | sled 最后发布 2024-10 且长期无维护；本项目缓存是"可丢弃的 KV"，文件 + 原子 rename 已足够，且便于人工排查与清理 |
| 清单解析 | 不解析 Flatpak manifest | manifest 仅构建期存在，运行时通过 Flathub API 拿元数据（§4.4） |
| 依赖解析（AUR） | AUR RPC `info` 的 `Depends`/`MakeDepends`/`OptDepends` 字段 | 实测可用，无需解析 `.SRCINFO`（省掉一个解析器与一类 bug） |
| UI 构建方式 | **纯 Rust 代码构建控件**（可用 `gtk::Builder` 加载 `.ui`，但禁止模板宏与 XML 混用） | 代码构建可编译期检查；`.ui`/`#[template_child]` 的字符串错配是 GTK-Rust 最常见的运行期崩溃来源 |
| 依赖注入 | 不用 `Arc<dyn Backend>` 自我引用结构 | v0.1.0 的 `PackageCard { package }` + `self.load_icon_async(icon.clone())` 在 Rust 中是自引用，无法编译 |
| 单一实例保护 | `flock` 一个 `$XDG_RUNTIME_DIR/archstore.lock`（**优先**）；若需要"激活已有窗口"再引入 zbus | flock 零依赖、无 D-Bus 失败面；zbus 只在确有多实例交互需求时才值得引入 |
| Meson | 不使用 | PKGBUILD 直接 `cargo build --release` + `install`；需要 `.desktop`/metainfo 时用 `install -Dm644` |

### 2.3 环境自检与 `--doctor`

无论 GUI 还是 CLI 入口，都支持 `archstore --doctor`：只读输出检测结果并以退出码反映问题数（0 = 全部就绪）。

```
$ archstore --doctor
[ OK ] 发行版：Arch Linux
[ OK ] gtk4 运行时 4.22.4（要求 >= 4.18）
[ OK ] libadwaita 运行时 1.9.3（要求 >= 1.8）
[ OK ] libalpm 可打开（非 root，只读）：local 972 包，sync core/extra 可用
[ OK ] polkit 可用：/usr/bin/pkexec
[ OK ] helper：/usr/lib/archstore/archstore-helper（版本 0.1.0，polkit action 已安装）
[ OK ] flatpak 1.18.2，远程 flathub
[WARN] AUR 助手：未检测到 paru/yay，AUR 安装功能将置灰（查询不受影响）
[WARN] 网络：https://aur.archlinux.org 不可达（3000ms 超时），将使用缓存
[ OK ] 缓存目录：~/.cache/archstore（可写，12.4 MB）
```

**自检必须是只读的**：不得注册同步库之外的动作、不得写 `/var/lib/pacman`、不得触发下载。

### 2.4 运行期版本校验（防 `panic`）

gtk-rs 的 feature 版本若高于运行环境的库版本，会在初始化时 panic（例如 `Symbol not found` / `AdwApplication` 缺失）。
因此 `archstore-gui` 启动序列固定为：

1. `gtk::init()`；失败则以退出码 1 结束并打印原因（不得 panic）。
2. 读取 `gtk::major_version()/minor_version()/micro_version()` 与
   `libadwaita` 运行期版本，与编译期 feature 下限比较（`gtk >= 4.18`、`adw >= 1.8`）。
3. 不满足则弹出原生 `gtk::AlertDialog`（不依赖 libadwaita 的高级组件）说明需要升级的系统包名。

---

## 3. 系统架构

### 3.1 进程与线程模型（这是稳定性的核心）

```
┌──────────────────────────────────────────────────────────────────────────┐
│ archstore (GUI 进程，普通用户)                                             │
│                                                                          │
│  [线程 1] GTK 主线程 (glib MainContext)                                   │
│     · 只做 UI：控件树、模型、CSS、事件                                     │
│     · 允许：把 Channel 消息挂到主循环 (glib::MainContext::spawn_local)     │
│     · 禁止：文件 IO / libalpm 调用 / Command::output() / reqwest 阻塞等待   │
│                                                                          │
│  [线程 2] alpm 工作线程 (std::thread, 独占 alpm::Alpm)                     │
│     · 拥有唯一 Alpm 句柄，串行处理查询请求（libalpm 句柄非 Send/Sync）      │
│     · 消息：AlpmReq{id, op} → AlpmResp{id, result}                        │
│                                                                          │
│  [线程池] tokio 多线程运行时 (网络)                                        │
│     · AUR RPC / Flathub API / 图标下载 / 翻译 API                          │
│     · 每个请求带 CancellationToken；结果一律回投到 glib MainContext        │
│                                                                          │
│  [子进程] flatpak CLI（只读查询时以普通用户运行；写操作走 helper）           │
└───────────────┬──────────────────────────────────────────────────────────┘
                │ pkexec（D-Bus 到 polkit，弹窗授权）
┌───────────────▼──────────────────────────────────────────────────────────┐
│ archstore-helper (root，短生命周期，无 GUI 依赖)                           │
│   · 只接受：计划文件路径 + 计划类型                                        │
│   · 自行校验：包名白名单字符集、目标必须存在于已注册后端、操作类型枚举         │
│   · 自行重新解析依赖（不信任 GUI 传入的依赖清单）                           │
│   · 逐行输出机器可读进度到 stdout，结束时输出 JSON 摘要                      │
└──────────────────────────────────────────────────────────────────────────┘
```

**为什么必须有 alpm 工作线程**：`alpm::Alpm` 不是 `Send + Sync`，且首次遍历 `extra` 库（实测 14955 个包）
是毫秒到数十毫秒级的同步操作。放在主线程会造成可感知卡顿；放在 `spawn_blocking` 里则需要跨线程传递句柄（不允许）。
单线程独占句柄 + 消息传递是唯一既安全又简单的方案。

**消息字典（`alpm` 线程）**

```rust
// 示意：跨线程请求/响应契约（crates/archstore-core/src/alpm_worker.rs）
pub enum AlpmOp {
    Search { query: String, repos: Vec<String>, limit: usize },
    Info { name: String },
    Installed,
    Upgradable,
    Deps { name: String },
    RevDeps { name: String },
    Groups,
    GroupMembers { group: String },
}

pub struct AlpmResp {
    pub req_id: u64,
    pub result: Result<AlpmPayload, CoreError>,
}
```

请求携带自增 `req_id`。UI 侧保存"当前有效 req_id"，**丢弃过期响应**（用户快速输入时必备）。

### 3.2 Cargo Workspace 布局

```
archstore/
├── Cargo.toml                       # workspace（依赖集中声明）
├── rust-toolchain.toml              # channel = "stable"
├── crates/
│   ├── archstore-core/              # 业务核心：无 GTK 依赖
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── error.rs             # CoreError（thiserror）
│   │   │   ├── model/
│   │   │   │   ├── mod.rs
│   │   │   │   ├── package.rs       # PackageId / PackageSource / Installed / PackageSummary / PackageDetail
│   │   │   │   └── plan.rs          # TransactionPlan / PlanItem / PlanKind / PlanRisk
│   │   │   ├── backend/
│   │   │   │   ├── mod.rs           # PackageBackend trait + Capability
│   │   │   │   ├── pacman.rs        # 只读查询（走 alpm worker）
│   │   │   │   ├── pacman_worker.rs # alpm 线程与消息
│   │   │   │   ├── aur.rs           # raur + RPC 解析
│   │   │   │   └── flatpak.rs       # flatpak CLI 封装与解析
│   │   │   ├── flathub.rs           # Flathub v2 API 客户端（元数据/图标/大小）
│   │   │   ├── cache/
│   │   │   │   ├── mod.rs           # Cache：get_or_fetch(ttl) / 原子写 / LRU 索引
│   │   │   │   └── key.rs           # cache key 白名单编码
│   │   │   ├── config.rs            # 配置模型 + 原子读写 + 版本迁移
│   │   │   ├── plan.rs              # 计划构建、风险标注、序列化/校验
│   │   │   ├── net.rs               # reqwest 客户端工厂（代理/超时/UA/重试）
│   │   │   ├── env.rs               # 环境探测（Capability 汇总，供 --doctor 与设置页）
│   │   │   └── i18n.rs              # gettext 初始化与 N_() 标记
│   │   └── tests/                   # 集成测试（见 §11）
│   ├── archstore-gui/               # GTK4 应用
│   │   ├── src/
│   │   │   ├── main.rs              # 启动序列（§2.4）、单实例、--doctor
│   │   │   ├── app.rs               # AdwApplication，全局 AppState
│   │   │   ├── state.rs             # AppState：Arc<ServiceRegistry> + 计划队列 + 事务状态机
│   │   │   ├── window.rs            # AdwApplicationWindow + NavigationSplitView
│   │   │   ├── pages/{home,category,search,installed,updates,detail,settings}.rs
│   │   │   ├── widgets/{package_row,package_detail_view,plan_bar,dep_list,progress_panel,error_view}.rs
│   │   │   └── style.css
│   │   ├── data/
│   │   │   ├── io.github.archstore.ArchStore.desktop
│   │   │   ├── io.github.archstore.ArchStore.metainfo.xml
│   │   │   ├── io.github.archstore.ArchStore.gschema.xml   # 仅窗口几何等非敏感偏好
│   │   │   └── icons/
│   │   └── Cargo.toml
│   └── archstore-helper/            # root 侧执行器（无 GTK）
│       ├── src/main.rs
│       └── Cargo.toml
├── data/
│   ├── io.github.archstore.ArchStore.policy       # polkit action
│   └── archstore-helper                            # (可选) 由 pkexec 直接执行的 wrapper
├── po/                                              # gettext：LINGUAS, zh_CN.po, zh_TW.po, ...
├── i18n/software-names.json                         # 本地软件中文名表（§7.2）
└── packaging/PKGBUILD
```

**依赖方向约束（编译期即可强制）**

```
archstore-gui  ──►  archstore-core  ◄──  archstore-helper
      (GTK)              (无 GUI)            (root, 无 GUI)
```

- `archstore-core` 的依赖里**不得出现** `gtk4`/`libadwaita`。CI 用 `cargo tree` 校验（§11.3）。
- `archstore-helper` 不得依赖 `archstore-gui`；它只依赖 `archstore-core` 里的 `plan`/`model` 模块。
- 两个二进制共享 `TransactionPlan` 的 serde 定义，**这是 GUI 与 root 之间唯一的数据契约**。

### 3.3 数据流示例：搜索 "firefox"

```
用户输入 (防抖 300ms)
  → AppState::search(scope)
      ├─ scope ⊇ 本地：AlpmOp::Search → alpm 线程 → 官方仓库结果（可能数十条）
      ├─ scope ⊇ AUR ：tokio reqwest → https://aur.archlinux.org/rpc?v=5&type=search&by=name-desc&arg=…
      │                 命中缓存则跳过网络（TTL 5min）
      └─ scope ⊇ Flatpak：flatpak search -j（普通用户子进程，30s 超时）
  → 合并去重（按 PackageId）→ 排序（精确匹配 > 流行度 > 名称）
  → 分批投递到主线程（每批 <= 50 条，用 glib::MainContext::spawn_local）
  → GtkListBox 增量 diff 更新（不做全量重建，避免滚动位置丢失）
```

**禁止**：搜索时自动触发 AUR/Flatpak 网络请求却不告知用户。搜索栏右侧提供来源开关（本地/AUR/Flatpak），
默认只勾选"本地 + 已启用的源"，并在结果顶部显示"来自缓存（5 分钟前）"或"正在请求 AUR…"。

### 3.4 单一实例保护与退出

- 启动时对 `$XDG_RUNTIME_DIR/archstore.lock` 取 `flock(LOCK_EX | LOCK_NB)`。
  失败（已有实例）→ 打印提示并以退出码 0 结束，**不**强行启动第二个实例。
- 这样做是因为：两个实例会各自维护 alpm 只读句柄与缓存索引，第二个实例退出时的索引写入
  可能覆盖第一个实例的结果（缓存自愈能兜住，但没必要制造这种竞争）。
- `$XDG_RUNTIME_DIR` 不存在时（极端环境）降级为 `$XDG_CACHE_HOME/archstore/instance.lock`。
- 退出顺序：取消 tokio 任务 → join alpm 线程 → `handle.release()` → flush 缓存索引 → 释放锁。
  任一步超过 3 秒则记录日志后强制退出（缓存索引的原子写保证不会留下半个文件）。

---

## 4. 后端抽象层

### 4.1 统一数据模型（`archstore-core::model`）

```rust
// 示意：字段与类型已按"能编译、能序列化"设计
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PackageSource {
    Official { repo: String },   // core / extra / multilib / 自定义
    Aur,
    Flatpak { remote: String },  // flathub / 其他远程
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackageId {
    pub source: PackageSource,
    /// Official/Aur: 包名；Flatpak: application id（如 org.mozilla.firefox）
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    No,
    /// version: 已安装版本；explicit: 是否用户显式安装（false = 依赖带入）
    Yes { version: String, explicit: bool },
}

#[derive(Debug, Clone)]
pub struct PackageSummary {
    pub id: PackageId,
    pub display_name: String,
    pub summary: String,             // 单行描述（<= 120 字符，超出截断）
    pub version: Option<String>,
    pub installed: Installed,
    pub update: Option<UpdateInfo>,  // 有可用更新时填充
    pub icon: IconRef,               // 见下：不持有已解码位图
    pub popularity: Option<f64>,     // AUR Popularity
    pub votes: Option<u32>,          // AUR NumVotes
    pub out_of_date: bool,           // AUR OutOfDate / Flatpak EOL
}

#[derive(Debug, Clone)]
pub enum IconRef {
    /// 本地图标名（主题图标，如 "firefox"）
    IconName(String),
    /// 本地已缓存的图标文件（下载完成后填入）
    CachedFile(PathBuf),
    /// 远程 URL（详情页懒下载）
    Remote(String),
    Missing,
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current: String,
    pub candidate: String,
    pub download_size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PackageDetail {
    pub summary: PackageSummary,
    pub description: String,               // 长描述（可含 Pango markup，渲染前必须校验）
    pub licenses: Vec<String>,
    pub homepage: Option<String>,
    pub download_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub maintainer: Option<String>,
    pub screenshots: Vec<String>,
    pub rating: Option<f32>,               // 0.0..=5.0
    pub review_count: Option<u32>,
    pub permissions: Vec<String>,          // 仅 Flatpak：文件系统/总线权限
    pub dependencies: Vec<DependencyInfo>, // 见 §8
    pub extra: DetailExtra,                // 后端特有字段的只读展示（键值对列表）
}

#[derive(Debug, Clone, Default)]
pub struct DetailExtra(pub Vec<(String, String)>);
```

**设计要点**

- `IconRef` 只描述"从哪里取图标"，**不在数据模型里持有 `gdk::Texture`**（否则 core 会依赖 GTK）。
  位图生命周期由 `archstore-gui` 的 `IconCache`（`HashMap<PackageId, gdk::Texture>` + 上限）管理。
- `Installed` 用枚举而非 `Option<String>`，避免"已安装但版本未知"这类非法状态的表达困难。
- 所有公共结构体派生 `Clone`；跨线程只传这些**拥有所有权**的数据，不传借用。

### 4.2 后端 Trait

```rust
// 示意：archstore-core::backend
use async_trait::async_trait;

/// 后端能力描述：用于设置页置灰、--doctor 报告与"为什么不能用"提示
#[derive(Debug, Clone)]
pub struct Capability {
    pub available: bool,
    /// 不可用原因（面向用户的中文句子，含修复建议）
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// 只查本地数据（已安装 + 已有缓存），绝不联网
    LocalOnly,
    /// 走该后端的完整搜索（可能联网）
    Full,
}

#[async_trait]
pub trait PackageBackend: Send + Sync {
    fn source_kind(&self) -> &'static str;          // "pacman" | "aur" | "flatpak"
    fn capability(&self) -> &Capability;

    async fn search(&self, query: &str, scope: SearchScope) -> Result<Vec<PackageSummary>, CoreError>;
    async fn info(&self, id: &PackageId) -> Result<PackageDetail, CoreError>;
    async fn installed(&self) -> Result<Vec<PackageSummary>, CoreError>;
    async fn upgradable(&self) -> Result<Vec<PackageSummary>, CoreError>;
    async fn categories(&self) -> Result<Vec<Category>, CoreError>;
    async fn list_category(&self, category: &str, page: Page) -> Result<Vec<PackageSummary>, CoreError>;
    /// 依赖信息（只读、用于展示与计划构建）
    async fn dependencies(&self, id: &PackageId) -> Result<Vec<DependencyInfo>, CoreError>;
    /// 反向依赖（卸载前警告用）
    async fn reverse_dependencies(&self, id: &PackageId) -> Result<Vec<PackageId>, CoreError>;
}

pub struct Page { pub offset: usize, pub limit: usize }
pub struct Category { pub id: String, pub display: String, pub source_kind: &'static str }
```

**为什么 trait 里没有 `build_install_cmd` / `build_remove_cmd`**：v0.1.0 让后端返回 `Vec<Command>`，
这等于把"怎么提权执行"混进了后端层，并且无法序列化成计划文件。本版把执行完全外移到 helper（§5），
后端只负责**只读的事实**（包名、版本、依赖、大小）。计划构建由 `plan.rs` 基于这些事实完成。

### 4.3 Pacman 后端（基于 libalpm）

**实测结论（本机，普通用户 uid=1000）**

```
Alpm::new("/", "/var/lib/pacman")        -> Ok（无需 root）
register_syncdb("core")   -> 297 包
register_syncdb("extra")  -> 14955 包
register_syncdb("multilib") -> 0 包（本机未启用，属正常）
localdb().pkgs()          -> 972 个已安装包，其中 explicit 185
extra.groups()            -> 107 个包组（分类页数据源）
Alpm::new("/", "/nonexistent") -> Err(NotADir)
register_syncdb("does-not-exist") -> **Ok**，pkgs=0，is_valid()=true   ← 陷阱，见下
register_syncdb("Extra")          -> **Ok**，pkgs=0                    ← 大小写不匹配也是"成功"
extra.pkg("不存在")        -> Err(PkgNotFound)
mesa.provides()           -> [(mesa-libgl, 1:26.2.2-1), (libva-driver, None), (opengl-driver, None), …]
handle.release()          -> Ok
```

结论：**只读查询完全不需要 root**，因为 `/var/lib/pacman/{local,sync}` 是 0755 且文件 0644。
GUI 因此可以常驻一个只读句柄进程，无需守护进程或 D-Bus 服务。

**最大的陷阱：注册不存在的同步库不会报错。** `register_syncdb_mut()` 对任何名字都返回 `Ok`，
并给出一个永久为空的 `Db`。如果只靠"注册是否成功"判断"仓库是否可用"，程序会在用户机器上
永久显示"该仓库有 0 个包"而不报任何错误。因此**必须**双条件判定：

```
仓库可用 ⇔ pacman.conf 中存在该段名
           ∧ /var/lib/pacman/sync/<name>.db 存在且非空
           ∧ register_syncdb_mut 返回的 db.pkgs().len() > 0
```

三个条件任一不成立 → 该仓库在 UI 中标注"未同步，请先运行 `pacman -Sy`"（注意：**本程序不代为执行 `-Sy`**，
见 §5.4 的部分升级约束）。`db.is_valid()` 不足够（空库也返回 true）。

```rust
// 示意：pacman_worker.rs —— alpm 句柄只在工作线程内被访问
use alpm::{Alpm, SigLevel};

pub struct AlpmWorker {
    req_tx: std::sync::mpsc::Sender<AlpmReq>,
    handle: std::thread::JoinHandle<()>,
}

impl AlpmWorker {
    /// 启动工作线程；同步库名从 /etc/pacman.conf 解析并做存在性校验（见上）
    pub fn spawn(repos: Vec<String>, resp_sink: RespSink) -> Result<Self, CoreError> { /* … */ }

    fn thread_main(repos: Vec<String>, rx: Receiver<AlpmReq>, sink: RespSink) {
        let mut handle = match Alpm::new("/", "/var/lib/pacman") {
            Ok(h) => h,
            Err(e) => { sink.fatal(CoreError::AlpmInit(e.to_string())); return; }
        };
        for name in &repos {
            // USE_DEFAULT：沿用 pacman.conf 的签名策略；只读查询不会触发下载
            // 注意：返回 Ok 不代表库存在（见上文陷阱），必须再检查 pkgs().len()
            match handle.register_syncdb_mut(name.as_str(), SigLevel::USE_DEFAULT) {
                Ok(db) if db.pkgs().len() > 0 => sink.repo_ready(name),
                Ok(_) => sink.repo_empty(name),          // 未同步 / 名不匹配
                Err(e) => sink.repo_failed(name, e.to_string()),
            }
        }
        while let Ok(req) = rx.recv() {
            sink.send(handle_req(&handle, req));  // 内部投递到 glib MainContext 或测试收集器
        }
    }
}
```

**必须遵守的 libalpm 用法（实测 API，非发明）**

| 需求 | 正确调用 | 备注 |
| --- | --- | --- |
| 打开句柄 | `Alpm::new("/", "/var/lib/pacman")` | 不需要 root；路径不是目录时返回 `NotADir` |
| 注册同步库 | `handle.register_syncdb_mut(name, SigLevel::USE_DEFAULT)` | 需要 `&mut`；**库不存在也返回 Ok**，必须再查 `pkgs().len()` |
| 遍历一个库的包 | `db.pkgs()`（返回 `AlpmList<&Package>`，`.iter()` 迭代） | **没有 `pkg_cache()` 这个方法** |
| 按名精确取包 | `db.pkg("firefox")` → `Result<&Package>` | 精确名；未找到返回 `PkgNotFound` |
| 库内模糊搜索 | `db.search(["firefox"].iter().cloned())` | 参数是 `AsAlpmList<&str>` 的迭代器/列表，**不能直接传 `[&str; 1]` 数组** |
| 版本号比较 | `alpm::vercmp(a, b) -> Ordering` / `ver.as_ver().vercmp(other)` | 实测 `1.10 > 1.9`、`1.0-2 > 1.0-1`、`2.0rc1 < 2.0`；`Ver: Display + Deref<str>` |
| 反查"谁依赖我" | `pkg.required_by()` | 实测 glibc → 705 |
| 虚拟依赖来源 | `pkg.provides()`（含版本） | 实测 mesa 提供 `mesa-libgl 1:26.2.2-1`、`opengl-driver`（无版本） |
| 包组（分类） | `db.groups()` / `db.group(name)` | 实测 extra 有 107 个组 |
| 依赖缺失检查 | `handle.check_deps(pkgs, rem, upgrade, reverse_deps)` | 见下方语义说明 |
| 句柄释放 | `handle.release()` | 正常退出路径必须调用 |

**`check_deps` 语义（实测，容易用错）**

`check_deps(pkgs, rem, upgrade, reverse_deps)` 是 libalpm `alpm_checkdeps` 的封装：

- 判定"安装 pkgs 会缺什么依赖"：`pkgs = [待安装包]`，`rem = []`，`upgrade = []`，`reverse_deps = false`。
  实测对 `extra/firefox` 返回 0（其依赖在本地已全部满足）。
- 判定"删除 rem 会破坏谁"：**必须把待检查的全体已安装包放进 `pkgs`**，`rem = [待删包]`，`reverse_deps = true`。
  实测 `pkgs = localdb().pkgs()`（全体）、`rem = [glibc]` 返回 705 条；若只传 `rem = [glibc]` 而 `pkgs` 为空则返回 0
  （**静默漏报**，这是最危险的误用）。
- 该函数是只读的（不触碰文件系统），可在非 root 下调用。

**同步库名解析**：不要硬编码 `core/extra/multilib`（本机 multilib 为空即为反例）。
从 `/etc/pacman.conf` 中 `[section]` 且非 `options` 的段名解析；解析失败时降级为"探测
`/var/lib/pacman/sync/*.db` 的文件名"。得到名字后按上文三条件判定可用性；若全部不可用，
官方仓库相关入口置灰并显示"未检测到可用的同步数据库，请先运行 `sudo pacman -Sy`"
（**只提示，不代执行**）。

**首次加载性能**：`extra` 有 14955 个包，遍历整库代价可观。
策略：启动时只做"已安装/可更新"（972 + 少量）；分类浏览与搜索走 `db.search` 与 `db.pkg`；
若确需全量列表（分类页首屏），把结果缓存在 **内存**（`OnceCell<Vec<IndexEntry>>`，条目只保留
name/version/desc/size 四个字段），并在后台线程构建，避免每次切页重建。

### 4.4 AUR 后端（基于 raur + AUR RPC v5）

**实测（本机直连）**

- `GET https://aur.archlinux.org/rpc?v=5&type=search&by=name-desc&arg=firefox` → 200，`resultcount=508`
- `GET https://aur.archlinux.org/rpc?v=5&type=info&arg[]=yay` → 200，含
  `Depends=["pacman>6.1","git"]`、`MakeDepends=["go>=1.24"]`、`OptDepends=["sudo","doas"]`、
  `License`、`Maintainer`、`NumVotes`、`Popularity`、`OutOfDate`、`URL`、`Version`

**关键结论：依赖信息可以直接从 RPC `info` 得到，无需下载解析 `.SRCINFO`**，
这删掉了 v0.1.0 中"解析 .SRCINFO"的一整块复杂度。

```rust
// 示意：aur.rs
use raur::{Raur, SearchBy};   // 必须引入 Raur trait，search/info 是 trait 方法

pub struct AurBackend {
    handle: raur::Handle,       // 注意：类型是 Handle（v0.1.0 写的 "Raur 对象" 不存在）
    cache: Arc<Cache>,          // 本项目自己的磁盘缓存，见 §6.1
}

impl AurBackend {
    /// 注入自建 reqwest::Client，使代理/超时/UA 统一生效（raur 8: Handle::new_with_client）
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            handle: raur::Handle::new_with_client(client),
            cache: Arc::new(Cache::default()),
        }
    }

    async fn search_raw(&self, q: &str) -> Result<Vec<raur::Package>, CoreError> {
        self.handle
            .search_by(q, SearchBy::NameDesc)
            .await
            .map_err(|e| CoreError::Network { url: "aur:search".into(), source: e.to_string() })
    }
}
```

**`raur::Package` 字段名（实测 8.0.0，与 AUR RPC 的 PascalCase 名称不同，写错就编译不过）**

| RPC 字段 | Rust 字段 |
| --- | --- |
| `Name` / `Version` / `Description` | `name` / `version` / `description: Option<String>` |
| `NumVotes` / `Popularity` | `num_votes: u32` / `popularity: f64` |
| `OutOfDate` / `Maintainer` | `out_of_date: Option<i64>` / `maintainer: Option<String>` |
| `Depends` / `MakeDepends` / `OptDepends` / `CheckDepends` | `depends` / `make_depends` / `opt_depends` / `check_depends`（都是 `Vec<String>`） |
| `License` / `Keywords` / `Groups` / `Provides` / `Conflicts` / `Replaces` | `license` / `keywords` / `groups` / `provides` / `conflicts` / `replaces` |
| `PackageBase` / `PackageBaseID` / `ID` / `URL` / `URLPath` | `package_base` / `package_base_id` / `id` / `url` / `url_path` |

注意 `description` 与 `url`、`maintainer`、`out_of_date` 是 `Option`，**不要直接 `unwrap`**。


**注意事项**

- `raur` 的 `search`/`info` 是 **trait 方法**（`use raur::Raur;` 必须在作用域内），返回
  `Result<_, Self::Err>`；错误需 `map_err` 转成 `CoreError` 并带上人类可读原因。
- `raur::Cache` 是 `HashSet<ArcPackage>`，**不是持久化缓存**。本项目的磁盘缓存由 `cache::Cache` 负责，
  `raur::Cache` 仅作为进程内的二级去重。
- `raur 8` 的默认 feature 会拉入 `reqwest/native-tls`。本项目显式 `default-features = false,
  features = ["rusttls-ring"]`，以与 §2.2 的 reqwest 保持同一 TLS 实现，避免同一进程里两份 TLS 栈。
- **速率与礼貌**：search/info 串行化（单飞），同一 key 5 分钟内不重复请求；
  对 429/503 做指数退避（1s→2s→4s，最多 3 次），退避期间 UI 明确显示"请求过于频繁，稍后重试"。
- **AUR `info` 上限**：批量查询时按 <= 100 个包名分批。

### 4.5 Flatpak 后端（flatpak CLI）

**实测（本机 flatpak 1.18.2）**

| 命令 | 结果 |
| --- | --- |
| `flatpak search --columns=application,name,description,version,origin firefox` | **失败**：`错误：未知列：origin` |
| `flatpak search --columns=application,name,description,version,remotes firefox` | 成功（列名是 `remotes`） |
| `flatpak search -j --columns=…` | 成功，JSON 数组 |
| `flatpak list --app --columns=application,name,version,origin` | 成功（`list` 才有 `origin`，且支持 `size`/`installation`） |
| `flatpak remote-info flathub org.mozilla.firefox` | 成功，含下载大小/安装大小/运行时/许可证/架构/分支/提交 |
| `flatpak remote-info` 的 JSON | **不存在**（无 `-j`），只能解析文本 |
| `flatpak update --appstream` | 存在，用于刷新远程 appstream |

**最关键的实现细节（否则解析会随机失败）**：`flatpak -j` 的 JSON **键名会被本地化**。
本机中文 locale 下实测输出：

```json
[ { "应用程序_id" : "org.mozilla.firefox", "名称" : "Firefox", "描述" : "…", "版本" : "155.0.1", "远程仓库" : "flathub" } ]
```

因此**必须**固定 `LC_ALL=C`（实测得到稳定的英文键 `application_id` / `name` / `description` /
`version` / `remotes`），并且**按位置（列顺序）而不是按 key 名**解析 `--columns` 输出，
两种策略同时使用以互相校验。

```rust
// 示意：flatpak.rs
fn flatpak_cmd(args: &[&str]) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("flatpak");
    c.env("LC_ALL", "C")
     .env("LANG", "C")
     .env_remove("GIO_EXTRA_MODULES")   // 避免用户环境注入的模块影响输出
     .args(args)
     .stdin(std::process::Stdio::null());
    c
}
```

**解析与容错规则**

1. 所有 flatpak 调用统一 `LC_ALL=C`。
2. `-j` 优先；`remote-info` 无 `-j`，用**逐行 `键：值` 解析**并只提取白名单字段
   （`下载大小`/`Download size` 这类标签在 C locale 下为英文，仍要做"两种语言都试"的兜底）。
3. 输出解析失败时返回 `CoreError::Parse`，携带**原始前 20 行**写入日志，UI 显示"Flatpak 输出格式无法识别"，
   绝不让 `unwrap()` 崩掉页面。
4. 只读查询（search/list/remote-info）以普通用户运行；写操作一律交给 helper（§5.3）。
5. `flatpak list -j --app --columns=…` 的结果用于已安装列表；"可更新"通过对比
   `flatpak remote-info --cached`（或 `flatpak update --no-deploy` 的 dry-run 语义）得到，
   **不使用** `flatpak list --updates`（该 flag 在 1.18 的 `list` 中不存在）。

### 4.6 Flathub 元数据（图标、截图、评分、权限）

**实测可用端点**

| 端点 | 用途 | 实测 |
| --- | --- | --- |
| `GET https://flathub.org/api/v2/appstream/{app_id}` | 名称、分类、项目许可证、图标 URL（`dl.flathub.org/media/icons/128x128/…png`）、截图 URL、关键词 | 200 |
| `GET https://flathub.org/api/v2/summary/{app_id}` | 下载大小 / 安装大小 / runtime / 权限（shared、sockets、filesystems、session-bus…） | 200 |
| `GET https://odrs.gnome.org/1.0/reviews/api/ratings/{app_id}` | 星级分布（`star0..star5`, `total`） | **不稳定**：同一会话内 curl 可用、reqwest 直接请求失败（连接重置，649 ms 内报错）。**必须** 3s 超时 + 静默降级为"无评分"，且不得阻塞详情页渲染 |

**离线优先**：Flathub 会在本地维护 appstream 缓存
（本机 `/var/lib/flatpak/appstream/flathub/x86_64/<hash>/appstream.xml`，**105 MB**）。
**禁止在启动时解析该文件**；仅在用户打开"Flatpak 分类页"且网络不可用时，按需流式解析（用 XML pull parser，
限制在需要的 `<component>` 上），且必须有内存上限保护。

---

## 5. 事务、提权与执行模型

这是本项目**唯一会改变系统**的部分，也是 v0.1.0 最危险的缺陷所在。

### 5.1 计划（TransactionPlan）：GUI 与 root 之间唯一契约

```rust
// 示意：archstore-core::model::plan —— 必须 serde 双向可序列化且向前兼容
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransactionPlan {
    pub schema: u32,                 // 当前 = 1；helper 遇到未知 schema 直接拒绝
    pub created_at: u64,
    pub kind: PlanKind,
    pub items: Vec<PlanItem>,
    /// 用户可读的摘要条目（仅用于展示，helper 不信任）
    pub summary: Vec<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum PlanKind { PacmanSync, PacmanRemove, FlatpakInstall, FlatpakUninstall, FlatpakUpdate }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanItem {
    pub source: PlanSource,          // Official{repo} | Flatpak{remote,installation}
    pub name: String,                // 严格校验：见 validate_name
    pub target_version: Option<String>,
    pub reason: PlanItemReason,      // Explicit | Dependency | BuildDependency
}

/// 包名白名单：拒绝任何可能被解释为选项或路径的内容
pub fn validate_name(name: &str) -> Result<(), CoreError> {
    let ok = !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'@'));
    if ok { Ok(()) } else { Err(CoreError::InvalidName(name.to_string())) }
}
```

**禁止事项（安全红线）**

1. **绝不把用户输入拼进 shell 字符串**，也不使用 `sh -c`。所有子进程用参数数组调用。
2. **绝不通过命令行传递包名列表给 root**。`pkexec` 会把用户提供的参数原样交给程序，
   一旦 helper 逻辑有漏洞就等于 root 命令注入。因此改成"GUI 把计划写成临时文件，
   只把**文件路径**传给 helper"，helper 校验路径在允许目录内、属主为调用者、权限为 0600。
3. **helper 不信任计划内容**：对每个 `PlanItem.name` 重新执行 `validate_name`，并重新向系统确认该包
   存在于对应源（`db.pkg(name)` / AUR RPC / `flatpak remote-info`）。不存在的包 → 拒绝整个计划。
4. **不信任 `reason` 与依赖清单**：helper 自己用 libalpm `check_deps` 重新计算依赖。
5. **不做部分执行**：校验失败 → 拒绝全部，退出码非 0，不执行任何子命令。

### 5.2 提权通道

| 场景 | 通道 | 说明 |
| --- | --- | --- |
| 官方仓库安装/卸载/更新 | `pkexec /usr/lib/archstore/archstore-helper …` | polkit 弹窗，`allow_active=yes`（本机会话活跃用户免密码，与 GNOME Software 行为一致） |
| Flatpak 系统安装/卸载/更新 | 同上，helper 内调用 `flatpak --system …` | 保持"所有 root 操作只经过一个二进制"的审计面 |
| Flatpak 用户级安装 | 不需要提权，GUI 直接以用户身份调用 `flatpak --user …` | 需在计划中明确区分 `installation = "user"` |
| AUR 安装/构建 | **不走 pkexec**，以用户身份调用 `paru` / `yay` | AUR 构建必须在非 root 下进行；助手内部自行调 sudo |

**Helper 接口（`archstore-helper`）**

```
archstore-helper --plan <path> --kind <pacman-sync|pacman-remove|flatpak-install|flatpak-uninstall|flatpak-update>
archstore-helper --version
archstore-helper --self-check          # 只读：验证 polkit action 与自身路径
```

**输出协议**（helper → GUI，逐行 JSON，便于解析且不依赖日志格式）：

```json
{"event":"start","plan_schema":1,"items":3}
{"event":"progress","phase":"download","percent":42,"detail":"firefox-155.0.1-1-x86_64.pkg.tar.zst"}
{"event":"log","level":"info","line":"正在检查密钥环…"}
{"event":"error","code":"LOCKED","message":"数据库被锁定：另一个 pacman 正在运行"}
{"event":"done","status":"ok","installed":3,"failed":0}
```

GUI 侧用 `tokio::process::Command` 读 stdout（`BufReader::lines`），逐行解析并投递到主线程。
**不嵌入 PTY**（v0.1.0 提到 PTY 嵌入）：MVP 不需要交互式输入，因为所有确认都在 GUI 里完成，
helper 调用 `pacman --noconfirm`。需要交互的场景（PGP 密钥导入、AUR 助手的编辑器）由 helper 检测到后
以 `{"event":"needs_tty","hint":"…"}` 结束并提示用户改用终端，而不是伪终端转发的复杂实现。

**polkit action**（`data/io.github.archstore.ArchStore.policy`，示意）

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <vendor>ArchStore</vendor>
  <action id="io.github.archstore.ArchStore.transaction">
    <description>安装、更新或删除软件包</description>
    <message>需要授权以修改系统软件包</message>
    <defaults>
      <allow_any>auth_admin</allow_any>
      <allow_inactive>auth_admin</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
    <annotate key="org.freedesktop.policykit.exec.path">/usr/lib/archstore/archstore-helper</annotate>
    <annotate key="org.freedesktop.policykit.exec.allow_gui">false</annotate>
  </action>
</policyconfig>
```

`allow_gui=false` 是刻意的：helper 永远不接触显示服务器。`allow_active=auth_admin_keep` 让活跃会话用户
在授权一次后可连续操作（否则每步都弹窗，体验不可接受）。

### 5.3 AUR 执行路径

- 检测顺序：`paru` → `yay` → 无（无则 AUR 安装按钮置灰，按钮 tooltip 说明"需要 paru 或 yay"）。
- **参数必须按助手类型分别构造，且每个 `--flag` 都要在实现阶段用 `--help` 实测确认**。
  已实测（本机 `yay 13.0.1 --help`）yay 支持：`--noconfirm`、`--sudoloop`、`--answerclean`、
  `--cleanafter`、`--pgpfetch`、`--devel`、`--rebuild`、`--save`、`--repo/--aur`。
  `paru` 在本机未安装，其参数（如是否存在 `--skipreview`、`--sudoloop` 的同名形式）
  **标记为待验证**，属于阶段 4 的前置任务；在验证完成前，paru 分支只使用两个必然存在的参数
  （`-S` 与 `--noconfirm`），其余一律不加。

  ```
  # yay（已实测参数）
  yay -S --noconfirm --answerclean None --answerdiff None --sudoloop <pkg...>
  yay -Rns --noconfirm <pkg...>
  yay -Syu --noconfirm --devel <pkg...>      # AUR 更新（单独计划，不与官方仓库混批）

  # paru（参数需在阶段 4 用 --help 验证后补充）
  paru -S --noconfirm <pkg...>
  ```

- **参数白名单**：助手参数由程序常量决定，**绝不接受来自 UI 或计划的字符串**。
- **AUR 与官方仓库的更新不能混在一次执行里**：Arch 官方明确不建议部分升级（partial upgrade）。
  因此 `PlanKind::PacmanSync` 若同时包含 AUR 与官方仓库包，必须拆成两个顺序计划，并在 UI 中
  明确警告"同时更新官方与 AUR 包可能导致部分升级"。
- AUR 助手需要用户输入 sudo 密码 → 必须有 TTY。GUI 以普通用户启动助手时：
  优先尝试 `pkexec` 不可用（因为要保留用户身份）；实际操作是**在 GUI 内嵌终端对话框**（`vte4`）或
  提示用户"按回车将在终端中打开"，后者更简单可靠。**MVP 选择**：用户点击"执行"后，GUI 弹出
  "AUR 构建需要在终端中完成"，并提供按钮打开 `x-terminal-emulator -e yay -S …`；
  若无法打开终端，则复制完整命令到剪贴板并提示用户。此决策在 §12 里程碑中显式验收。

### 5.4 事务状态机（必须实现为单一枚举 + 转移函数）

```
Idle → Draft(计划非空) → Confirmed(用户点执行) → Authorized(polkit 通过)
     → Running → { Succeeded | Failed | Cancelled }
任何状态 → Idle（清空队列）
```

**规则**

1. **同一时刻只允许一个事务**。`Running` 期间"执行"按钮禁用，队列只读。
2. 计划入队时冻结一份**不可变快照**；执行时把快照写入临时文件（0700 目录，0600 文件）。
3. 退出应用时若状态为 `Running`：**不杀** helper（杀 pacman 比让它跑完更危险），
   弹出"事务正在进行，关闭窗口会保留后台执行"并提供"最小化到后台"与"等待完成"两个选项。
4. 崩溃恢复：临时目录保留最后一个计划文件（`$XDG_CACHE_HOME/archstore/plans/last.json`）。
   下次启动若发现该文件且**没有**对应的 `done` 记录，UI 顶部显示横幅：
   "上次事务可能未完成，请运行 `checkupdates`/`pacman -Qkk` 检查系统状态"，并可查看该计划内容。
5. 锁冲突：helper 启动前检查 `/var/lib/pacman/db.lck` 是否存在（实测：无操作时不存在）。
   存在则以 `{"event":"error","code":"LOCKED"}` 结束，UI 提示"另一个包管理器正在运行（可能是 pacman 或另一个更新器）"。
6. 卸载保护：任何 `remove` 计划都要先跑 `reverse_dependencies`，非空则在计划栏中用
   `AdwBanner` 显示"删除 X 会影响 N 个已安装包"，并默认把"同时删除这些包"设为**否**。

### 5.5 实时进度与日志

- helper 输出逐行 JSON；GUI 侧 `progress_panel.rs` 显示：总体进度条（按阶段权重估算）、
  当前包名、已下载/总大小、可展开的原始日志（`GtkTextView` + 环形缓冲，最多 5000 行，避免内存膨胀）。
- pacman 的原始输出仍会经过 `{"event":"log"}` 传递，**不做正则强解析**（版本间格式会变），
  只做"包含 → 阶段"的软映射，映射失败就当普通日志显示。
- 事务结束后展示结果摘要：成功/失败条目、耗时、磁盘变化（若可得）。失败时提供"复制错误详情"按钮。

---

## 6. 缓存、配置与网络

### 6.1 缓存设计（自研，替换 v0.1.0 的 sled）

**目录布局**

```
$XDG_CACHE_HOME/archstore/           (0700)
├── meta/<ns>/<key>.json             (0600)  # ns: aur | flatpak | flathub | translate
├── index.json                       # LRU 索引：key -> {size, atime, expires_at}
├── icons/<sha256>.<ext>             # 图标文件，按内容哈希命名（天然去重）
├── plans/last.json                  # 崩溃恢复用（§5.4）
└── archstore.log                    # 日志（滚动，<= 5 MB）
```

**原子写**：`write 到 <key>.json.tmp` → `File::sync_all()` → `rename`（同目录，POSIX 原子）。
读时若 JSON 解析失败 → 删除该条目 → 视为 miss，并记录一次 `warn`。**缓存损坏永远不能导致功能失效。**

**Key 编码**：`cache::key::encode()` 只允许 `[a-z0-9._-]`，其余字节用 `%XX` 转义。
严禁把用户输入或包名直接当作路径片段（防目录穿越）。

**TTL 表（明确数值，避免"看情况"）**

| 数据 | TTL | 过期后的行为 |
| --- | --- | --- |
| AUR 搜索结果 | 5 分钟 | 过期即重新请求；失败时使用过期数据并在 UI 标注"数据为 X 分钟前" |
| AUR 包详情 | 30 分钟 | 同上 |
| Flathub appstream/summary | 6 小时 | 同上 |
| 图标文件 | 30 天 | 过期后仍可用（不阻塞显示），后台刷新 |
| 翻译结果 | 24 小时 | 过期即丢弃（翻译质量随时间与文本变化无关，缓存只为省请求） |
| 已安装/可更新列表 | 不缓存 | 每次查询本地库（毫秒级），保证状态永远真实 |

**容量控制**：默认上限 500 MB（可在设置页调整）。
`index.json` 记录条目大小与最后访问时间；超限时按 LRU 删除到 80%，**icon 目录优先保留**。
`index.json` 与真实文件不一致时以文件系统为准重建索引（自愈）。

**并发**：多个请求命中同一 key 时用 `tokio::sync::Mutex` per-key 合并（single-flight），避免缓存击穿。

### 6.2 网络层（`net.rs`）

```rust
// 示意
pub fn build_client(cfg: &NetworkConfig) -> Result<reqwest::Client, CoreError> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("ArchStore/", env!("CARGO_PKG_VERSION"), " (+https://github.com/koiflan-514/ArchStore)"))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(4);
    if cfg.proxy_enabled {
        let url = match cfg.proxy_type {
            ProxyType::Http   => format!("http://{}", cfg.proxy_url),
            ProxyType::Socks5 => format!("socks5h://{}", cfg.proxy_url),  // socks5h：DNS 也走代理
        };
        b = b.proxy(reqwest::Proxy::all(&url).map_err(|e| CoreError::Config(e.to_string()))?);
    }
    b.build().map_err(|e| CoreError::Config(e.to_string()))
}
```

**规则**

1. **代理只影响网络**，不影响 libalpm 与 flatpak（它们各有自己的机制）。
2. 所有请求必须有超时；UI 侧显示"请求中"，并允许用户取消（`CancellationToken` 中断等待，
   即使底层请求仍需时间完成，UI 立即回到可用状态）。
3. 重试策略：仅对幂等的 GET，且仅对网络错误/5xx/429 重试，最多 3 次，指数退避 + 抖动。
   **4xx（除 429）不重试**。
4. 图标下载：并发上限 4，单文件上限 5 MB（超出则丢弃并使用占位图标），
   下载完成后按内容哈希落盘并在内存 `IconCache` 中缓存 `gdk::Texture`（上限 200 个 / 64 MB）。
5. 尊重系统代理：reqwest 开启 `system-proxy` feature，让未显式配置时也能跟随环境变量。

### 6.3 配置（`config.rs`）

**路径**：`$XDG_CONFIG_HOME/archstore/config.toml`，权限 0600（默认不创建，首次保存时创建）。

```toml
# config.toml —— 版本化，便于未来迁移
schema = 1

[appearance]
color_scheme = "system"    # system | light | dark
icon_size = "medium"       # small | medium | large
animations = true

[network]
proxy_enabled = false
proxy_type = "http"        # http | socks5
proxy_url = ""
timeout_secs = 30
# 可选：额外的 AUR 镜像 RPC 端点（留空使用官方）
aur_rpc_url = ""

[sources]
pacman_enabled = true
aur_enabled = true
aur_helper = "auto"        # auto | paru | yay | none
flatpak_enabled = true
flatpak_remote = "flathub"
flatpak_installation = "system"   # system | user

[cache]
max_size_mb = 500
aur_search_ttl_minutes = 5
aur_info_ttl_minutes = 30
flathub_ttl_hours = 6

[translation]
# 默认关闭：开启后软件描述会发送到第三方服务（隐私影响见 §7.2）
auto_translate = false
api = "none"               # none | libretranslate
api_endpoint = ""

[update]
check_on_startup = true
check_interval_hours = 6

[ui]
window_width = 1100
window_height = 720
```

**读写规则**

1. 读取：文件不存在 → 使用默认值并**不**写盘；TOML 解析失败 → 备份为 `config.toml.bak.<ts>`，
   使用默认值，并以 `AdwToast` 告知"配置已损坏，已重置"（**绝不静默覆盖用户配置**）。
2. 写入：原子替换（tmp + rename）；写入前与当前磁盘内容做一次字段级合并，
   避免两个窗口互相覆盖。
3. 未知字段保留（`#[serde(flatten)] extra: toml::Table`），保证版本回退不丢配置。
4. `schema` 高于本程序支持值时，**只读模式**启动并提示升级应用。

---

## 7. 界面设计

### 7.1 布局与导航

```
┌───────────────────────────────────────────────────────────────┐
│ 🔍 搜索软件…                     ⟳ 刷新   ⬆ 更新(3)   ⚙ 设置   │  HeaderBar
├──────────────┬────────────────────────────────────────────────┤
│ 发现          │                                                │
│  · 首页       │    [ 内容区：AdwViewStack 的一个页面 ]           │
│  · 分类       │                                                │
│  · AUR 社区   │                                                │
│  · Flatpak    │                                                │
│ 我的          │                                                │
│  · 已安装 972 │                                                │
│  · 可更新 3   │                                                │
├──────────────┴────────────────────────────────────────────────┤
│ 计划栏：安装 firefox、GIMP（2 项，下载 92 MB） [查看详情][执行]   │  AdwBin
└───────────────────────────────────────────────────────────────┘
```

- 容器：`AdwApplicationWindow` → `AdwToastOverlay` → `AdwNavigationSplitView`（侧栏 `AdwNavigationPage`）。
- 窄窗口（< 600sp）自动折叠为 `AdwFlap`/导航切换（用 `AdwBreakpoint` 声明，不手写尺寸判断）。
- 列表用 `GtkListView` + `gio::ListStore`（**不用 `GtkFlowBox`**：FlowBox 在千级条目下性能不可接受）。
  列表项是 `GtkListItemFactory` 生成的 `widgets::package_row`。
- **图标加载**：行内先用 `gtk::Image::from_icon_name("application-x-executable")` 占位；
  详情页用 `gtk::Picture::set_file(Some(&gio::File::new_for_path(p)))` 展示本地缓存图标，
  远程 URL 先下载到缓存再展示（`Picture` 没有"从 URL 异步加载"的 API，v0.1.0 的
  `set_icon_name` / `set_from_file_async` 在 GTK4 中不存在）。
- **图标来源与回填（v0.1.0 实测补充）**：优先级为
  AppStream 数据包文件 → 已安装包 `.desktop` 的 `Icon=` → Flatpak/Flathub 图标 URL
  → 主题同名图标 → 字母头像。远程图标下载完成后必须**原地回填**已渲染的行
  （`RowContext::slots` 弱引用槽位）—— `GtkListView` 的 factory 只在 bind 时创建控件，
  只写内存缓存不会触发重绘。覆盖率与硬限制见 README「软件图标从哪里来」。
- 键盘：`Ctrl+F` 搜索、`Ctrl+Q` 退出、`Ctrl+W` 关窗、`Ctrl+,` 设置、`F5` 刷新、`Esc` 清空搜索。

### 7.2 中文与国际化（分三层，且诚实标注来源）

| 优先级 | 来源 | 说明 | 是否默认开启 |
| --- | --- | --- | --- |
| 1 | AppStream 内嵌翻译 | Flatpak 应用的名称/描述随 locale 变化（Flathub 元数据在中文 locale 下实测已返回中文，如"网易云音乐"） | 是（自动） |
| 2 | 内置软件名表 `i18n/software-names.json` | 只做**名称**映射（如 `firefox → 火狐浏览器`），随应用分发，离线可用 | 是 |
| 3 | 界面文案 | gettext `po/zh_CN.po`，全部 UI 字符串通过 `gettext()` 获取 | 是 |
| 4 | 在线翻译描述 | 调用用户配置的 LibreTranslate 兼容端点 | **否**（需用户在设置页显式开启，并展示隐私说明） |

**关于第 4 层的明确取舍**：v0.1.0 把"在线翻译软件描述"当作核心卖点，但它的成本（额外的失败面、
第三方隐私暴露、翻译质量不可控）远高于收益。本版降级为可选功能，并规定：

1. 默认关闭；启用时必须显示"软件描述将发送至 <endpoint>，可能包含包名与描述文本"。
2. 译文**必须**在 UI 上标注"机器翻译"，绝不冒充上游元数据。
3. 译文缓存 24 小时（§6.1）；翻译失败静默回退原文，不弹错误。
4. 一次只翻译可见条目的描述（详情页），**禁止**批量翻译列表页。

**界面 i18n 机制**

```rust
// 示意：i18n.rs
pub fn init() {
    let _ = gettextrs::bindtextdomain("archstore", "/usr/share/locale");
    let _ = gettextrs::bind_textdomain_codeset("archstore", "UTF-8");
    let _ = gettextrs::textdomain("archstore");
}
/// 标记可翻译字符串（用于抽取 POT），返回值在运行期会被 gettext 替换
pub fn t(s: &str) -> String { gettextrs::gettext(s) }
```

POT 抽取在 Makefile 中固定为：

```
xgettext --from-code=UTF-8 -o po/archstore.pot $(shell find crates -name '*.rs')
msgmerge --update po/zh_CN.po po/archstore.pot
msgfmt --check -o /dev/null po/zh_CN.po     # CI 必须跑，检查格式错误与未翻译占位符
```

### 7.3 页面清单与空/错/加载三态

每个页面都必须实现三种非正常状态（这是"稳定可靠"最直观的体现）：

| 页面 | 加载态 | 空态 | 错态 |
| --- | --- | --- | --- |
| 首页 | 骨架屏（AdwSkeleton 风格占位行） | "暂无推荐内容" | 显示 `ErrorView`（原因 + 重试 + 打开设置） |
| 分类 | 分页 spinner 在列表尾部 | "该分类暂无软件" | 同上 |
| 搜索 | 顶部细进度条（不阻塞输入） | "没有找到与 X 匹配的软件"，附"搜索 AUR"按钮 | 分后端显示："AUR 请求失败（超时）"，其他源结果照常显示 |
| 已安装 | 首次 200ms 内显示 spinner | 不应为空（至少有 base 包）；若为空则提示系统数据库异常 | 显示"无法读取本地数据库"+ 诊断按钮 |
| 可更新 | 同上 | "系统已是最新" ✅ | 同已安装 |
| 详情 | 摘要先显示（本地数据），详情字段逐块填充 | — | 缺失字段显示"不可用"，而不是整页失败 |
| 设置 | — | — | 每个不可用后端显示原因与"重新检测"按钮 |

### 7.4 软件详情页结构

```
[图标 128]  Firefox                                     [安装 / 已安装 ▾ / 更新]
            155.0.1-1 · 官方仓库 extra · 88.2 MB 下载 / 309.9 MB 安装
            MPL-2.0 · 维护者 Mozilla · 主页 ↗
────────────────────────────────────────────────────────────────
[ 截图横向滚动（每张单击查看大图） ]
[ 描述（可展开，支持 Pango markup，渲染前必须 gtk::pango::parse_markup 校验） ]
[ 权限（仅 Flatpak）]
[ 依赖 (24) ▾ ]   ← 可展开列表，见 §8
[ 详情 ] 包名 / 版本历史（若可得）/ 安装日期 / 原因（显式 or 依赖）
```

**演示图片必须能放大看**：截图槽位外面包一层无边框 `GtkButton`
（hover 高亮 + 键盘可达），单击打开独立的大图查看器
（`widgets/image_viewer.rs`：`←`/`→` 切换、`Esc` 关闭、标题显示 `N / M`）。
查看器读的是详情页**已经下载到本地缓存**的那份文件（`screenshot_file(i)`），
不重新发起网络请求；尚未下载完的槽位显示"图片尚未下载完成"的占位提示。

### 7.5 CSS（`style.css`）

只定义确实需要的类，颜色一律使用 libadwaita 提供的命名色（不硬编码 hex），保证深浅色自适应：

```css
.package-row        { padding: 8px 12px; }
.package-row:hover  { background-color: alpha(@accent_bg_color, 0.08); border-radius: 8px; }
.source-badge       { font-size: 0.75em; padding: 1px 8px; border-radius: 9999px; }
.source-badge.official { background-color: alpha(@accent_bg_color, 0.20); color: @accent_fg_color; }
.source-badge.aur      { background-color: alpha(@warning_bg_color, 0.25); }
.source-badge.flatpak  { background-color: alpha(@success_bg_color, 0.25); }
.state-pill.installed  { color: @success_color; }
.state-pill.upgradable { color: @accent_color; }
.state-pill.orphan     { color: @warning_color; }
.dep-missing        { color: @error_color; }
.dep-build          { opacity: 0.75; }
.screenshot-button:hover .screenshot { background-color: alpha(@accent_bg_color, 0.25); }
.screenshot-viewer  { background-color: alpha(black, 0.92); }   /* 大图查看器 */
```

CSS 通过 `gtk::CssProvider` 加载并 `StyleContext::add_provider_for_display`，
在**开发模式**（`ARCHSTORE_DEV=1`）下监听文件变化热重载；发布模式不监听。

---

## 8. 依赖解析与展示

### 8.1 三个后端的依赖来源

| 后端 | 来源 | 是否网络 | 备注 |
| --- | --- | --- | --- |
| pacman | `pkg.depends()/makedepends()/optdepends()/checkdepends()` + `handle.check_deps()` | 否 | 已安装状态由本地库判断 |
| AUR | RPC `info` 的 `Depends`/`MakeDepends`/`OptDepends` | 是（30 分钟缓存） | 不需要 `.SRCINFO` |
| Flatpak | Flathub `summary` 的 `metadata.runtime`/`extensions` | 是（6 小时缓存） | 依赖主要是 runtime 与扩展 |

```rust
// 示意
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepKind { Runtime, Make, Check, Optional, RuntimeRef, Extension }

#[derive(Debug, Clone)]
pub struct DependencyInfo {
    pub name: String,             // 原始依赖表达式，如 "gtk3>=3.24"
    pub kind: DepKind,
    pub description: Option<String>,   // optdepends 的用途说明
    pub satisfied_by: Option<PackageId>, // 已满足时指向提供者（含 virtual provides）
    pub missing: bool,
    pub size: Option<u64>,        // 需要下载时的体积
    pub recommended: bool,        // 可选依赖中的"推荐"项（源里标注）
}
```

### 8.2 展示规则（依赖弹窗）

```
⚠️  安装 Firefox 需要处理以下依赖
   运行时依赖 (3)          ├ gtk3   76.2 MB  [已安装]
                          ├ nss     2.1 MB  [已安装]
                          └ libvpx  1.8 MB  [将安装]  ← 高亮
   构建依赖 (2) 仅构建期     ├ rust    312 MB  [将安装]  ← 灰底 + 提示"构建后可移除"
                          └ cargo   8.4 MB  [将安装]
   可选依赖 (1)             └ ffmpeg — 视频解码支持  [不勾选]  ← 默认不勾选
   虚拟依赖冲突             libgl 可由以下提供：○ mesa（推荐）  ○ nvidia-utils
                                              [取消]  [仅安装主包]  [加入计划]
```

**硬性规则**

1. **可选依赖默认不勾选**，勾选后其体积计入总计。
2. 构建依赖（AUR 的 `makedepends`）用不同样式标注，并提供"构建后自动清理"开关
   （对应 `yay --cleanafter`，仅在该助手支持时展示）。
3. 虚拟依赖（多个包 `provides` 同一名字）必须让用户选择；默认选 `base-devel`/官方仓库优先的候选，
   选择结果写入计划（`PlanItemReason::Dependency` 的具体包名）。
4. **弹窗是只读展示 + 选择**，它不执行任何操作；"加入计划"只是把条目放进计划栏。
5. 依赖无法解析完整（如 AUR 依赖本身未收录、网络失败）时，**明确显示"依赖信息不完整"**，
   而不是假装没有依赖。

---

## 9. 本地软件管理与更新

### 9.1 已安装页

- 数据源：`localdb().pkgs()`（实测 972 个包）**加上 Flatpak 已安装应用**
  （`flatpak list --app`）。两者按包名合并去重，去重键是 **(来源, 包名)**：
  Flatpak 应用 ID 与 pacman 包名不是同一命名空间。
- Flatpak 侧读取失败只降级为"没有这一部分"（`tracing::warn` + 其余照常显示），
  不把整页变成错误页；**已安装快照仍然只收 pacman 侧**，因为它是 AUR 后端判断
  "是否已安装"的依据。
- 列：名称、版本、来源、安装大小（`pkg.isize()`）、安装日期（`pkg.install_date()`）、
  状态药丸（显式安装 / 依赖 / 可更新 / 孤儿 / 外部包）。
- 筛选：来源（仓库 / AUR 外来包 / Flatpak）、状态、搜索框（本地过滤，不联网）。
- **AUR 外来包识别**：`pkg.db()` 为本地库且不在任何同步库中 → 标记为"外来包（可能来自 AUR）"。
- 孤儿包：安装原因为 `Dependency` 且 `required_by()` 为空（实测 glibc 有 705 个反向依赖，
  这在千级包量下是廉价操作）。
- 卸载：走 §5.4 的 `remove` 计划（先反依赖检查）。

### 9.2 更新页

- 数据源：本地库 vs 同步库比对（`Version::vercmp`），**不使用 `checkupdates`**（它需要下载临时库，
  会重复联网且与 pacman 锁交互）。
- Flatpak 可更新：`flatpak remote-info --cached` 与已安装版本比对；结果缓存 30 分钟。
- 分组展示：安全更新（与 Arch 安全公告匹配）/ 普通更新 / AUR 更新（需要助手）/ Flatpak 更新。
- 安全公告：`https://security.archlinux.org/issues/all.json`（实测 308 → 重定向到
  `https://security.archlinux.org/issues/all.json`，需跟随重定向）。该请求**仅在更新页可见时**
  发起一次，结果缓存 6 小时；字段示例：`{"name":"AVG-2843","packages":["vim"],"status":"Unknown",
  "severity":"Unknown","affected":"9.0.1224-1","fixed":"9.0.1225-1","issues":["CVE-2023-0433"]}`。
  匹配规则：`packages` 包含包名 **且** 当前版本落在 `affected`..`fixed` 区间。
- 一键更新：把全部可更新项加入计划（仍走审查栏，不直接执行）。
- 部分升级警告：官方仓库包必须**整批**更新，禁止让用户只勾选其中几个（UI 上官方仓库分组为原子组）。

### 9.3 卸载

```rust
// 示意：构建删除计划（纯数据，不执行）
pub fn build_remove_plan(
    backend: &dyn PackageBackend,
    targets: &[PackageId],
    cascade: bool,
) -> Result<TransactionPlan, CoreError> {
    // 1) 反依赖检查（非 root，只读）
    // 2) 有反依赖且 !cascade → 返回 Err(CoreError::ReverseDeps{..})，由 UI 询问用户
    // 3) 生成 PlanKind::PacmanRemove / FlatpakUninstall
    // 4) 计算"多余依赖"（见下）并逐条加入计划
    // 5) helper 会再次独立校验并自行计算依赖
}
```

默认使用 `pacman -Rns` 语义（同时清理不再需要的依赖）；但**清理范围必须展示在计划里**，
不能像命令行那样隐式省略。因此计划项分三类（`PlanItemReason`）：

| 摘要文案 | reason | 含义 |
| --- | --- | --- |
| `卸载 X` | `explicit` | 用户选中的目标包 |
| `连带卸载 Y` | `dependency` | 用户确认级联删除的反向依赖 |
| `清理多余依赖 Z` | `unneeded` | 删掉之后不再被任何已安装包需要的依赖 |

"多余依赖"由 `PackageBackend::unneeded_dependencies(&removing)` 计算
（pacman 后端实现为 `AlpmOp::Unneeded`，只读）：

1. 从**完整删除集合**（目标 + 级联包）出发，只沿 `pkg.depends()`（运行时依赖）展开；
   版本约束与虚拟 provides 都走 `find_satisfier`，不按名字字符串猜。
2. 候选必须 `reason == Dependency`：**显式安装的包永远不自动删除**。
3. 候选的 `required_by()` 必须全部落在删除集合内（`required_by` 已计入 provides）。
4. 对命中的包继续展开（传递闭包），直到不动点；结果按包名排序。

必须一次性传入完整删除集合：两个待删包**共同依赖**的包不是多余依赖。
实机验证：`remove-cascade gst-plugins-good` → 41 项（1 显式 + 3 连带 + 37 多余依赖）。

卸载计划的 `PlanSource::Official` 允许**空仓库名**：`pacman -Rns` 不按仓库解析，
仓库名只是展示信息（级联删到的外来包在同步库里本就没有归属）；安装计划仍必须指明仓库。
UI 侧的级联重试必须复用用户点击时的原始 `PackageId`，否则会丢掉仓库名。

---

## 10. 设置页

用 `AdwPreferencesPage` + `AdwPreferencesGroup`，与 §6.3 的配置字段一一对应：

| 分组 | 项 | 控件 | 特殊要求 |
| --- | --- | --- | --- |
| 外观 | 配色方案 | `AdwComboRow` | 直接驱动 `AdwStyleManager::set_color_scheme` |
| 外观 | 图标大小 / 动画 | `AdwComboRow` / `AdwSwitchRow` | 动画关闭时列表不使用过渡 |
| 网络 | 代理开关/类型/地址 | Switch + Combo + Entry | 地址校验；"测试连接"按钮请求 AUR RPC 的 `type=search&arg=` 空查询 |
| 软件源 | pacman / AUR / Flatpak 开关 | Switch | 后端 `Capability::available == false` 时置灰并显示 `reason` |
| 软件源 | AUR 助手 | Combo（自动/paru/yay/无） | 选项只列出实际检测到的助手 |
| 软件源 | Flatpak 远程与安装位置 | Combo | 从 `flatpak remotes --columns=name,installation` 读取 |
| 缓存 | 当前大小 / 上限 / 清除 | 只读行 + Spin + Button | 大小按目录实际统计；清除有确认对话框 |
| 翻译 | 自动翻译 / API / 端点 | Switch + Combo + Entry | 开启时显示隐私说明（§7.2） |
| 更新 | 启动检查 / 间隔 | Switch + Spin | 间隔最小 1 小时 |
| 诊断 | 运行环境自检 | Button（打开 `--doctor` 结果窗口） | 复用 §2.3 输出 |
| 关于 | 版本 / 许可 / 链接 | 只读 | 显示 GPL-3.0-or-later、源码与问题反馈链接 |

**设置项生效时机**：外观/主题立即生效；代理与超时**立即重建** `reqwest::Client`（旧请求保留但不再复用）；
缓存上限立即触发一次裁剪；软件源开关立即刷新侧栏与类别列表。

---

## 11. 质量保障

### 11.1 稳定性硬性要求（Code Review 检查项）

1. **库代码中禁止 `unwrap()` / `expect()` / `panic!()`**（`archstore-core` 与 `archstore-helper` 全部禁止；
   GUI 中仅允许在"初始化前"的少数位置使用，并需注释说明为何不可能失败）。CI 用
   `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` 强制。
2. **禁止在主线程做 IO**：`archstore-gui` 中不允许出现 `std::fs`、`Command::output`、
   `reqwest::blocking`。用 CI 的 `grep` 白名单检查（只允许 `pages/settings.rs` 通过 core 提供的异步接口）。
3. **每个后端都要有 capability 探测**，不可用时置灰 + 说明原因，而不是返回空列表。
4. **所有外部输入都要校验**：包名（§5.1 白名单）、URL scheme（只允许 https，`dl.flathub.org`/
   `flathub.org`/`aur.archlinux.org` 之外需用户确认）、Pango markup（先 `parse_markup`）、
   文件路径（缓存 key 编码）。
5. **优雅退出**：`AdwApplication::shutdown` 时（a）取消 tokio 任务，（b）join alpm 线程，
   （c）flush 缓存索引。超时 3 秒后强制退出，但要保证索引文件不处于"写了一半"状态（原子写已保证）。
6. **panic hook**：捕获 panic → 写入 `$XDG_CACHE_HOME/archstore/archstore.log` 并弹出
   "发生内部错误"对话框（含日志路径），绝不静默退出。

### 11.2 测试策略

| 层级 | 范围 | 手段 |
| --- | --- | --- |
| 单元测试 | 缓存（TTL/原子写/损坏自愈/LRU）、cache key 编码、`validate_name`、依赖表达式解析与版本比较、pacman.conf 解析、plan 的 serde 往返与向前兼容 | `cargo test`，无外部依赖 |
| 契约测试 | AUR RPC 响应 → `PackageSummary` 映射（用真实抓取的 JSON 固件）；`flatpak -j` 输出 → 解析（中文 locale 与 C locale 两份固件） | 固件放 `crates/archstore-core/tests/fixtures/` |
| 集成测试（只读） | `AlpmWorker` 在本机打开数据库、搜索、`required_by`、`check_deps` 反依赖；断言"非 root 可用" | 需要目标机为 Arch；非 Arch 环境自动 `#[ignore]` |
| 集成测试（写，手动） | 计划文件 → helper 校验逻辑（**用 `--dry-run` 模式**：只校验不执行） | helper 必须实现 `--dry-run` |
| UI 冒烟 | `xvfb-run`/Wayland 下启动 → 构造主要页面 → 断言无 GTK critical 警告 | 已在本机验证 `GDK_BACKEND=wayland` 下可无头运行 |
| 无回归 | 每次提交跑 `cargo clippy -- -D warnings` + `cargo fmt --check` + `msgfmt --check` | CI |

### 11.3 CI 检查清单（GitHub Actions 或本地 `make check`）

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo tree -p archstore-core | grep -q gtk4 && exit 1    # 依赖方向约束
msgfmt --check po/*.po
xgettext 抽取后与 po/archstore.pot 比对，若有新增未翻译字符串则警告（不阻塞）
```

---

## 12. 开发路线图（含退出标准）

每个阶段必须有**可演示的产物**与**可验证的退出标准**。禁止"完成 X 功能"这类不可验证的表述。

### 阶段 0：环境与骨架（1 周）

- workspace + 三个 crate 骨架；`--doctor` 可用；`--version` 可用。
- 单实例保护（`flock` 于 `$XDG_RUNTIME_DIR/archstore.lock`，§3.4）。
- gtk4/libadwaita 运行期版本校验 + 失败对话框。
- 日志与 panic hook。
- **风险前置验证（本阶段必须完成，不可推迟）**：写一个一次性 PoC 二进制，把三条最不确定的路径各跑通一次：
  (a) 用 `gtk4 0.11` + `libadwaita 0.9` 开一个带 `AdwNavigationSplitView` 的窗口并显示一个列表；
  (b) 用 `alpm 5` 以普通用户打开 `/var/lib/pacman`、遍历 `extra`、调用 `check_deps`；
  (c) 用 `flatpak 1.18`（`LC_ALL=C`）跑 `search -j` 与 `list -j` 并解析成结构体。
  这三项**已在本机验证通过**（详见 §4.3/§4.5/附录 A），PoC 的目的是确认在**项目自身的构建环境**里
  版本解析与 feature 组合一致，避免在阶段 2/3 才发现依赖冲突。
- **退出标准**：`archstore --doctor` 在本机输出全绿/合理的 WARN；`cargo run -p archstore-gui`
  能开窗并显示空白页；窗口无 GTK critical；上述三条 PoC 全绿。

### 阶段 1：pacman 只读全链路（2 周）

- `AlpmWorker` + 同步库名解析 + `Capability` 探测。
- 已安装页、可更新页、官方仓库搜索与分类浏览、详情页（本地字段）。
- `GtkListView` + `ListStore` 的增量渲染与图标占位。
- 失败态 UI（`ErrorView`）。
- **退出标准**：断网状态下能列出 972 个已安装包、按名称搜索、打开详情；主线程在任一操作中
  不出现 > 50 ms 的阻塞（用 `tracing` 打点验证）；`extra` 首次遍历 < 300 ms。

### 阶段 2：网络后端（2 周）

- **第一件事**：TLS/连通性冒烟测试（同一个 `reqwest::Client` 打通 Flathub + AUR，§2.2）。
  同时验证 `raur` 的 feature 组合（`default-features = false` + `["async","rusttls-ring"]`）能拿到
  `raur::Handle` 与 `raur::Raur`，以及 `raur::Package` 的字段名与 §4.4 映射表一致。
- `cache::Cache`（TTL/原子写/LRU/自愈）+ single-flight。
- AUR 后端（搜索/详情/依赖）+ Flathub 元数据 + Flatpak 后端（list/search/remote-info）。
- 代理与超时；请求取消；失败降级。
- 图标下载与 `IconCache`。
- **退出标准**：搜索 "firefox" 同时返回官方 + AUR（+ Flatpak）结果；断网时显示缓存数据并标注时间；
  杀掉网络后 UI 不卡、无 panic；缓存目录在 `kill -9` 后仍可读（原子写验证）；
  `flatpak` 相关解析在 `LC_ALL=C` 与中文 locale 两种环境下都通过固件测试。

### 阶段 3：事务与提权（3 周，风险最高）

- `TransactionPlan` + 计划栏 + 依赖弹窗（含虚拟依赖选择）。
- `archstore-helper` + polkit action + `--dry-run` 校验。
- 进度面板（JSON 事件流）+ 锁冲突处理 + 崩溃恢复横幅。
- Flatpak 事务（系统与用户两种安装位置）。
- **退出标准**：在**测试机/虚拟机**上完成：安装一个官方仓库小包（如 `sl`）→ 卸载 →
  在计划文件中手工注入非法包名与路径穿越尝试，helper 必须拒绝；
  并发触发第二个事务必须被状态机挡住；helper 被 `kill -9` 后 GUI 显示失败并给出诊断。

### 阶段 4：AUR 执行与体验完善（2 周）

- AUR 执行路径（终端打开策略，§5.3）+ 部分升级警告。
- 设置页全部项 + 配置原子写与损坏恢复。
- 中文界面（PO 文件）+ 软件名表 + 可选在线翻译。
- 主题适配（浅/深/系统）、快捷键、空/错/加载三态补齐。
- **退出标准**：在测试机上用 `yay` 安装一个纯 AUR 小包（如 `cowsay`）成功；
  设置页每项改动重启后保持；`LANG=zh_CN.UTF-8` 下界面完整中文，无 `??` 或英文残留（除专有名词）。

### 阶段 5：发布（1 周）

- `.desktop`、metainfo、polkit policy、图标、`PKGBUILD`（含 helper 安装到 `/usr/lib/archstore/`）。
- AUR 发布 `archstore` 与 `archstore-git`。
- README（安装、首次运行、权限说明、隐私说明）、GPL-3.0 全文。
- 性能回归：千级列表滚动 60 fps（`GSK_RENDERER=gl` 与 `cairo` 各测一次）、常驻内存 < 200 MB。
- **退出标准**：全新 Arch 虚拟机 `makepkg -si` 安装后，从启动到完成一次安装事务全程无手动依赖安装、
  无错误输出。

---

## 13. 许可证

本项目采用 **GPL-3.0-or-later**。

```
ArchStore — Arch Linux Software Store
Copyright (C) 2026 ArchStore Contributors

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <https://www.gnu.org/licenses/>.
```

**依赖许可兼容性（已核对 crates.io 元数据）**

| 依赖 | 许可 | 与本项目的关系 |
| --- | --- | --- |
| `alpm` / `alpm-sys` | **GPL-3.0** | 静态链接，要求整体以 GPL-3.0(-or-later) 分发 → 与本项目选择一致，**无需例外** |
| `raur` | MPL-2.0 | 文件级 copyleft，不传染；保留其许可声明 |
| gtk4 / libadwaita 绑定 | MIT | 兼容 |
| libgtk-4 / libadwaita（系统库） | LGPL-2.1+ | 动态链接，兼容 GPL |
| `reqwest` / `rustls` / `tokio` / `serde` | MIT / Apache-2.0 | 兼容 |
| 参考项目 Aurora / PacHub | GPL-3.0 | 仅借鉴交互设计，**不复制其代码**；若引用代码必须保留版权与作者 |

**注意**：`alpm` 是 GPL-3.0（非 "-or-later"）。若未来需要改为更宽松许可，必须替换该依赖
（例如改为调用 `pacman` CLI）。当前选择 GPL-3.0-or-later 是兼容的，但**发布时必须在 README
与 about 页明确声明包含 GPL-3.0 的静态链接组件**。

---

## 附录 A：外部接口参考（均已实测）

### A.1 AUR RPC v5

```
# 搜索（默认按 name-desc）
GET https://aur.archlinux.org/rpc?v=5&type=search&by=name-desc&arg=firefox
  实测：200，resultcount = 508

# 批量信息（含依赖！）
GET https://aur.archlinux.org/rpc?v=5&type=info&arg[]=yay
  实测字段：Name, Version, Description, NumVotes, Popularity, OutOfDate, Maintainer,
           URL, URLPath, License[], Depends[], MakeDepends[], OptDepends[], Keywords[],
           FirstSubmitted, LastModified, PackageBase, PackageBaseID, Submitter, ID
```

`by` 可选值：`name`、`name-desc`、`maintainer`、`depends`、`makedepends`、`optdepends`、
`checkdepends`、`provides`、`conflicts`、`replaces`、`groups`、`submitter`、`keywords`、`comaintainers`。

约束：批量 `info` 建议 <= 100 个名字/次；无认证，需遵守速率礼貌（见 §4.4）。

### A.2 Flatpak CLI（1.18.2 实测）

```bash
# 搜索（列名是 remotes，不是 origin；必须 LC_ALL=C 以稳定 JSON 键名）
LC_ALL=C flatpak search -j --columns=application,name,description,version,remotes firefox

# 已安装
LC_ALL=C flatpak list --app -j --columns=application,name,version,origin,size
LC_ALL=C flatpak list --app --columns=application,name,version,origin,size

# 远程信息（无 -j，只能解析文本）
LC_ALL=C flatpak remote-info flathub org.mozilla.firefox
LC_ALL=C flatpak remote-info --show-runtime --show-sdk flathub org.mozilla.firefox

# 刷新远程 appstream
flatpak update --appstream

# 写操作（由 helper 以 root 执行；用户级安装由 GUI 以用户身份执行）
flatpak --system install -y flathub org.mozilla.firefox
flatpak --system uninstall -y org.mozilla.firefox
flatpak --system update -y org.mozilla.firefox
flatpak --user  install -y flathub org.mozilla.firefox
```

**不要使用**：`flatpak search --columns=…,origin`（列不存在）、
`flatpak list --updates`（1.18 不存在该选项）、`flatpak remote-info -j`（不存在）。

### A.3 Flathub / ODRS / Arch 安全公告

```
GET https://flathub.org/api/v2/appstream/{app_id}   -> 200（名称/分类/许可/图标URL/截图/关键词）
GET https://flathub.org/api/v2/summary/{app_id}     -> 200（下载/安装大小、runtime、权限、extensions）
GET https://odrs.gnome.org/1.0/reviews/api/ratings/{app_id} -> {star0..star5,total}（不稳定，需超时+降级）
GET https://security.archlinux.org/issues/all.json  -> 308 重定向后 200（须跟随重定向）
```

**实测网络特征（reqwest + rustls，本机 2026-09-10）**——用于设定超时与缓存参数：

| 请求 | 结果 | 耗时 | 响应体 |
| --- | --- | --- | --- |
| AUR `search&arg=cowsay` | 200 | 1773 ms（首次，含 TLS） | 11.7 KB |
| AUR `info&arg[]=yay` | 200 | 508 ms | 659 B |
| AUR 连续 5 次 `info` | 200 × 5/5 | 515–713 ms | — |
| Flathub appstream（firefox） | 200 | 1243 ms | 6.6 KB |
| security.archlinux.org `issues/all.json` | 200（跟随重定向） | 5351 ms | **898 KB** |
| ODRS ratings | **失败**（连接重置） | 649 ms | — |

由此得出硬性参数：AUR/Flathub 请求超时 30 s 足够但**首屏不应阻塞**（异步填充）；
安全公告必须缓存 6 小时（900 KB 且 5 s 级延迟）；ODRS 超时设 3 s 且失败静默。

---

## 附录 B：libalpm（`alpm` crate 5.0.2）用法映射

| 需求 | 正确 API | 实测/说明 |
| --- | --- | --- |
| 打开句柄 | `Alpm::new("/", "/var/lib/pacman")` | 普通用户可用 |
| 注册同步库 | `handle.register_syncdb_mut(name, SigLevel::USE_DEFAULT)` | 需 `&mut handle`；库名来自 pacman.conf |
| 枚举库中所有包 | `db.pkgs()` → `AlpmList<&Package>`（`.iter()`、`.len()`） | **不存在 `pkg_cache()`** |
| 精确取包 | `db.pkg("firefox")` | `Result<&Package>`；未找到 → `PkgNotFound` |
| 库内搜索 | `db.search(["firefox"].iter().cloned())` | 参数需 `AsAlpmList<&str>`；数组字面量不满足 |
| 包字段 | `name/version/desc/url/size/isize/licenses/groups/depends/makedepends/optdepends/checkdepends/provides/conflicts/replaces/reason/install_date/build_date/files/required_by` | `size` = 下载大小（本地库为 0），`isize` = 安装后大小 |
| 虚拟依赖 | `pkg.provides()` | 实测 mesa → `mesa-libgl 1:26.2.2-1`、`opengl-driver`（无版本） |
| 反向依赖 | `pkg.required_by()` | 实测 glibc → 705 |
| 依赖缺失 | `handle.check_deps(pkgs, rem, upgrade, reverse_deps)` | 反依赖检查必须把全体已安装包放进 `pkgs`（§4.3） |
| 版本比较 | `alpm::vercmp(a, b)` / `ver.as_ver().vercmp(other)` | 实测 `1.10>1.9`、`2.0rc1<2.0`；`Ver: Display + Deref<str>` |
| 包组 | `db.groups()` / `db.group(name)` | 实测 extra → 107 个组，可映射为侧栏分类 |
| 句柄释放 | `handle.release()` | 正常退出路径调用 |

**不要使用（v0.1.0 中的错误写法）**：`handle.register_syncdbs()`（无此方法）、
`db.pkg_cache()`（无此方法）、`pkg.check_deps()`（无此方法，已在 `handle` 上）。

**不要相信**：`register_syncdb_mut` 的返回值只表示"注册动作成功"，不表示库存在（§4.3 陷阱）；
`db.is_valid()` 对空库同样返回 true。

---

## 附录 C：错误类型清单（`CoreError`）

| 变体 | 触发 | UI 表现 |
| --- | --- | --- |
| `EnvUnsupported(String)` | 非 Arch 系统、gtk/adw 版本不足 | 启动即失败对话框（含修复命令） |
| `AlpmInit(String)` | `Alpm::new` 失败 | 官方仓库相关页面全部显示 ErrorView |
| `SyncDbMissing(String)` | 同步库缺失/为空 | 分类页提示"请先运行 `pacman -Sy`" |
| `BackendUnavailable { kind, reason }` | 后端能力探测失败 | 设置页置灰 + 原因；相关入口隐藏 |
| `Network { url, source }` | 请求失败 | 该源结果区显示错误 + 重试；其他源不受影响 |
| `Timeout { url, secs }` | 超时 | 同上，文案为"请求超时（30 秒）" |
| `RateLimited { retry_after }` | 429/503 | "请求过于频繁，N 秒后自动重试" |
| `Parse { context, raw_head }` | 子进程/JSON 解析失败 | "无法识别 <后端> 的输出"；附"复制诊断信息" |
| `InvalidName(String)` | 包名不合规 | 表单错误提示（正常路径下不应出现） |
| `PlanRejected { reason }` | helper 拒绝计划 | 计划栏显示原因，计划保留供用户修改 |
| `Locked` | `/var/lib/pacman/db.lck` 存在 | "另一个包管理器正在运行" |
| `ReverseDeps { dependents }` | 卸载会破坏依赖 | 询问是否级联删除 |
| `AuthDenied` | polkit 拒绝/取消 | "已取消授权"，计划保留 |
| `TransactionFailed { code, log_tail }` | helper 返回非 0 | 失败面板 + 日志尾部 + 重试按钮 |
| `Config(String)` | 配置解析/写入失败 | Toast 提示 + 自动备份 |

**原则**：错误必须携带**足够的上下文让用户自己修复**（URL、路径、退出码、原始输出片段），
而不是"出错了"。

---

## 附录 D：v0.1.0 缺陷与修正记录

| # | v0.1.0 内容 | 问题 | 本版修正 |
| --- | --- | --- | --- |
| 1 | `gtk4 = "0.9"` / `libadwaita = "0.7"` | 版本落后（当前 0.11 / 0.9），且 feature 与系统库不匹配 | §2.2 给出实测基线与最低版本 |
| 2 | `flatpak = "0"  # flatpak-rs 解析 manifest` | 该 crate 最后发布 2022-08（0.18.1），依赖 ostree 0.20；且运行时根本拿不到 manifest | 改为 `flatpak` CLI + Flathub API（§4.5/§4.6） |
| 3 | `sled = "0.34"` | 最后发布 2024-10，长期无维护；为"可丢弃缓存"引入完整 KV 数据库不划算 | 自研文件缓存 + LRU 索引（§6.1） |
| 4 | `alpm = "4"`、`alpm::Alpm::new(...)` + `register_syncdbs()` | 版本落后；`register_syncdbs()` 方法**不存在**（应为 `register_syncdb_mut`） | 附录 B 给出实测 API |
| 5 | 附录 C 用 `pkg_cache()`、`pkg.check_deps()` | 两个 API 都不存在 | 改为 `db.pkgs()`、`handle.check_deps(...)` |
| 6 | `flatpak search --columns=…,origin` | **实测报错**：未知列 origin | 改为 `remotes`（附录 A.2） |
| 7 | 用 `flatpak -j` 但不控制 locale | JSON 键名被本地化（实测中文键"应用程序_id"），解析会随机失败 | 强制 `LC_ALL=C` + 按列序解析兜底（§4.5） |
| 8 | "通过 alpm 执行事务 + PolicyKit 提权" | 自相矛盾：libalpm 事务必须在 root 下，会导致 GTK 以 root 运行（安全红线） | 引入 `archstore-helper` 独立 root 进程与计划文件协议（§5） |
| 9 | 事务队列直接持有 `Vec<Command>` | 无法序列化、无法做崩溃恢复，且把提权细节混进后端层 | 改为可序列化的 `TransactionPlan`（§5.1） |
| 10 | `CacheService::get_or_fetch` 示例 | 泛型边界错误、`T: DeserializeOwned` 与 `serde_json::Value` 混用、缺 `Send + 'static`，无法编译 | 重新设计缓存接口（§6.1），示例改为示意并标注 |
| 11 | `PackageCard { package }` + `self.load_icon_async(icon.clone())` | 自引用结构，Rust 中无法编译 | 改为"数据行 + 独立渲染函数"（§7.1/§7.4） |
| 12 | `Picture::set_icon_name()` / `set_from_file_async()` | GTK4 无这些 API（`set_icon_name` 属于 `gtk::Image`） | 改为 `Image::from_icon_name` + `Picture::set_file`（§7.1） |
| 13 | 事务审查"执行"直接提权执行 | 无白名单、无重校验、可能命令注入到 root；无锁冲突与崩溃恢复处理 | §5.1/§5.2 的安全红线与 §5.4 状态机 |
| 14 | 未定义线程模型 | 千级 libalpm 遍历会阻塞 GTK 主线程 | §3.1 三线程/进程模型 |
| 15 | `raur::Handle::search()` 作为固有方法 | 实为 trait 方法，需 `use raur::Raur;`；且 `raur::Cache` 不是持久化缓存 | §4.4 澄清 |
| 16 | "在线翻译描述"为核心功能 | 隐私与失败面成本 > 收益 | 降级为可选、默认关闭、标注"机器翻译"（§7.2） |
| 17 | 依赖 `.SRCINFO` 解析 | 不必要：RPC `info` 已含 `Depends/MakeDepends/OptDepends` | §4.4/附录 A.1 |
| 18 | 使用 Meson 打包纯 Rust GTK 应用 | 增加故障面，无实际收益 | 改为 Cargo + PKGBUILD（§2.1） |
| 19 | 使用 PTY 嵌入展示安装日志 | MVP 无交互式输入需求；PTY 转发复杂且易死锁 | 改为 helper 逐行 JSON 事件流（§5.5） |
| 20 | 硬编码 `core/extra/multilib` | 本机 multilib 实测为 0 包（未启用） | 从 pacman.conf 解析同步库名（§4.3） |
| 21 | 版本比较使用字符串比较（隐含） | 会误判（如 `1.10` vs `1.9`） | 统一 `alpm::vercmp`（附录 B） |
| 22 | 无空/错/加载三态、无错误类型设计、无测试与 CI | "能跑"与"稳定可靠"的差距 | §7.3、附录 C、§11 |
| 23 | "安全更新"依赖未指明的匹配逻辑 | 无法实现 | 明确使用 security.archlinux.org JSON 与区间匹配规则（§9.2） |
| 24 | AUR 与官方仓库更新混在一批 | 会导致 Arch 部分升级风险 | 拆分计划 + 显式警告（§5.3） |

### D.1 本轮实测新增的、必须在实现前知道的坑

这些不是 v0.1.0 的原文错误，而是"照 v0.1.0 的思路写下去必然踩到"的运行时陷阱，全部来自本机实测：

| # | 现象（实测） | 后果 | 对策 |
| --- | --- | --- | --- |
| 25 | `register_syncdb_mut("does-not-exist")` 返回 **Ok**，`pkgs()=0`，`is_valid()=true` | 用户机器上仓库永远"0 个包"却无任何报错，问题无法定位 | 三条件判定仓库可用性（§4.3） |
| 26 | `register_syncdb_mut("Extra")`（大小写错误）同样返回 Ok | 同上，且极易发生在从 pacman.conf 手工解析时 | 库名必须原样取自 pacman.conf，不做事后大小写归一 |
| 27 | `Alpm::new("/", "不存在的路径")` → `Err(NotADir)` | 若假设"路径总会存在"，启动即 panic | 显式处理并降级（附录 C `AlpmInit`） |
| 28 | `handle.check_deps(空, [待删包], 空, true)` 返回 **0**，而正确传法返回 705 | **静默漏报反依赖**：用户删 glibc 前的警告消失，导致系统损坏 | 反依赖检查必须把全体已安装包放进 `pkgs`（§4.3） |
| 29 | `raur` 的 `default-features = false` 会同时关掉 `async` feature | `raur::Handle`、`raur::Raur` 直接"不存在"，报错信息不直观 | 显式加回 `features = ["async", "rusttls-ring"]`（§2.2） |
| 30 | `raur::Package` 字段是 `make_depends`/`opt_depends`/`check_depends`/`num_votes`，不是 RPC 里的 `MakeDepends`/`NumVotes` | 按 RPC 文档写代码会编译不过 | §4.4 字段映射表 |
| 31 | `flatpak list -j` 在中文 locale 下键名是"应用程序_id"等中文 | 解析随机失败，且只在中文用户处复现 | 强制 `LC_ALL=C`（§4.5） |
| 32 | `flatpak search` 没有 `origin` 列（`list` 才有） | 命令直接失败 | 用 `remotes`（附录 A.2） |
| 33 | ODRS rating 接口在 reqwest 下直接连接重置（curl 偶尔可用） | 评分区域加载失败拖慢详情页 | 3 s 超时 + 静默降级（§4.6、附录 A.3） |
| 34 | `security.archlinux.org/issues/all.json` 有 **898 KB** 且耗时 5.3 s | 每次进入更新页都拉取会明显卡顿并浪费流量 | 缓存 6 小时、仅更新页可见时拉取（§9.2） |
| 35 | AUR RPC 首次请求 1.8 s（含 TLS 握手），后续 0.5 s | 若同步等待会明显感觉卡 | 全部异步 + 增量渲染（§3.3） |
| 36 | 「同步库名硬编码」在本机表现为 multilib 为 0 包 | 分类页出现空分类且无解释 | 从 pacman.conf 解析 + 空库标注"未同步"（§4.3） |
