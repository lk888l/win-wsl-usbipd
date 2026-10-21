//! Domain types shared by the command line, terminal UI, and system adapter.

use std::{fmt, str::FromStr};

use anyhow::{Result, bail};
use serde::{Serialize, Serializer};
use thiserror::Error;

/// A Windows USB bus and port pair, validated before it reaches a subprocess.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BusId {
    bus: u16,
    port: u16,
}

#[derive(Debug, Error)]
#[error("无效的 USB BUSID `{0}`；应为正整数组成的“总线-端口”，例如 2-3")]
pub struct InvalidBusId(String);

impl FromStr for BusId {
    type Err = InvalidBusId;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let invalid = || InvalidBusId(value.to_owned());
        let (bus, port) = value.split_once('-').ok_or_else(invalid)?;
        if bus.is_empty()
            || port.is_empty()
            || !bus.bytes().all(|byte| byte.is_ascii_digit())
            || !port.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid());
        }
        let bus = bus.parse::<u16>().map_err(|_| invalid())?;
        let port = port.parse::<u16>().map_err(|_| invalid())?;
        if bus == 0 || port == 0 {
            return Err(invalid());
        }
        Ok(Self { bus, port })
    }
}

impl fmt::Display for BusId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.bus, self.port)
    }
}

impl Serialize for BusId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

/// Only currently connected devices are represented here; remembered devices
/// without a physical bus ID are deliberately excluded by the system adapter.
#[derive(Clone, Debug, Serialize)]
pub struct Device {
    pub bus_id: BusId,
    pub description: String,
    pub instance_id: String,
    pub shared: bool,
    /// usbipd reports any USB/IP client here, not necessarily WSL.
    pub client_ip: Option<String>,
}

impl Device {
    #[must_use]
    pub fn status_label(&self) -> &'static str {
        if self.client_ip.is_some() {
            "已连接"
        } else if self.shared {
            "已共享"
        } else {
            "未共享"
        }
    }

    #[must_use]
    pub fn vid_pid(&self) -> String {
        let upper = self.instance_id.to_ascii_uppercase();
        let component = |prefix: &str| {
            upper.split_once(prefix).and_then(|(_, suffix)| {
                suffix.get(..4).filter(|value| {
                    value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
        };
        match (component("VID_"), component("PID_")) {
            (Some(vid), Some(pid)) => format!("{vid}:{pid}"),
            _ => "----:----".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Distribution {
    pub name: String,
    pub version: u8,
    pub is_default: bool,
    pub state: String,
    /// Derived from `wsl --list --running --quiet`, not a translated state label.
    pub running: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Environment {
    pub usbipd_version: String,
    pub distributions: Vec<Distribution>,
}

impl Environment {
    /// Resolve a distribution without changing WSL's global default.
    pub fn select_distribution(&self, requested: Option<&str>) -> Result<String> {
        if let Some(name) = requested {
            let Some(distribution) = self
                .distributions
                .iter()
                .find(|distribution| distribution.name.eq_ignore_ascii_case(name))
            else {
                bail!("找不到 WSL 发行版 `{name}`；请运行 wsl --list --verbose 查看名称");
            };
            if distribution.version != 2 {
                bail!(
                    "发行版 `{}` 使用 WSL{}，USB 映射需要 WSL2；请先升级该发行版",
                    distribution.name,
                    distribution.version
                );
            }
            return Ok(distribution.name.clone());
        }

        let wsl2: Vec<_> = self
            .distributions
            .iter()
            .filter(|distribution| distribution.version == 2)
            .collect();
        let selected = wsl2
            .iter()
            .find(|distribution| distribution.is_default && distribution.running)
            .or_else(|| {
                wsl2.iter().find(|distribution| {
                    distribution.running && !distribution.name.starts_with("docker-desktop")
                })
            })
            .or_else(|| wsl2.iter().find(|distribution| distribution.running))
            .or_else(|| wsl2.iter().find(|distribution| distribution.is_default))
            .or_else(|| {
                wsl2.iter()
                    .find(|distribution| !distribution.name.starts_with("docker-desktop"))
            })
            .or_else(|| wsl2.first());
        match selected {
            Some(distribution) => Ok(distribution.name.clone()),
            None => bail!(
                "未找到 WSL2 发行版；请先安装并初始化 WSL2，然后运行 wsl --list --verbose 检查"
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn distro(name: &str, version: u8, running: bool, is_default: bool) -> Distribution {
        Distribution {
            name: name.into(),
            version,
            running,
            is_default,
            state: String::new(),
        }
    }

    #[test]
    fn bus_id_rejects_shell_arguments_and_invalid_ports() {
        for value in [
            "", "1", "1-", "-1", "1-2-3", "0-1", "1-0", "65536-1", "1-65536", "1-2;exit", "1-2  ",
            "+1-2", "１-２", "--help",
        ] {
            assert!(value.parse::<BusId>().is_err(), "accepted {value:?}");
        }
        assert_eq!("02-03".parse::<BusId>().unwrap().to_string(), "2-3");
        assert!("2-2".parse::<BusId>().unwrap() < "2-10".parse::<BusId>().unwrap());
        assert_eq!(
            serde_json::to_string(&"2-3".parse::<BusId>().unwrap()).unwrap(),
            "\"2-3\""
        );
    }

    #[test]
    fn distribution_selection_prefers_running_default_then_user_distribution() {
        let mut environment = Environment {
            usbipd_version: "5.0.0".into(),
            distributions: vec![
                distro("legacy", 1, true, true),
                distro("docker-desktop", 2, true, false),
                distro("Ubuntu", 2, true, false),
            ],
        };
        assert_eq!(environment.select_distribution(None).unwrap(), "Ubuntu");
        environment.distributions[1].is_default = true;
        assert_eq!(
            environment.select_distribution(None).unwrap(),
            "docker-desktop"
        );
        assert_eq!(
            environment.select_distribution(Some("ubuntu")).unwrap(),
            "Ubuntu"
        );
        assert!(environment.select_distribution(Some("legacy")).is_err());
        assert!(environment.select_distribution(Some("missing")).is_err());
    }

    #[test]
    fn distribution_selection_can_show_a_stopped_target() {
        let environment = Environment {
            usbipd_version: "5.0.0".into(),
            distributions: vec![distro("Ubuntu", 2, false, true)],
        };
        assert_eq!(environment.select_distribution(None).unwrap(), "Ubuntu");
        assert!(
            Environment {
                distributions: vec![],
                ..environment
            }
            .select_distribution(None)
            .is_err()
        );
    }

    #[test]
    fn device_exposes_vid_pid_without_assuming_every_device_has_one() {
        let mut device = Device {
            bus_id: "2-3".parse().unwrap(),
            description: "设备".into(),
            instance_id: "USB\\VID_046d&PID_c53f\\abc".into(),
            shared: false,
            client_ip: None,
        };
        assert_eq!(device.vid_pid(), "046D:C53F");
        assert_eq!(device.status_label(), "未共享");
        device.shared = true;
        assert_eq!(device.status_label(), "已共享");
        device.client_ip = Some("172.20.0.2".into());
        assert_eq!(device.status_label(), "已连接");
        device.instance_id = "USB\\unusual".into();
        assert_eq!(device.vid_pid(), "----:----");
    }
}
