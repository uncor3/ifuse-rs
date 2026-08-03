use std::{
    net::IpAddr,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

/// The device connection used by a mount.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceTarget {
    Usb {
        udid: Option<String>,
    },
    UsbmuxdNetwork {
        udid: Option<String>,
    },
    Network {
        address: String,
        pairing_file: PathBuf,
    },
}

impl Default for DeviceTarget {
    fn default() -> Self {
        Self::Usb { udid: None }
    }
}

impl DeviceTarget {
    pub fn identity(&self) -> String {
        match self {
            Self::Usb { udid } => format!("usb:{}", udid.as_deref().unwrap_or("first")),
            Self::UsbmuxdNetwork { udid } => {
                format!("usbmuxd-network:{}", udid.as_deref().unwrap_or("first"))
            }
            Self::Network { address, .. } => format!("network:{address}"),
        }
    }
}

/// The AFC service exposed through the mount.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum MountSource {
    #[default]
    Media,
    Root,
    Documents {
        bundle_id: String,
    },
    Container {
        bundle_id: String,
    },
}

/// Configures and starts an iOS filesystem mount.
#[derive(Clone, Debug)]
pub struct IfuseBuilder {
    pub(crate) target: DeviceTarget,
    pub(crate) mount_point: Option<PathBuf>,
    pub(crate) source: MountSource,
    pub(crate) mount_options: Vec<String>,
    pub(crate) volume_label: Option<String>,
}

impl Default for IfuseBuilder {
    fn default() -> Self {
        Self::first_usb()
    }
}

impl IfuseBuilder {
    pub fn first_usb() -> Self {
        Self::with_target(DeviceTarget::default())
    }

    pub fn usb(udid: impl Into<String>) -> Self {
        Self::with_target(DeviceTarget::Usb {
            udid: Some(udid.into()),
        })
    }

    pub fn usbmuxd_network(udid: Option<String>) -> Self {
        Self::with_target(DeviceTarget::UsbmuxdNetwork { udid })
    }

    pub fn network(address: impl Into<String>, pairing_file: impl Into<PathBuf>) -> Self {
        Self::with_target(DeviceTarget::Network {
            address: address.into(),
            pairing_file: pairing_file.into(),
        })
    }

    pub fn with_target(target: DeviceTarget) -> Self {
        Self {
            target,
            mount_point: None,
            source: MountSource::default(),
            mount_options: Vec::new(),
            volume_label: None,
        }
    }

    pub fn mount_point(mut self, path: impl Into<PathBuf>) -> Self {
        self.mount_point = Some(path.into());
        self
    }

    pub fn source(mut self, source: MountSource) -> Self {
        self.source = source;
        self
    }

    pub fn mount_options(mut self, options: impl IntoIterator<Item = String>) -> Self {
        self.mount_options = options.into_iter().collect();
        self
    }

    pub fn volume_label(mut self, label: impl Into<String>) -> Self {
        self.volume_label = Some(label.into());
        self
    }

    pub(crate) fn validate(&self) -> Result<&Path> {
        match &self.target {
            DeviceTarget::Usb { udid } | DeviceTarget::UsbmuxdNetwork { udid }
                if udid.as_ref().is_some_and(|value| value.trim().is_empty()) =>
            {
                return Err(Error::EmptyUdid);
            }
            DeviceTarget::Network {
                address,
                pairing_file,
            } => {
                parse_network_address(address)?;
                if pairing_file.as_os_str().is_empty() {
                    return Err(Error::EmptyPairingFile);
                }
            }
            _ => {}
        }
        if matches!(
            &self.source,
            MountSource::Documents { bundle_id } | MountSource::Container { bundle_id }
                if bundle_id.trim().is_empty()
        ) {
            return Err(Error::EmptyBundleId);
        }
        if self
            .volume_label
            .as_ref()
            .is_some_and(|label| label.trim().is_empty())
        {
            return Err(Error::EmptyVolumeLabel);
        }
        let mount_point = self
            .mount_point
            .as_deref()
            .ok_or(Error::MountpointRequired)?;
        if mount_point.as_os_str().is_empty() {
            return Err(Error::MountpointRequired);
        }
        #[cfg(target_os = "linux")]
        {
            if !mount_point.exists() {
                return Err(Error::MountpointMissing(mount_point.to_path_buf()));
            }
            if !mount_point.is_dir() {
                return Err(Error::MountpointNotDirectory(mount_point.to_path_buf()));
            }
        }
        #[cfg(target_os = "windows")]
        {
            if mount_point.exists() {
                return Err(Error::MountpointExists(mount_point.to_path_buf()));
            }
            if !self.mount_options.is_empty() {
                return Err(Error::WindowsMountOptions);
            }
        }
        Ok(mount_point)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn fuser_config(&self) -> Result<fuser::Config> {
        use fuser::{Config, MountOption, SessionACL};

        let mut config = Config::default();
        config
            .mount_options
            .push(MountOption::FSName("ifuse-rs".into()));
        config
            .mount_options
            .push(MountOption::Subtype("ifuse-rs".into()));
        for raw in &self.mount_options {
            for option in raw.split(',').filter(|value| !value.is_empty()) {
                match option {
                    "allow_other" => config.acl = SessionACL::All,
                    "allow_root" => config.acl = SessionACL::RootAndOwner,
                    "auto_unmount" => config.mount_options.push(MountOption::AutoUnmount),
                    "default_permissions" => {
                        config.mount_options.push(MountOption::DefaultPermissions)
                    }
                    "dev" => config.mount_options.push(MountOption::Dev),
                    "nodev" => config.mount_options.push(MountOption::NoDev),
                    "suid" => config.mount_options.push(MountOption::Suid),
                    "nosuid" => config.mount_options.push(MountOption::NoSuid),
                    "ro" => config.mount_options.push(MountOption::RO),
                    "rw" => config.mount_options.push(MountOption::RW),
                    "exec" => config.mount_options.push(MountOption::Exec),
                    "noexec" => config.mount_options.push(MountOption::NoExec),
                    "atime" => config.mount_options.push(MountOption::Atime),
                    "noatime" => config.mount_options.push(MountOption::NoAtime),
                    "dirsync" => config.mount_options.push(MountOption::DirSync),
                    "sync" => config.mount_options.push(MountOption::Sync),
                    "async" => config.mount_options.push(MountOption::Async),
                    value if value.starts_with("fsname=") => config
                        .mount_options
                        .push(MountOption::FSName(value[7..].into())),
                    value if value.starts_with("subtype=") => config
                        .mount_options
                        .push(MountOption::Subtype(value[8..].into())),
                    value => config.mount_options.push(MountOption::CUSTOM(value.into())),
                }
            }
        }
        Ok(config)
    }
}

pub(crate) fn parse_network_address(input: &str) -> Result<(IpAddr, Option<u32>)> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::InvalidNetworkAddress(
            "network address cannot be empty".into(),
        ));
    }
    let input = input
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(input);
    let (address, scope_id) = match input.rsplit_once('%') {
        Some((address, scope)) if address.contains(':') => {
            let scope_id = scope.parse::<u32>().map_err(|_| {
                Error::InvalidNetworkAddress("IPv6 scope must be a numeric interface ID".into())
            })?;
            (address, Some(scope_id))
        }
        _ => (input, None),
    };
    let address: IpAddr = address
        .parse()
        .map_err(|_| Error::InvalidNetworkAddress(input.into()))?;
    if scope_id.is_some() && !address.is_ipv6() {
        return Err(Error::InvalidNetworkAddress(
            "scope IDs are only valid for IPv6 addresses".into(),
        ));
    }
    Ok((address, scope_id))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppInfo {
    pub bundle_id: String,
    pub version: String,
    pub display_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_and_scoped_ipv6() {
        assert_eq!(
            parse_network_address("192.0.2.1").unwrap(),
            ("192.0.2.1".parse().unwrap(), None)
        );
        assert_eq!(
            parse_network_address("fe80::1%3").unwrap(),
            ("fe80::1".parse().unwrap(), Some(3))
        );
        assert!(parse_network_address("fe80::1%ethernet").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_comma_separated_mount_options() {
        let builder = IfuseBuilder::first_usb()
            .mount_point("/tmp")
            .mount_options(["ro,noexec,allow_other".into()]);
        let parsed = builder.fuser_config().unwrap();
        assert_eq!(parsed.acl, fuser::SessionACL::All);
        assert!(parsed.mount_options.contains(&fuser::MountOption::RO));
    }
}
