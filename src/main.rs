use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;
use win_wsl_usbipd::{
    backend::{Backend, SystemBackend},
    model::{BusId, Device, Environment},
    process::{CommandRunner, SystemRunner, find_usbipd, find_wsl},
    ui,
};

#[derive(Debug, Parser)]
#[command(
    name = "wsl-usb",
    version,
    about = "选择 USB 设备，按 Enter 映射到 WSL2",
    long_about = "Windows USB → WSL2 终端工具。\n不指定子命令时进入交互模式：↑/↓ 选择，Enter 映射，d 断开，r 刷新，q 退出。\n首次共享可能触发 Windows UAC 授权。请保持一个 WSL2 终端运行。"
)]
struct Cli {
    /// 指定执行映射的 WSL2 发行版
    #[arg(short = 'd', long, global = true, value_name = "NAME")]
    distribution: Option<String>,
    /// 普通外部命令超时（秒）；UAC 授权至少等待 120 秒
    #[arg(long, global = true, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300), value_name = "SECONDS")]
    timeout: u64,
    /// 禁止自动发起 UAC；需要预先共享设备或从管理员终端运行
    #[arg(long, global = true)]
    no_elevate: bool,
    /// 交互模式自动刷新间隔（秒）
    #[arg(long, global = true, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..=60), value_name = "SECONDS")]
    refresh_seconds: u64,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// 列出当前连接的 USB 设备（不要求 WSL 正在运行）
    List {
        /// 输出可供脚本读取的 JSON 数组
        #[arg(long)]
        json: bool,
    },
    /// 检查 usbipd、WSL2 与设备读取是否可用
    Doctor {
        /// 输出 JSON 诊断报告；检查失败仍返回非零退出码
        #[arg(long)]
        json: bool,
    },
    /// 共享并映射指定 BUSID，例如：attach 2-3
    Attach { busid: BusId },
    /// 断开指定 BUSID 的连接，保留共享设置
    Detach { busid: BusId },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{}", plain_text(&format!("{error:#}")));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Some(Action::Doctor { json }) = &cli.command {
        return doctor(&cli, *json);
    }
    if !cfg!(windows) {
        bail!(
            "请在 Windows PowerShell / CMD / Windows Terminal 中运行此工具；不要在 WSL 内运行 Linux 版本。"
        );
    }
    let usbipd = find_usbipd()?;
    // Listing and detaching still work when WSL is absent or broken.
    let needs_wsl = cli.command.is_none() || matches!(cli.command, Some(Action::Attach { .. }));
    let wsl = if needs_wsl {
        find_wsl()?
    } else {
        PathBuf::from("wsl.exe")
    };
    let mut backend = SystemBackend::new(
        SystemRunner::new(Duration::from_secs(cli.timeout), !cli.no_elevate),
        usbipd,
        wsl,
    );

    match cli.command {
        Some(Action::List { json }) => {
            let devices = backend.list_devices()?;
            if json {
                print_json(&devices)?;
            } else {
                print_devices(&devices)?;
            }
        }
        Some(Action::Attach { busid }) => {
            let environment = backend.inspect()?;
            let distribution = environment.select_distribution(cli.distribution.as_deref())?;
            let device = find_device(&mut backend, &busid)?;
            println!("{}", plain_text(&backend.attach(&device, &distribution)?));
        }
        Some(Action::Detach { busid }) => {
            let device = find_device(&mut backend, &busid)?;
            println!("{}", plain_text(&backend.detach(&device)?));
        }
        Some(Action::Doctor { .. }) => unreachable!("doctor is handled before platform validation"),
        None => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "交互模式需要终端输入和输出。请直接运行 wsl-usb；脚本中请使用 wsl-usb list --json 或 attach / detach 子命令。"
                );
            }
            eprintln!("正在检测 usbipd 和 WSL2…");
            let environment = backend.inspect()?;
            let distribution = environment.select_distribution(cli.distribution.as_deref())?;
            ui::run(
                backend,
                environment,
                distribution,
                Duration::from_secs(cli.refresh_seconds),
            )?;
        }
    }
    Ok(())
}

fn find_device(backend: &mut impl Backend, busid: &BusId) -> Result<Device> {
    backend
        .list_devices()?
        .into_iter()
        .find(|device| &device.bus_id == busid)
        .with_context(|| {
            format!("设备 {busid} 不存在或已拔出。请先运行 wsl-usb list 查看最新 BUSID。")
        })
}

fn print_devices(devices: &[Device]) -> Result<()> {
    let mut output = io::stdout().lock();
    if devices.is_empty() {
        writeln!(output, "没有检测到已连接的 USB 设备。插入设备后重试。")?;
        return Ok(());
    }
    writeln!(output, "BUSID    VID:PID    状态      设备")?;
    for device in devices {
        writeln!(
            output,
            "{:<8} {:<10} {}    {}",
            device.bus_id.to_string(),
            device.vid_pid(),
            device.status_label(),
            plain_text(&device.description)
        )?;
    }
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

fn plain_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

#[derive(Serialize)]
struct DiagnosticReport {
    ok: bool,
    usbipd_path: Option<PathBuf>,
    wsl_path: Option<PathBuf>,
    usbipd_version: Option<String>,
    environment: Option<Environment>,
    selected_distribution: Option<String>,
    connected_devices: Option<usize>,
    errors: Vec<String>,
}

fn doctor(cli: &Cli, json: bool) -> Result<()> {
    let mut report = DiagnosticReport {
        ok: false,
        usbipd_path: None,
        wsl_path: None,
        usbipd_version: None,
        environment: None,
        selected_distribution: None,
        connected_devices: None,
        errors: Vec::new(),
    };
    if !cfg!(windows) {
        report
            .errors
            .push("必须从 Windows 主机运行，Linux / WSL 版本不能控制 Windows USB 设备。".into());
    } else {
        match find_usbipd() {
            Ok(path) => report.usbipd_path = Some(path),
            Err(error) => report.errors.push(format!("{error:#}")),
        }
        match find_wsl() {
            Ok(path) => report.wsl_path = Some(path),
            Err(error) => report.errors.push(format!("{error:#}")),
        }
        if let Some(usbipd) = &report.usbipd_path {
            let mut runner = SystemRunner::new(Duration::from_secs(cli.timeout), false);
            match runner
                .run(usbipd, &["--version"], Duration::from_secs(cli.timeout))
                .and_then(|output| output.ensure_success("读取 usbipd 版本失败"))
            {
                Ok(output) => report.usbipd_version = Some(output.stdout.trim().into()),
                Err(error) => report.errors.push(format!("{error:#}")),
            }
            let mut backend = SystemBackend::new(
                runner,
                usbipd.clone(),
                report
                    .wsl_path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("wsl.exe")),
            );
            match backend.list_devices() {
                Ok(devices) => report.connected_devices = Some(devices.len()),
                Err(error) => report.errors.push(format!("{error:#}")),
            }
            if report.wsl_path.is_some() {
                match backend.inspect() {
                    Ok(environment) => {
                        match environment.select_distribution(cli.distribution.as_deref()) {
                            Ok(name) => {
                                if environment
                                    .distributions
                                    .iter()
                                    .any(|distro| distro.name == name && distro.running)
                                {
                                    report.selected_distribution = Some(name);
                                } else {
                                    report.errors.push(format!("WSL2 发行版 {name} 尚未运行。请打开 wsl --distribution \"{name}\" 并保持终端开启。"));
                                }
                            }
                            Err(error) => report.errors.push(format!("{error:#}")),
                        }
                        report.environment = Some(environment);
                    }
                    Err(error) => report.errors.push(format!("{error:#}")),
                }
            }
        }
    }
    report.ok = report.errors.is_empty();
    if json {
        print_json(&report)?;
    } else {
        println!("WSL USB 环境检查");
        if let Some(path) = &report.usbipd_path {
            println!("[OK] usbipd: {}", plain_text(&path.display().to_string()));
        }
        if let Some(version) = &report.usbipd_version {
            println!("     版本: {}", plain_text(version));
        }
        if let Some(path) = &report.wsl_path {
            println!("[OK] WSL: {}", plain_text(&path.display().to_string()));
        }
        if let Some(environment) = &report.environment {
            for distro in &environment.distributions {
                println!(
                    "     {}  WSL{}  {}{}",
                    plain_text(&distro.name),
                    distro.version,
                    if distro.running {
                        "运行中"
                    } else {
                        "已停止"
                    },
                    if distro.is_default {
                        "（默认）"
                    } else {
                        ""
                    }
                );
            }
        }
        if let Some(name) = &report.selected_distribution {
            println!("[OK] 执行映射的发行版: {}", plain_text(name));
        }
        if let Some(count) = report.connected_devices {
            println!("[OK] 已连接 USB 设备: {count}");
        }
        for error in &report.errors {
            println!("[失败] {}", plain_text(error));
        }
        println!(
            "{}",
            if report.ok {
                "环境就绪。运行 wsl-usb 即可选择设备。"
            } else {
                "请处理上述问题后重新检查。"
            }
        );
    }
    if !report.ok {
        bail!("环境检查未通过");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn global_options_work_after_subcommands() {
        let cli = Cli::try_parse_from([
            "wsl-usb",
            "attach",
            "2-3",
            "--distribution",
            "Ubuntu 24.04",
            "--no-elevate",
            "--timeout",
            "60",
        ])
        .unwrap();
        assert_eq!(cli.distribution.as_deref(), Some("Ubuntu 24.04"));
        assert!(cli.no_elevate);
        assert_eq!(cli.timeout, 60);
        assert!(matches!(cli.command, Some(Action::Attach { .. })));
    }

    #[test]
    fn rejects_invalid_busids_and_unbounded_wait_options() {
        for args in [
            vec!["wsl-usb", "attach", "2-3;echo"],
            vec!["wsl-usb", "--timeout", "0"],
            vec!["wsl-usb", "--refresh-seconds", "61"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn default_is_interactive() {
        assert!(Cli::try_parse_from(["wsl-usb"]).unwrap().command.is_none());
    }
}
