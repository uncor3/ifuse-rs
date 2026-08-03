use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use idevice::IdeviceService;
use idevice::afc::AfcClient;
use idevice::afc::opcode::{AfcFopenMode, LinkType};
use idevice::house_arrest::HouseArrestClient;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::pairing_file::PairingFile;
use idevice::provider::{IdeviceProvider, TcpProvider, UsbmuxdProvider};
use idevice::usbmuxd::{Connection, UsbmuxdAddr};

use crate::afc::{AfcTransport, RemoteDeviceInfo, RemoteFileInfo};
use crate::options::{AppInfo, DeviceTarget, MountSource, parse_network_address};
use crate::{Error, Result};

#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) enum BackendCommand {
    DeviceInfo(SyncSender<Result<RemoteDeviceInfo>>),
    FileInfo(String, SyncSender<Result<RemoteFileInfo>>),
    ListDir(String, SyncSender<Result<Vec<String>>>),
    Mkdir(String, SyncSender<Result<()>>),
    Remove(String, SyncSender<Result<()>>),
    Rename(String, String, SyncSender<Result<()>>),
    Link(String, String, LinkType, SyncSender<Result<()>>),
    Open(String, AfcFopenMode, SyncSender<Result<u64>>),
    Read(u64, u64, u32, SyncSender<Result<Vec<u8>>>),
    Write(u64, u64, Vec<u8>, SyncSender<Result<u32>>),
    Close(u64, SyncSender<Result<()>>),
    Truncate(String, u64, SyncSender<Result<()>>),
    TruncateHandle(u64, u64, SyncSender<Result<()>>),
    SetMtime(String, SystemTime, SyncSender<Result<()>>),
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct Backend {
    sender: Sender<BackendCommand>,
    worker: Option<JoinHandle<()>>,
}

impl Backend {
    pub fn start(target: DeviceTarget, source: MountSource) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("ifuse-afc".into())
            .spawn(move || worker(target, source, receiver, ready_tx))
            .map_err(Error::ThreadSpawn)?;
        ready_rx.recv().map_err(|_| Error::BackendStopped)??;
        Ok(Self {
            sender,
            worker: Some(worker),
        })
    }

    pub fn sender(&self) -> Sender<BackendCommand> {
        self.sender.clone()
    }

    pub fn device_info(&self) -> Result<RemoteDeviceInfo> {
        request(&self.sender, BackendCommand::DeviceInfo)
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.sender.send(BackendCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker(
    target: DeviceTarget,
    source: MountSource,
    receiver: Receiver<BackendCommand>,
    ready: SyncSender<Result<()>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(Error::ThreadSpawn(error)));
            return;
        }
    };
    let mut afc = match runtime.block_on(connect(target, source)) {
        Ok(afc) => afc,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    while let Ok(command) = receiver.recv() {
        match command {
            BackendCommand::DeviceInfo(reply) => send(reply, runtime.block_on(afc.device_info())),
            BackendCommand::FileInfo(path, reply) => {
                send(reply, runtime.block_on(afc.file_info(&path)))
            }
            BackendCommand::ListDir(path, reply) => {
                send(reply, runtime.block_on(afc.list_dir(&path)))
            }
            BackendCommand::Mkdir(path, reply) => send(reply, runtime.block_on(afc.mkdir(&path))),
            BackendCommand::Remove(path, reply) => send(reply, runtime.block_on(afc.remove(&path))),
            BackendCommand::Rename(from, to, reply) => {
                send(reply, runtime.block_on(afc.rename(&from, &to)))
            }
            BackendCommand::Link(target, link, kind, reply) => {
                send(reply, runtime.block_on(afc.link(&target, &link, kind)))
            }
            BackendCommand::Open(path, mode, reply) => {
                send(reply, runtime.block_on(afc.open(&path, mode)))
            }
            BackendCommand::Read(handle, offset, size, reply) => {
                send(reply, runtime.block_on(afc.read(handle, offset, size)))
            }
            BackendCommand::Write(handle, offset, bytes, reply) => {
                send(reply, runtime.block_on(afc.write(handle, offset, &bytes)))
            }
            BackendCommand::Close(handle, reply) => {
                send(reply, runtime.block_on(afc.close(handle)))
            }
            BackendCommand::Truncate(path, size, reply) => {
                send(reply, runtime.block_on(afc.truncate(&path, size)))
            }
            BackendCommand::TruncateHandle(handle, size, reply) => {
                send(reply, runtime.block_on(afc.truncate_handle(handle, size)))
            }
            BackendCommand::SetMtime(path, time, reply) => {
                let nanos = time
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .min(u64::MAX as u128) as u64;
                send(reply, runtime.block_on(afc.set_mtime(&path, nanos)))
            }
            BackendCommand::Shutdown => break,
        }
    }
}

async fn connect(target: DeviceTarget, source: MountSource) -> Result<AfcTransport> {
    match target {
        DeviceTarget::Usb { udid } => {
            let provider = usbmuxd_provider(udid.as_deref(), Connection::Usb).await?;
            connect_source(&provider, source).await
        }
        DeviceTarget::UsbmuxdNetwork { udid } => {
            let provider = usbmuxd_provider(
                udid.as_deref(),
                Connection::Network(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
            )
            .await?;
            connect_source(&provider, source).await
        }
        DeviceTarget::Network {
            address,
            pairing_file,
        } => {
            let (addr, scope_id) = parse_network_address(&address)?;
            let provider = TcpProvider {
                addr,
                scope_id,
                pairing_file: PairingFile::read_from_file(pairing_file)?,
                label: "ifuse-rs".into(),
            };
            connect_source(&provider, source).await
        }
    }
}

async fn connect_source(
    provider: &dyn IdeviceProvider,
    source: MountSource,
) -> Result<AfcTransport> {
    let (client, root) = match source {
        MountSource::Media => (AfcClient::connect(provider).await?, "/"),
        MountSource::Root => (AfcClient::new_afc2(provider).await?, "/"),
        MountSource::Documents { bundle_id } => (
            HouseArrestClient::connect(provider)
                .await?
                .vend_documents(bundle_id)
                .await?,
            "/Documents",
        ),
        MountSource::Container { bundle_id } => (
            HouseArrestClient::connect(provider)
                .await?
                .vend_container(bundle_id)
                .await?,
            "/",
        ),
    };
    Ok(AfcTransport::new(client, root))
}

async fn usbmuxd_provider(
    udid: Option<&str>,
    connection_type: Connection,
) -> Result<UsbmuxdProvider> {
    let addr = UsbmuxdAddr::from_env_var()
        .map_err(|error| Error::InvalidAfcResponse(format!("invalid usbmuxd address: {error}")))?;
    let mut connection = addr.connect(0).await?;
    let devices = connection.get_devices().await?;
    let selected = devices.into_iter().find(|candidate| {
        let transport_matches = matches!(
            (&connection_type, &candidate.connection_type),
            (Connection::Usb, Connection::Usb) | (Connection::Network(_), Connection::Network(_))
        );
        let udid_matches = udid.is_none_or(|udid| udid == candidate.udid);
        transport_matches && udid_matches
    });
    selected
        .map(|selected| selected.to_provider(addr, "ifuse-rs"))
        .ok_or(Error::DeviceNotFound)
}

pub(crate) fn list_apps_blocking(target: DeviceTarget) -> Result<Vec<AppInfo>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::ThreadSpawn)?;
    runtime.block_on(async move {
        match target {
            DeviceTarget::Usb { udid } => {
                let provider = usbmuxd_provider(udid.as_deref(), Connection::Usb).await?;
                list_apps_with_provider(&provider).await
            }
            DeviceTarget::UsbmuxdNetwork { udid } => {
                let provider = usbmuxd_provider(
                    udid.as_deref(),
                    Connection::Network(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                )
                .await?;
                list_apps_with_provider(&provider).await
            }
            DeviceTarget::Network {
                address,
                pairing_file,
            } => {
                let (addr, scope_id) = parse_network_address(&address)?;
                let provider = TcpProvider {
                    addr,
                    scope_id,
                    pairing_file: PairingFile::read_from_file(pairing_file)?,
                    label: "ifuse-rs".into(),
                };
                list_apps_with_provider(&provider).await
            }
        }
    })
}

async fn list_apps_with_provider(provider: &dyn IdeviceProvider) -> Result<Vec<AppInfo>> {
    let mut client = InstallationProxyClient::connect(provider).await?;
    let apps = client.get_apps(Some("Any"), None).await?;
    let mut output = apps
        .into_iter()
        .filter_map(|(bundle_id, value)| {
            let dictionary = value.as_dictionary()?;
            if !dictionary
                .get("UIFileSharingEnabled")
                .and_then(plist::Value::as_boolean)
                .unwrap_or(false)
            {
                return None;
            }
            Some(AppInfo {
                bundle_id,
                version: string_value(dictionary.get("CFBundleVersion")),
                display_name: string_value(dictionary.get("CFBundleDisplayName")),
            })
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| left.bundle_id.cmp(&right.bundle_id));
    Ok(output)
}

fn string_value(value: Option<&plist::Value>) -> String {
    value
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_owned()
}

fn send<T>(reply: SyncSender<Result<T>>, value: Result<T>) {
    let _ = reply.send(value);
}

pub(crate) fn request<T>(
    sender: &Sender<BackendCommand>,
    make: impl FnOnce(SyncSender<Result<T>>) -> BackendCommand,
) -> Result<T> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    sender
        .send(make(reply_tx))
        .map_err(|_| Error::BackendStopped)?;
    reply_rx.recv().map_err(|_| Error::BackendStopped)?
}
