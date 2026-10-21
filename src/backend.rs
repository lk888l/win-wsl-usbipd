//! usbipd/WSL integration with a mockable command boundary.

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    model::{BusId, Device, Distribution, Environment},
    process::CommandRunner,
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const ELEVATION_TIMEOUT: Duration = Duration::from_secs(120);

/// The terminal and CLI depend on operations, not on subprocess details.
pub trait Backend: Send {
    fn inspect(&mut self) -> Result<Environment>;
    fn list_devices(&mut self) -> Result<Vec<Device>>;
    fn attach(&mut self, device: &Device, distribution: &str) -> Result<String>;
    fn detach(&mut self, device: &Device) -> Result<String>;
}

pub struct SystemBackend<R> {
    runner: R,
    usbipd: PathBuf,
    wsl: PathBuf,
}

impl<R: CommandRunner> SystemBackend<R> {
    #[must_use]
    pub fn new(runner: R, usbipd: PathBuf, wsl: PathBuf) -> Self {
        Self {
            runner,
            usbipd,
            wsl,
        }
    }

    fn distributions(&mut self) -> Result<Vec<Distribution>> {
        let verbose = self
            .runner
            .run(&self.wsl, &["--list", "--verbose"], QUERY_TIMEOUT)?
            .ensure_success("查询 WSL 发行版失败")?;
        let names = self
            .runner
            .run(&self.wsl, &["--list", "--quiet"], QUERY_TIMEOUT)?
            .ensure_success("查询 WSL 发行版名称失败")?;
        let running = self
            .runner
            .run(
                &self.wsl,
                &["--list", "--running", "--quiet"],
                QUERY_TIMEOUT,
            )?
            .ensure_success("查询正在运行的 WSL 发行版失败")?;
        parse_distributions(&verbose.stdout, &names.stdout, &running.stdout)
    }

    /// A bus ID can be reused after unplugging. Never act on the replacement
    /// device merely because the user selected that port on an earlier screen.
    fn current_device(&mut self, selected: &Device) -> Result<Device> {
        let devices = self.list_devices()?;
        let Some(current) = devices
            .into_iter()
            .find(|device| device.bus_id == selected.bus_id)
        else {
            bail!(
                "设备 {} 已断开或 BUSID 已改变；请刷新设备列表后重新选择",
                selected.bus_id
            );
        };
        if current.instance_id != selected.instance_id {
            bail!(
                "端口 {} 的设备已被替换；已取消操作，请刷新后重新选择",
                selected.bus_id
            );
        }
        Ok(current)
    }
}

impl<R: CommandRunner> Backend for SystemBackend<R> {
    fn inspect(&mut self) -> Result<Environment> {
        let version = self
            .runner
            .run(&self.usbipd, &["--version"], QUERY_TIMEOUT)?
            .ensure_success("检测 usbipd 版本失败")?;
        let usbipd_version = version
            .stdout
            .trim()
            .trim_start_matches('\u{feff}')
            .to_owned();
        check_usbipd_version(&usbipd_version)?;
        Ok(Environment {
            usbipd_version,
            distributions: self.distributions()?,
        })
    }

    fn list_devices(&mut self) -> Result<Vec<Device>> {
        let output = self
            .runner
            .run(&self.usbipd, &["state"], QUERY_TIMEOUT)?
            .ensure_success("查询 USB 设备失败；请检查 usbipd 服务是否运行")?;
        parse_devices(&output.stdout)
    }

    fn attach(&mut self, selected: &Device, distribution: &str) -> Result<String> {
        let current = self.current_device(selected)?;
        if let Some(client) = current.client_ip {
            return Ok(format!(
                "设备 {} 已连接到 USB/IP 客户端 {client}，未重复映射",
                selected.bus_id
            ));
        }

        let distributions = self.distributions()?;
        let target = distributions
            .iter()
            .find(|target| target.name.eq_ignore_ascii_case(distribution))
            .with_context(|| format!("WSL 发行版 `{distribution}` 已不存在，请重新选择"))?;
        if target.version != 2 {
            bail!(
                "发行版 `{}` 使用 WSL{}，需要 WSL2",
                target.name,
                target.version
            );
        }
        if !target.running {
            bail!(
                "WSL2 发行版 `{}` 尚未运行；请在另一终端执行 wsl --distribution \"{}\" 并保持开启，然后重新映射",
                target.name,
                target.name.replace('"', "`\"")
            );
        }

        // Re-check after the WSL queries and, when needed, after the UAC dialog.
        let current = self.current_device(selected)?;
        if current.client_ip.is_some() {
            return Ok(format!(
                "设备 {} 已被其他操作连接，请刷新查看",
                selected.bus_id
            ));
        }
        let bus_id = selected.bus_id.to_string();
        if !current.shared {
            self.runner
                .elevated_bind(
                    &self.usbipd,
                    &bus_id,
                    &selected.instance_id,
                    ELEVATION_TIMEOUT,
                )?
                .ensure_success("共享 USB 设备失败；首次共享需要接受 Windows UAC 管理员授权")?;
            let bound = self.current_device(selected)?;
            if !bound.shared {
                bail!("usbipd bind 返回成功，但设备 {bus_id} 尚未共享；请刷新后重试");
            }
            if bound.client_ip.is_some() {
                return Ok(format!("设备 {bus_id} 已被其他操作连接，请刷新查看"));
            }
        }

        self.runner
            .run(
                &self.usbipd,
                &["attach", "--wsl", &target.name, "--busid", &bus_id],
                ACTION_TIMEOUT,
            )?
            .ensure_success("映射到 WSL2 失败；设备仍保持共享，可修复问题后重试")?;
        let attached = self.current_device(selected)?;
        if attached.client_ip.is_none() {
            bail!(
                "usbipd attach 返回成功，但设备 {bus_id} 未显示连接状态；请确认 WSL2 保持运行并刷新"
            );
        }
        Ok(format!(
            "设备 {bus_id} 已映射到 WSL2，可在 WSL2 中运行 lsusb 检查"
        ))
    }

    fn detach(&mut self, selected: &Device) -> Result<String> {
        let current = self.current_device(selected)?;
        if current.client_ip.is_none() {
            return Ok(format!("设备 {} 当前未连接，无需断开", selected.bus_id));
        }
        let bus_id = selected.bus_id.to_string();
        self.runner
            .run(
                &self.usbipd,
                &["detach", "--busid", &bus_id],
                ACTION_TIMEOUT,
            )?
            .ensure_success("断开 USB 映射失败")?;
        if self.current_device(selected)?.client_ip.is_some() {
            bail!("usbipd detach 返回成功，但设备 {bus_id} 仍显示已连接；请刷新后检查");
        }
        Ok(format!(
            "设备 {bus_id} 已断开映射，保留共享状态以便下次连接"
        ))
    }
}

fn check_usbipd_version(version: &str) -> Result<()> {
    let major = version
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .with_context(|| {
            format!("无法识别 usbipd 版本 `{version}`，需要 usbipd-win 5.0 或更新版本")
        })?;
    if major < 5 {
        bail!(
            "usbipd {version} 版本过旧，需要 5.0 或更新版本；可运行 winget upgrade --interactive --exact dorssel.usbipd-win"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawState {
    devices: Vec<RawDevice>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawDevice {
    // These fields may explicitly be null, but absence indicates an unexpected
    // automation schema. deserialize_with prevents Serde's implicit None default.
    #[serde(deserialize_with = "Option::deserialize")]
    bus_id: Option<String>,
    description: String,
    instance_id: String,
    #[serde(deserialize_with = "Option::deserialize")]
    persisted_guid: Option<String>,
    #[serde(rename = "ClientIPAddress", deserialize_with = "Option::deserialize")]
    client_ip: Option<String>,
}

fn parse_devices(input: &str) -> Result<Vec<Device>> {
    let state: RawState = serde_json::from_str(input.trim().trim_start_matches('\u{feff}'))
        .context("无法解析 usbipd state JSON；请确认 usbipd-win 版本为 5.0 或更新版本")?;
    let mut devices = Vec::new();
    for raw in state.devices {
        let Some(bus_id) = raw.bus_id else {
            continue;
        };
        if raw.instance_id.trim().is_empty() {
            bail!("usbipd state 返回空设备标识；为避免操作错误设备，已停止查询");
        }
        devices.push(Device {
            bus_id: bus_id.parse::<BusId>().context("usbipd 返回了无效 BUSID")?,
            description: raw.description,
            instance_id: raw.instance_id,
            shared: raw.persisted_guid.is_some(),
            client_ip: raw.client_ip.filter(|value| !value.is_empty()),
        });
    }
    devices.sort_by(|left, right| left.bus_id.cmp(&right.bus_id));
    if devices
        .windows(2)
        .any(|pair| pair[0].bus_id == pair[1].bus_id)
    {
        bail!("usbipd state 返回重复 BUSID；为避免操作错误设备，已停止查询");
    }
    Ok(devices)
}

fn names_from_quiet(input: &str) -> Vec<&str> {
    input
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

fn parse_distributions(verbose: &str, quiet: &str, running: &str) -> Result<Vec<Distribution>> {
    let names = names_from_quiet(quiet);
    let running = names_from_quiet(running);
    let mut distributions = Vec::with_capacity(names.len());
    for &name in &names {
        let mut matches = verbose.lines().filter_map(|line| {
            let line = line.trim().trim_start_matches('\u{feff}');
            let is_default = line.starts_with('*');
            let line = line.strip_prefix('*').unwrap_or(line).trim_start();
            // Two valid names can share a prefix (e.g. Ubuntu / Ubuntu Dev).
            // Match the longest installed name before extracting its state.
            let matched_name = names
                .iter()
                .filter(|candidate| {
                    line.strip_prefix(**candidate)
                        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
                })
                .max_by_key(|candidate| candidate.len())?;
            if *matched_name != name {
                return None;
            }
            let rest = line.strip_prefix(name)?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            let (state, version) = rest.trim().rsplit_once(char::is_whitespace)?;
            let version = version.parse::<u8>().ok()?;
            Some(Distribution {
                name: name.to_owned(),
                version,
                is_default,
                state: state.trim().to_owned(),
                running: running.iter().any(|item| item.eq_ignore_ascii_case(name)),
            })
        });
        let distribution = matches.next().with_context(|| {
            format!("无法解析 WSL 发行版 `{name}` 的版本；请运行 wsl --list --verbose 检查")
        })?;
        if matches.next().is_some() {
            bail!("WSL 列表中的发行版 `{name}` 不唯一，请运行 wsl --list --verbose 检查");
        }
        distributions.push(distribution);
    }
    Ok(distributions)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::{collections::VecDeque, path::Path};

    use crate::process::CommandOutput;

    use super::*;

    struct ExpectedCall {
        elevated: bool,
        args: Vec<String>,
        output: CommandOutput,
    }

    #[derive(Default)]
    struct MockRunner {
        calls: VecDeque<ExpectedCall>,
    }

    impl MockRunner {
        fn expect(&mut self, args: &[&str], stdout: &str) {
            self.calls.push_back(ExpectedCall {
                elevated: false,
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                output: ok(stdout),
            });
        }

        fn expect_bind(&mut self, output: CommandOutput) {
            self.calls.push_back(ExpectedCall {
                elevated: true,
                args: vec!["2-3".into()],
                output,
            });
        }

        fn expect_wsl(&mut self, running: bool) {
            self.expect(
                &["--list", "--verbose"],
                "  NAME       STATE     VERSION\n* Ubuntu     Running   2\n",
            );
            self.expect(&["--list", "--quiet"], "Ubuntu\n");
            self.expect(
                &["--list", "--running", "--quiet"],
                if running { "Ubuntu\n" } else { "" },
            );
        }

        fn take(&mut self, elevated: bool, args: &[&str]) -> CommandOutput {
            let call = self.calls.pop_front().expect("unexpected subprocess call");
            assert_eq!(call.elevated, elevated);
            assert_eq!(call.args, args);
            call.output
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &mut self,
            _program: &Path,
            args: &[&str],
            _timeout: Duration,
        ) -> Result<CommandOutput> {
            Ok(self.take(false, args))
        }

        fn elevated_bind(
            &mut self,
            _program: &Path,
            bus_id: &str,
            expected_instance_id: &str,
            _timeout: Duration,
        ) -> Result<CommandOutput> {
            assert_eq!(expected_instance_id, device().instance_id);
            Ok(self.take(true, &[bus_id]))
        }
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn device() -> Device {
        Device {
            bus_id: "2-3".parse().unwrap(),
            description: "调试器".into(),
            instance_id: "USB\\VID_1234&PID_5678\\original".into(),
            shared: false,
            client_ip: None,
        }
    }

    fn state(shared: bool, attached: bool) -> String {
        serde_json::json!({"Devices": [{
            "BusId": "2-3",
            "Description": "调试器",
            "InstanceId": device().instance_id,
            "PersistedGuid": shared.then_some("guid"),
            "ClientIPAddress": attached.then_some("172.20.0.2")
        }]})
        .to_string()
    }

    fn backend(runner: MockRunner) -> SystemBackend<MockRunner> {
        SystemBackend::new(runner, "usbipd.exe".into(), "wsl.exe".into())
    }

    #[test]
    fn parses_connected_devices_and_excludes_unplugged_persisted_entries() {
        let value = serde_json::json!({"Devices": [
            {"BusId": "2-10", "Description": "设备十", "InstanceId": "ten", "PersistedGuid": "guid", "ClientIPAddress": null, "FutureField": true},
            {"BusId": null, "Description": "已拔出", "InstanceId": "gone", "PersistedGuid": "other-guid", "ClientIPAddress": null},
            {"BusId": "2-2", "Description": "设备二", "InstanceId": "two", "PersistedGuid": null, "ClientIPAddress": "172.20.0.2"}
        ]});
        let devices = parse_devices(&format!("\u{feff}{value}")).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].bus_id.to_string(), "2-2");
        assert_eq!(devices[0].status_label(), "已连接");
        assert_eq!(devices[1].status_label(), "已共享");
    }

    #[test]
    fn rejects_invalid_state_schema_duplicate_ports_and_invalid_ids() {
        for value in ["not-json", "{}", "{\"Devices\":null}"] {
            assert!(parse_devices(value).is_err());
        }
        assert!(parse_devices(&state(false, false).replace("2-3", "2-3;exit")).is_err());
        let mut value: serde_json::Value = serde_json::from_str(&state(false, false)).unwrap();
        let devices = value["Devices"].as_array_mut().unwrap();
        devices.push(devices[0].clone());
        assert!(parse_devices(&value.to_string()).is_err());
    }

    #[test]
    fn nullable_status_fields_must_be_present_in_the_automation_schema() {
        for field in [
            "BusId",
            "Description",
            "InstanceId",
            "PersistedGuid",
            "ClientIPAddress",
        ] {
            let mut value: serde_json::Value = serde_json::from_str(&state(false, false)).unwrap();
            value["Devices"][0].as_object_mut().unwrap().remove(field);
            assert!(
                parse_devices(&value.to_string()).is_err(),
                "accepted missing {field}"
            );
        }
        // Explicit null remains the documented representation of unshared state.
        assert_eq!(parse_devices(&state(false, false)).unwrap().len(), 1);
    }

    #[test]
    fn parses_localized_wsl_states_and_distribution_names_with_spaces() {
        let distributions = parse_distributions(
            "  名称              状态         版本\r\n* Ubuntu Dev       正在 运行     2\r\n  legacy box       已停止        1\r\n",
            "\u{feff}Ubuntu Dev\r\nlegacy box\r\n",
            "Ubuntu Dev\r\n",
        )
        .unwrap();
        assert_eq!(distributions.len(), 2);
        assert_eq!(distributions[0].name, "Ubuntu Dev");
        assert_eq!(distributions[0].state, "正在 运行");
        assert!(distributions[0].running);
        assert!(distributions[0].is_default);
        assert_eq!(distributions[1].version, 1);
        assert!(!distributions[1].running);
        assert!(parse_distributions("bad output", "Ubuntu", "").is_err());
    }

    #[test]
    fn accepts_development_version_suffix_and_rejects_old_usbipd() {
        assert!(check_usbipd_version("5.3.0-54+Branch.master.Sha.aa3db8b").is_ok());
        assert!(check_usbipd_version("4.4.0").is_err());
        assert!(check_usbipd_version("unknown").is_err());
    }

    #[test]
    fn distro_name_prefix_does_not_match_another_distribution() {
        let distributions = parse_distributions(
            "  NAME STATE VERSION\n* Ubuntu        Running 2\n  Ubuntu Dev    Stopped 1\n",
            "Ubuntu\nUbuntu Dev\n",
            "Ubuntu\n",
        )
        .unwrap();
        assert_eq!(distributions[0].name, "Ubuntu");
        assert_eq!(distributions[0].version, 2);
        assert_eq!(distributions[1].name, "Ubuntu Dev");
        assert_eq!(distributions[1].version, 1);
    }

    #[test]
    fn first_attach_elevates_only_bind_and_verifies_result() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(false, false));
        runner.expect_wsl(true);
        runner.expect(&["state"], &state(false, false));
        runner.expect_bind(ok(""));
        runner.expect(&["state"], &state(true, false));
        runner.expect(&["attach", "--wsl", "Ubuntu", "--busid", "2-3"], "");
        runner.expect(&["state"], &state(true, true));
        let mut backend = backend(runner);
        assert!(
            backend
                .attach(&device(), "Ubuntu")
                .unwrap()
                .contains("已映射")
        );
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn shared_device_does_not_request_elevation() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(true, false));
        runner.expect_wsl(true);
        runner.expect(&["state"], &state(true, false));
        runner.expect(&["attach", "--wsl", "Ubuntu", "--busid", "2-3"], "");
        runner.expect(&["state"], &state(true, true));
        let mut backend = backend(runner);
        backend.attach(&device(), "Ubuntu").unwrap();
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn failed_bind_does_not_continue_to_attach() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(false, false));
        runner.expect_wsl(true);
        runner.expect(&["state"], &state(false, false));
        runner.expect_bind(CommandOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: "用户取消授权".into(),
        });
        let mut backend = backend(runner);
        let error = backend.attach(&device(), "Ubuntu").unwrap_err();
        assert!(format!("{error:#}").contains("用户取消授权"));
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn stale_device_identity_is_rejected_before_any_mutation() {
        let mut runner = MockRunner::default();
        runner.expect(
            &["state"],
            &state(false, false).replace("original", "replacement"),
        );
        let mut backend = backend(runner);
        assert!(
            backend
                .attach(&device(), "Ubuntu")
                .unwrap_err()
                .to_string()
                .contains("替换")
        );
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn device_replacement_during_uac_prevents_attach() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(false, false));
        runner.expect_wsl(true);
        runner.expect(&["state"], &state(false, false));
        runner.expect_bind(ok(""));
        runner.expect(
            &["state"],
            &state(true, false).replace("original", "replacement"),
        );
        let mut backend = backend(runner);
        assert!(backend.attach(&device(), "Ubuntu").is_err());
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn stopped_wsl_does_not_bind_device() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(false, false));
        runner.expect_wsl(false);
        let mut backend = backend(runner);
        let error = backend.attach(&device(), "Ubuntu").unwrap_err();
        assert!(error.to_string().contains("另一终端"));
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn attached_device_does_not_get_stolen_or_reattached() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(true, true));
        let mut backend = backend(runner);
        assert!(
            backend
                .attach(&device(), "Ubuntu")
                .unwrap()
                .contains("USB/IP 客户端")
        );
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn attach_success_requires_observed_client() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(true, false));
        runner.expect_wsl(true);
        runner.expect(&["state"], &state(true, false));
        runner.expect(&["attach", "--wsl", "Ubuntu", "--busid", "2-3"], "");
        runner.expect(&["state"], &state(true, false));
        let mut backend = backend(runner);
        assert!(
            backend
                .attach(&device(), "Ubuntu")
                .unwrap_err()
                .to_string()
                .contains("未显示连接")
        );
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn detach_verifies_client_gone_and_retains_sharing() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(true, true));
        runner.expect(&["detach", "--busid", "2-3"], "");
        runner.expect(&["state"], &state(true, false));
        let mut backend = backend(runner);
        assert!(backend.detach(&device()).unwrap().contains("保留共享"));
        assert!(backend.runner.calls.is_empty());
    }

    #[test]
    fn vanished_device_is_not_reported_as_successful_detach() {
        let mut runner = MockRunner::default();
        runner.expect(&["state"], &state(true, true));
        runner.expect(&["detach", "--busid", "2-3"], "");
        runner.expect(&["state"], "{\"Devices\":[]}");
        let mut backend = backend(runner);
        assert!(backend.detach(&device()).is_err());
        assert!(backend.runner.calls.is_empty());
    }
}
