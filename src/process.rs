//! Process boundary: argument-safe execution, bounded waits and Windows elevation.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

/// The captured result of a command. Exit failure is distinct from launch failure.
#[derive(Clone, Debug)]
pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ensure_success(self, context: &str) -> Result<Self> {
        if !self.success {
            let details = [self.stderr.trim(), self.stdout.trim()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            bail!("{context}（退出码 {:?}）\n{details}", self.code);
        }
        Ok(self)
    }
}

/// Injectable boundary used by the backend's hardware-free workflow tests.
pub trait CommandRunner: Send {
    fn run(&mut self, program: &Path, args: &[&str], timeout: Duration) -> Result<CommandOutput>;

    fn elevated_bind(
        &mut self,
        program: &Path,
        bus_id: &str,
        expected_instance_id: &str,
        timeout: Duration,
    ) -> Result<CommandOutput>;
}

pub struct SystemRunner {
    timeout: Duration,
    allow_elevation: bool,
}

impl SystemRunner {
    pub fn new(timeout: Duration, allow_elevation: bool) -> Self {
        Self {
            timeout,
            allow_elevation,
        }
    }
}

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &Path, args: &[&str], _timeout: Duration) -> Result<CommandOutput> {
        // The application's explicit --timeout setting overrides backend defaults.
        execute(program, args, self.timeout)
    }

    fn elevated_bind(
        &mut self,
        program: &Path,
        bus_id: &str,
        expected_instance_id: &str,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        // Validate at the privilege boundary even if the caller already used BusId.
        let _: crate::model::BusId = bus_id.parse()?;
        if expected_instance_id.is_empty() {
            bail!("无法共享没有 InstanceId 的设备");
        }
        let powershell = find_powershell()?;
        let budget = if self.allow_elevation {
            timeout.max(self.timeout)
        } else {
            self.timeout
        };
        let expires_at =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() + budget.as_millis();
        let helper = bind_helper_script(
            program,
            bus_id,
            expected_instance_id,
            self.timeout,
            expires_at,
        )?;
        let script = if self.allow_elevation {
            format!(
                "$helperScript = {}\n$powerShellExecutable = {}\n{}",
                powershell_literal(&helper),
                powershell_literal(
                    powershell
                        .to_str()
                        .context("PowerShell 路径不是有效 Unicode")?
                ),
                include_str!("platform/elevate.ps1"),
            )
        } else {
            helper
        };
        // The helper checks its absolute expiry after UAC, revalidates identity,
        // and kills/reaps its own privileged child. An old consent dialog cannot
        // execute a bind after this deadline. Allow a short cleanup grace period.
        execute(
            &powershell,
            &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &script],
            budget + Duration::from_secs(5),
        )
        .and_then(|output| output.ensure_success(if self.allow_elevation {
            "共享失败；若取消了 UAC，请重新操作。可在管理员终端运行 usbipd bind --busid <BUSID> 查看详细错误"
        } else {
            "共享失败；--no-elevate 已禁用 UAC。请在管理员终端执行 usbipd bind --busid <BUSID> 后重试"
        }))
        .context(if self.allow_elevation {
            "首次共享需要 Windows 管理员授权；若已取消 UAC，请重新按 Enter。若等待超时，请关闭遗留授权框并刷新设备状态"
        } else {
            "无法共享设备（自动提权已禁用）"
        })
    }
}

fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn bind_helper_script(
    program: &Path,
    bus_id: &str,
    expected_instance_id: &str,
    timeout: Duration,
    expires_at: u128,
) -> Result<String> {
    if !program.is_absolute() {
        bail!("管理员命令必须使用 usbipd.exe 的绝对路径");
    }
    let path = program
        .to_str()
        .context("usbipd.exe 路径不是有效 Unicode")?;
    Ok(format!(
        "$usbExecutable = {}\n$busId = {}\n$expectedInstance = {}\n$expiresAt = {expires_at}L\n$commandTimeoutMs = {}\n{}",
        powershell_literal(path),
        powershell_literal(bus_id),
        powershell_literal(expected_instance_id),
        timeout.as_millis(),
        include_str!("platform/bind.ps1"),
    ))
}

/// Execute without a shell, draining both pipes concurrently to avoid deadlocks.
fn execute(program: &Path, args: &[&str], timeout: Duration) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Recent WSL supports UTF-8; decode_output also supports older UTF-16 builds.
    command.env("WSL_UTF8", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("无法启动 {}", program.display()))?;
    let stdout = child.stdout.take().context("无法捕获标准输出")?;
    let stderr = child.stderr.take().context("无法捕获错误输出")?;
    let out_rx = read_pipe(stdout);
    let err_rx = read_pipe(stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("等待子进程失败");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} 超时（{} 秒）。请刷新设备状态后重试；可使用 --timeout 调整普通命令等待时间",
                program.display(),
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A descendant might keep an inherited pipe open. Do not wait indefinitely.
    let read_result = |rx: mpsc::Receiver<Result<Vec<u8>>>| -> Result<String> {
        let bytes = rx
            .recv_timeout(Duration::from_secs(2))
            .context("子进程已退出，但输出管道未关闭")??;
        decode_output(&bytes)
    };
    Ok(CommandOutput {
        success: status.success(),
        code: status.code(),
        stdout: read_result(out_rx)?,
        stderr: read_result(err_rx)?,
    })
}

fn read_pipe(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Result<Vec<u8>>> {
    const MAX_OUTPUT: usize = 4 * 1024 * 1024;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut output = Vec::new();
            let mut chunk = [0_u8; 8192];
            let mut overflow = false;
            loop {
                let count = pipe.read(&mut chunk).context("读取子进程输出失败")?;
                if count == 0 {
                    break;
                }
                if output.len() + count <= MAX_OUTPUT && !overflow {
                    output.extend_from_slice(&chunk[..count]);
                } else {
                    overflow = true;
                }
            }
            if overflow {
                bail!("子进程输出超过 4 MiB 上限");
            }
            Ok(output)
        })();
        let _ = tx.send(result);
    });
    rx
}

/// Decode Windows native redirected output without corrupting device identities.
pub fn decode_output(bytes: &[u8]) -> Result<String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    let little_endian_bom = bytes.starts_with(&[0xff, 0xfe]);
    let big_endian_bom = bytes.starts_with(&[0xfe, 0xff]);
    let sample: Vec<_> = bytes.iter().skip(1).step_by(2).take(128).collect();
    let looks_utf16 =
        sample.len() >= 2 && sample.iter().filter(|&&byte| *byte == 0).count() * 3 > sample.len();
    if little_endian_bom || big_endian_bom || looks_utf16 {
        let content = if little_endian_bom || big_endian_bom {
            &bytes[2..]
        } else {
            bytes
        };
        if !content.len().is_multiple_of(2) {
            bail!("UTF-16 输出不完整");
        }
        let words: Vec<u16> = content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if big_endian_bom {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from_le_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        return String::from_utf16(&words).context("子进程返回无效 UTF-16");
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).context("子进程输出既不是有效 UTF-8，也不是 UTF-16")
}

pub const INSTALL_USBIPD: &str = "未找到 usbipd.exe。请安装 usbipd-win 5.0 或更高版本：\n  winget install --interactive --exact dorssel.usbipd-win\n安装后重新打开终端，再运行 wsl-usb。";

/// Resolve to an absolute path before it can be used for an elevated bind.
pub fn find_usbipd() -> Result<PathBuf> {
    let mut candidates = path_candidates("usbipd.exe");
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(
            PathBuf::from(program_files)
                .join("usbipd-win")
                .join("usbipd.exe"),
        );
    }
    resolve_candidate(candidates).context(INSTALL_USBIPD)
}

pub fn find_wsl() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(windows_dir) = std::env::var_os("SystemRoot") {
        let windows_dir = PathBuf::from(windows_dir);
        candidates.push(windows_dir.join("Sysnative").join("wsl.exe"));
        candidates.push(windows_dir.join("System32").join("wsl.exe"));
    }
    candidates.extend(path_candidates("wsl.exe"));
    resolve_candidate(candidates).context(
        "未找到 wsl.exe。请先安装 WSL2：wsl --install，然后重启并完成 Linux 发行版初始化。",
    )
}

fn find_powershell() -> Result<PathBuf> {
    let windows_dir = std::env::var_os("SystemRoot").context("缺少 SystemRoot 环境变量")?;
    resolve_candidate([
        PathBuf::from(windows_dir).join("System32/WindowsPowerShell/v1.0/powershell.exe")
    ])
    .context("找不到 Windows PowerShell，无法发起 UAC；请使用管理员终端运行")
}

fn path_candidates(name: &str) -> Vec<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|entry| entry.is_absolute())
        .map(|entry| entry.join(name))
        .collect()
}

fn resolve_candidate(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    // Avoid canonicalize(): Windows extended-length \\?\ paths do not work with
    // ShellExecute/Start-Process on all supported systems.
    candidates
        .into_iter()
        .find(|candidate| candidate.is_absolute() && candidate.is_file())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn decodes_wsl_utf16_with_chinese_names() {
        let value = "  NAME          STATE        VERSION\r\n* 开发 Ubuntu    正在运行      2\r\n";
        let encoded: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_output(&encoded).unwrap(), value);
    }

    #[test]
    fn decodes_boms_and_utf8_without_changing_identity() {
        assert_eq!(
            decode_output(b"\xef\xbb\xbfUSB\\VID_1209").unwrap(),
            "USB\\VID_1209"
        );
        assert_eq!(decode_output("设备 😀".as_bytes()).unwrap(), "设备 😀");
        assert_eq!(decode_output(&[0xfe, 0xff, 0, 65]).unwrap(), "A");
        assert!(decode_output(&[0xff, 0xfe, 0]).is_err());
        assert!(decode_output(&[0xff, 0xfe, 0, 0xd8]).is_err());
    }

    #[test]
    fn quotes_powershell_literals_without_interpolation() {
        assert_eq!(
            powershell_literal("C:\\O'Brien\\$x;`test.exe"),
            "'C:\\O''Brien\\$x;`test.exe'"
        );
    }

    #[cfg(windows)]
    #[test]
    fn expired_helper_never_starts_a_process_and_preserves_quoted_inputs() {
        let helper = bind_helper_script(
            Path::new("C:\\missing\\O'Brien\\$(exit 91).exe"),
            "2-3",
            "USB\\x'; [Environment]::Exit(92); #\n中文",
            Duration::from_secs(30),
            0,
        )
        .unwrap();
        let output = execute(
            &find_powershell().unwrap(),
            &["-NoProfile", "-NonInteractive", "-Command", &helper],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.code, Some(124));
        assert!(output.stderr.contains("expired"));
    }

    #[cfg(windows)]
    #[test]
    fn elevation_wrapper_is_valid_windows_powershell_syntax() {
        let script = format!(
            "$tokens = $null; $parseErrors = $null; [void][System.Management.Automation.Language.Parser]::ParseInput({}, [ref]$tokens, [ref]$parseErrors); if ($parseErrors.Count) {{ [Console]::Error.WriteLine(($parseErrors | Out-String)); exit 1 }}",
            powershell_literal(include_str!("platform/elevate.ps1")),
        );
        let output = execute(
            &find_powershell().unwrap(),
            &["-NoProfile", "-Command", &script],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(output.success, "{}", output.stderr);
    }

    #[test]
    fn failed_commands_keep_stderr_and_exit_code() {
        let error = CommandOutput {
            success: false,
            code: Some(5),
            stdout: String::new(),
            stderr: "access denied".into(),
        }
        .ensure_success("bind failed")
        .unwrap_err()
        .to_string();
        assert!(error.contains("access denied") && error.contains('5'));
    }

    #[test]
    fn drains_both_pipes_and_preserves_nonzero_exit() {
        #[cfg(windows)]
        let output = execute(
            &find_powershell().unwrap(),
            &[
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('hello'); [Console]::Error.Write('failure'); exit 7",
            ],
            Duration::from_secs(10),
        )
        .unwrap();
        #[cfg(not(windows))]
        let output = execute(
            Path::new("/bin/sh"),
            &["-c", "printf hello; printf failure >&2; exit 7"],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.stderr, "failure");
        assert_eq!(output.code, Some(7));
        assert!(!output.success);
    }

    #[test]
    fn times_out_a_stuck_process() {
        let started = Instant::now();
        #[cfg(windows)]
        let result = execute(
            &find_powershell().unwrap(),
            &["-NoProfile", "-Command", "Start-Sleep -Seconds 10"],
            Duration::from_millis(100),
        );
        #[cfg(not(windows))]
        let result = execute(Path::new("/bin/sleep"), &["10"], Duration::from_millis(100));
        assert!(result.unwrap_err().to_string().contains("超时"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
