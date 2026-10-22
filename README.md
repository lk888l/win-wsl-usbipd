# wsl-usb

一个运行在 **Windows 终端**中的 Rust USB 映射工具。启动后检查 usbipd 和 WSL2，列出当前连接的 USB 设备；用方向键选择设备，按 **Enter** 即可完成共享和映射。

工具调用官方 `usbipd-win`，处理设备查询、首次共享提权、映射、断开和状态刷新。默认不修改驱动、不强制绑定、不自动安装软件。

## 环境要求

- Windows 10 / 11，已安装并配置 WSL2。
- `usbipd-win` **5.0.0 或更新版本**。
- 至少一个正在运行的 WSL2 发行版。请保持一个 WSL 终端打开。
- 从源码编译需要 Rust **1.88 或更新版本**及 MSVC C++ 构建工具。

应从 Windows 的 PowerShell、CMD 或 Windows Terminal 启动本工具。Linux / WSL 内可以运行纯逻辑测试，但不能执行设备管理功能。

如果尚未安装 usbipd，工具会给出安装指引。也可自行执行：

```powershell
winget install --interactive --exact dorssel.usbipd-win
```

安装后重新打开终端。`--interactive` 保留安装程序交互，避免安装依赖时无提示重启。

检查 WSL2：

```powershell
wsl --list --verbose
wsl --distribution Ubuntu-24.04
```

将示例发行版名称替换为自己的名称，并保持该 WSL 终端打开。若没有 WSL2，请先按 [微软 WSL 安装指南](https://learn.microsoft.com/windows/wsl/install)完成配置。

## 快速开始

### 下载 Windows x64 版本

从 [v1.0.1 Release](https://github.com/lk888l/win-wsl-usbipd/releases/tag/v1.0.1) 下载 `wsl-usb-v1.0.1-windows-x86_64.zip` 和 `SHA256SUMS.txt`。预编译版本适用于 Windows x64，无需安装 Rust；仍需先安装和配置上述 usbipd-win、WSL2，并保持一个 WSL2 终端运行。

在下载目录中打开 PowerShell，核对 ZIP 的 SHA-256 与 `SHA256SUMS.txt` 中对应文件的值一致，再解压运行：

```powershell
Get-FileHash .\wsl-usb-v1.0.1-windows-x86_64.zip -Algorithm SHA256
Get-Content .\SHA256SUMS.txt
Expand-Archive .\wsl-usb-v1.0.1-windows-x86_64.zip -DestinationPath .\wsl-usb-v1.0.1
.\wsl-usb-v1.0.1\wsl-usb.exe --version
.\wsl-usb-v1.0.1\wsl-usb.exe
```

### 从源码构建

安装 Rust 和 MSVC C++ 构建工具后，在项目目录执行：

```powershell
cargo run --release --locked
```

也可直接运行编译产物：

```powershell
.\target\release\wsl-usb.exe
```

安装到 Cargo 的可执行文件目录后，可在任意目录运行：

```powershell
cargo install --path . --locked
wsl-usb
```

1. 启动后查看检测结果和 USB 设备列表。
2. 按 `↑` / `↓` 选择设备。
3. 按 `Enter` 映射到 WSL2；未共享的设备会先请求 Windows UAC 管理员授权。
4. 操作完成后查看设备状态；需要归还 Windows 时，选择设备并按 `d`。

只在首次共享设备时需要管理员权限，日常查询、映射和断开均可从普通终端执行。UAC 是 Windows 的系统授权提示，工具本身没有图形界面。

## 按键

| 按键 | 操作 |
| --- | --- |
| `↑` / `↓`，`k` / `j` | 选择上一台 / 下一台设备 |
| `Enter` / `a` | 将所选设备映射到 WSL2 |
| `d` | 断开所选设备的连接 |
| `r` | 立即刷新设备列表 |
| `q` / `Esc` / `Ctrl+C` | 退出 |

设备列表默认每 3 秒刷新，仅显示当前物理连接的设备。已经拔出但保留共享记录的设备不会占据列表。退出工具不会主动断开已映射的设备。操作进行中按退出键，会等待当前操作完成后退出。

## 命令行用法

不提供子命令时进入交互模式，也可使用以下命令进行脚本操作：

```powershell
# 查看帮助
wsl-usb --help

# 查询当前连接的 USB 设备
wsl-usb list
wsl-usb list --json

# 检查 usbipd 和 WSL2 环境
wsl-usb doctor
wsl-usb doctor --json

# 使用实际列表中的 BUSID 进行映射或断开
wsl-usb attach 2-2
wsl-usb detach 2-2

# 指定用于执行映射的 WSL2 发行版
wsl-usb attach 2-2 --distribution Ubuntu-24.04

# 禁用自动 UAC 提权，未共享时显示手动 bind 指引
wsl-usb --no-elevate attach 2-2

# 调整命令超时和交互界面刷新频率
wsl-usb --timeout 60 --refresh-seconds 5
```

| 全局选项 | 默认值 | 说明 |
| --- | --- | --- |
| `-d, --distribution <NAME>` | 自动选择 | 指定执行映射的 WSL2 发行版；必须正在运行 |
| `--timeout <SECONDS>` | `30` | 每条普通外部命令超时，范围 `1..=300` 秒；UAC 流程总等待预算为至少 120 秒，另留 5 秒清理时间 |
| `--no-elevate` | 关闭 | 禁止自动请求 UAC，适合脚本或已预先共享的设备 |
| `--refresh-seconds <SECONDS>` | `3` | 交互界面刷新间隔，范围 `1..=60` 秒 |

全局选项可放在子命令前后。脚本建议使用 `--no-elevate`，避免等待人工授权。命令失败返回非零退出码；`--json` 适用于设备查询和环境诊断。

## 设备状态与行为

工具通过 `usbipd state` 的 JSON 接口查询设备，不解析可能被截断的 `usbipd list` 表格。判断依据如下：

| JSON 字段 | 含义 |
| --- | --- |
| `BusId` 为数字形式的总线-端口 | 当前已连接，可列出和操作；跳过不可映射的 `IncompatibleHub` 设备 |
| `PersistedGuid` 非空 | 已共享，通常无需再次提权绑定 |
| `ClientIPAddress` 非空 | 已连接到某个客户端 |

“已连接”可能表示 WSL，也可能是其他 USB/IP 客户端。工具不会仅凭该字段将其他客户端连接标记为 WSL。

映射的流程为：检查环境 → 刷新设备状态 → 必要时执行 `bind` → 执行 `attach` → 再次确认状态。执行前、UAC 授权后以及映射前核对 BUSID 与 InstanceId，降低拔插造成端口复用时误操作的风险。授权请求带过期时间，过期后接受 UAC 不会执行共享；提权辅助进程负责终止并回收超时的 usbipd 子进程。

共享是持久的，断开连接不会取消共享。设备映射期间由 WSL 使用，Windows 暂时无法使用它；断开后通常归还 Windows。工具不会自动执行 `unbind` 或 `bind --force`。

WSL2 的 USB 设备由各发行版共享。`--distribution` 选择的是执行连接命令的发行版，不会将设备隔离到这个发行版。

## 常见问题

**找不到 usbipd**：按工具提示安装，安装完成后重新打开终端；用 `usbipd --version` 验证。

**首次映射出现 UAC**：共享设备是管理员操作。接受授权后工具继续映射；如果取消授权，可保留在界面中重试，也可在管理员 PowerShell 中手动执行 `usbipd bind --busid <BUSID>`。

UAC 后台共享失败时，工具会显示退出码和重试指引；Windows 的提权通道不直接传回 usbipd 的详细输出，需在管理员终端执行上述命令查看。普通查询、映射、断开失败会保留原始命令诊断。

**没有运行中的 WSL2**：先运行 `wsl --list --verbose`，确认发行版版本为 `2`，再打开该发行版终端并保持运行。仅执行一个立即退出的 Linux 命令不能可靠地保持 WSL 虚拟机活动。

**映射失败或超时**：查看工具保留的 usbipd 错误信息，检查 usbipd 服务、WSL 运行状态及网络 / 防火墙配置。慢速启动环境可增大 `--timeout`。若内核缺少 USB/IP 支持，可执行 `wsl --update` 后重试。

**Windows 正在占用设备**：先关闭使用该设备的软件再映射。工具不会自动强制绑定。

**WSL 已连接但应用无权限**：在 WSL 中执行 `lsusb` 检查设备；应用可能需要发行版自己的驱动或 udev 权限规则。

**重新插入设备后连接消失**：拔插、WSL 重启会使连接断开，刷新列表后重新选择并映射。首次共享记录通常仍然保留。

## 开发与验证

采用 Rust 2024 edition，将命令行入口、终端交互、设备模型、业务流程和外部进程执行分离。业务逻辑使用可替换的后端进行测试；设备标识和错误使用明确的类型表达。

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

自动化测试使用模拟命令 / 后端覆盖解析和流程，不会共享、映射或断开本机 USB 设备。真实硬件验证需使用可安全转交给 WSL 的设备，在 Windows 上手动完成映射和断开操作。

CI 在 Windows 和 Linux 上执行格式、Clippy 和测试检查，并在 Windows 上生成 release 构建；另有最低 Rust 版本检查。`Cargo.lock` 随仓库提交，以固定依赖解析结果。Dependabot 检查 Cargo 和 GitHub Actions 依赖更新。

## 发布流程

发布前同步 `Cargo.toml`、`Cargo.lock` 的版本和 [CHANGELOG.md](CHANGELOG.md)。推送 `v*` 版本标签（例如 `v1.0.0`）会触发完整 CI；仅在 Windows、Linux 和最低 Rust 版本检查全部通过后，工作流才会打包 Windows x64 可执行文件、生成 `SHA256SUMS.txt` 并发布 GitHub Release。标签版本必须与 Cargo 包版本一致。

v1.0.0 的自动化验证使用模拟后端和进程测试，无需 USB 硬件。本次发布未进行真实 USB 设备的共享、映射或断开验证。

## 官方参考

- [微软：连接 USB 设备到 WSL](https://learn.microsoft.com/windows/wsl/connect-usb)
- [usbipd-win：WSL 支持与权限要求](https://github.com/dorssel/usbipd-win/wiki/WSL-support)
- [usbipd-win：自动化 JSON 接口](https://github.com/dorssel/usbipd-win/wiki/Automation)
- [微软：WSL 基本命令](https://learn.microsoft.com/windows/wsl/basic-commands)

## 许可证

本项目使用 [MIT 许可证](LICENSE)。usbipd-win 是单独安装和调用的外部程序，遵循其自身许可证。
