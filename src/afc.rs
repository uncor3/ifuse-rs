use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use idevice::afc::errors::AfcError;
use idevice::afc::opcode::{AfcFopenMode, AfcOpcode, LinkType};
use idevice::afc::packet::{AfcPacket, AfcPacketHeader};
use idevice::afc::{AfcClient, MAGIC};
use idevice::{Idevice, IdeviceError};

use crate::{Error, Result};

const MAX_TRANSFER: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteFileType {
    RegularFile,
    Directory,
    Symlink,
    BlockDevice,
    CharDevice,
    NamedPipe,
    Socket,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteFileInfo {
    pub size: u64,
    pub blocks: u64,
    pub created: SystemTime,
    pub modified: SystemTime,
    pub nlink: u32,
    pub kind: RemoteFileType,
    pub link_target: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteDeviceInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub block_size: u32,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub model: String,
}

/// A thin AFC request layer over an `idevice`-established service stream.
pub(crate) struct AfcTransport {
    idevice: Idevice,
    packet_number: u64,
    root: String,
}

impl AfcTransport {
    pub fn new(client: AfcClient, root: impl Into<String>) -> Self {
        Self {
            idevice: client.idevice,
            packet_number: 0,
            root: root.into(),
        }
    }

    fn path(&self, path: &str) -> String {
        if self.root == "/" {
            if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            }
        } else if path == "/" || path.is_empty() {
            self.root.clone()
        } else {
            format!("{}{}", self.root.trim_end_matches('/'), path)
        }
    }

    async fn request(
        &mut self,
        operation: AfcOpcode,
        header_payload: Vec<u8>,
        payload: Vec<u8>,
    ) -> Result<AfcPacket> {
        let header_payload_len = AfcPacketHeader::LEN + header_payload.len() as u64;
        let packet = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: header_payload_len + payload.len() as u64,
                header_payload_len,
                packet_num: self.packet_number,
                operation,
            },
            header_payload,
            payload,
        };
        self.packet_number = self.packet_number.wrapping_add(1);
        self.idevice.send_raw(&packet.serialize()).await?;
        let response = AfcPacket::read(&mut self.idevice).await?;
        if response.header.operation == AfcOpcode::Status {
            let bytes = response.header_payload.get(..8).ok_or_else(|| {
                Error::InvalidAfcResponse("status packet has no error code".into())
            })?;
            let code = u64::from_le_bytes(bytes.try_into().expect("eight-byte slice"));
            let error = AfcError::from(code);
            if error != AfcError::Success {
                return Err(Error::Device(IdeviceError::Afc(error)));
            }
        }
        Ok(response)
    }

    pub async fn list_dir(&mut self, path: &str) -> Result<Vec<String>> {
        let response = self
            .request(AfcOpcode::ReadDir, nul(self.path(path)), Vec::new())
            .await?;
        Ok(strings(&response.payload))
    }

    pub async fn file_info(&mut self, path: &str) -> Result<RemoteFileInfo> {
        let response = self
            .request(AfcOpcode::GetFileInfo, nul(self.path(path)), Vec::new())
            .await?;
        let mut values = dictionary(&response.payload)?;
        let kind = match take(&mut values, "st_ifmt")?.as_str() {
            "S_IFREG" => RemoteFileType::RegularFile,
            "S_IFDIR" => RemoteFileType::Directory,
            "S_IFLNK" => RemoteFileType::Symlink,
            "S_IFBLK" => RemoteFileType::BlockDevice,
            "S_IFCHR" => RemoteFileType::CharDevice,
            "S_IFIFO" => RemoteFileType::NamedPipe,
            "S_IFSOCK" => RemoteFileType::Socket,
            value => {
                return Err(Error::InvalidAfcResponse(format!(
                    "unknown file type {value}"
                )));
            }
        };
        Ok(RemoteFileInfo {
            size: parse(&mut values, "st_size")?,
            blocks: parse(&mut values, "st_blocks")?,
            created: timestamp(values.remove("st_birthtime")),
            modified: timestamp(values.remove("st_mtime")),
            nlink: parse(&mut values, "st_nlink")?,
            kind,
            link_target: values
                .remove("LinkTarget")
                .or_else(|| values.remove("st_link_target")),
        })
    }

    pub async fn device_info(&mut self) -> Result<RemoteDeviceInfo> {
        let response = self
            .request(AfcOpcode::GetDevInfo, Vec::new(), Vec::new())
            .await?;
        let mut values = dictionary(&response.payload)?;
        Ok(RemoteDeviceInfo {
            total_bytes: parse(&mut values, "FSTotalBytes")?,
            free_bytes: parse(&mut values, "FSFreeBytes")?,
            block_size: parse(&mut values, "FSBlockSize")?,
            model: values.remove("Model").unwrap_or_default(),
        })
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<()> {
        self.request(AfcOpcode::MakeDir, nul(self.path(path)), Vec::new())
            .await?;
        Ok(())
    }

    pub async fn remove(&mut self, path: &str) -> Result<()> {
        self.request(AfcOpcode::RemovePath, nul(self.path(path)), Vec::new())
            .await?;
        Ok(())
    }

    pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let mut data = nul(self.path(from));
        data.extend(nul(self.path(to)));
        self.request(AfcOpcode::RenamePath, data, Vec::new())
            .await?;
        Ok(())
    }

    pub async fn link(&mut self, target: &str, link: &str, kind: LinkType) -> Result<()> {
        let mut data = (kind as u64).to_le_bytes().to_vec();
        data.extend(nul(self.path(target)));
        data.extend(nul(self.path(link)));
        self.request(AfcOpcode::MakeLink, data, Vec::new()).await?;
        Ok(())
    }

    pub async fn open(&mut self, path: &str, mode: AfcFopenMode) -> Result<u64> {
        let mut data = (mode as u64).to_le_bytes().to_vec();
        data.extend(nul(self.path(path)));
        let response = self.request(AfcOpcode::FileOpen, data, Vec::new()).await?;
        let bytes = response.header_payload.get(..8).ok_or_else(|| {
            Error::InvalidAfcResponse("file-open response has no descriptor".into())
        })?;
        Ok(u64::from_le_bytes(
            bytes.try_into().expect("eight-byte slice"),
        ))
    }

    async fn seek(&mut self, handle: u64, offset: u64) -> Result<()> {
        let mut data = handle.to_le_bytes().to_vec();
        data.extend(0_u64.to_le_bytes());
        data.extend((offset as i64).to_le_bytes());
        self.request(AfcOpcode::FileSeek, data, Vec::new()).await?;
        Ok(())
    }

    pub async fn read(&mut self, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        self.seek(handle, offset).await?;
        let mut output = Vec::with_capacity(size as usize);
        let mut remaining = size as usize;
        while remaining > 0 {
            let amount = remaining.min(MAX_TRANSFER);
            let mut data = handle.to_le_bytes().to_vec();
            data.extend((amount as u64).to_le_bytes());
            let response = self.request(AfcOpcode::Read, data, Vec::new()).await?;
            let count = response.payload.len();
            output.extend(response.payload);
            if count == 0 || count < amount {
                break;
            }
            remaining -= count;
        }
        Ok(output)
    }

    pub async fn write(&mut self, handle: u64, offset: u64, bytes: &[u8]) -> Result<u32> {
        self.seek(handle, offset).await?;
        for chunk in bytes.chunks(MAX_TRANSFER) {
            self.request(
                AfcOpcode::Write,
                handle.to_le_bytes().to_vec(),
                chunk.to_vec(),
            )
            .await?;
        }
        Ok(bytes.len() as u32)
    }

    pub async fn close(&mut self, handle: u64) -> Result<()> {
        self.request(
            AfcOpcode::FileClose,
            handle.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await?;
        Ok(())
    }

    pub async fn truncate(&mut self, path: &str, size: u64) -> Result<()> {
        let mut data = size.to_le_bytes().to_vec();
        data.extend(nul(self.path(path)));
        self.request(AfcOpcode::Truncate, data, Vec::new()).await?;
        Ok(())
    }

    pub async fn truncate_handle(&mut self, handle: u64, size: u64) -> Result<()> {
        let mut data = handle.to_le_bytes().to_vec();
        data.extend(size.to_le_bytes());
        self.request(AfcOpcode::FileSetSize, data, Vec::new())
            .await?;
        Ok(())
    }

    pub async fn set_mtime(&mut self, path: &str, nanos: u64) -> Result<()> {
        let mut data = nanos.to_le_bytes().to_vec();
        data.extend(nul(self.path(path)));
        self.request(AfcOpcode::SetFileTime, data, Vec::new())
            .await?;
        Ok(())
    }
}

fn nul(value: String) -> Vec<u8> {
    let mut bytes = value.into_bytes();
    bytes.push(0);
    bytes
}

fn strings(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
        .map(|item| String::from_utf8_lossy(item).into_owned())
        .collect()
}

fn dictionary(bytes: &[u8]) -> Result<HashMap<String, String>> {
    let strings = strings(bytes);
    if !strings.len().is_multiple_of(2) {
        return Err(Error::InvalidAfcResponse("malformed key/value data".into()));
    }
    Ok(strings
        .chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect())
}

fn take(values: &mut HashMap<String, String>, key: &str) -> Result<String> {
    values
        .remove(key)
        .ok_or_else(|| Error::InvalidAfcResponse(format!("missing {key}")))
}

fn parse<T: std::str::FromStr>(values: &mut HashMap<String, String>, key: &str) -> Result<T> {
    take(values, key)?
        .parse()
        .map_err(|_| Error::InvalidAfcResponse(format!("invalid {key}")))
}

fn timestamp(value: Option<String>) -> SystemTime {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .map(|nanos| UNIX_EPOCH + Duration::from_nanos(nanos))
        .unwrap_or(UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_afc_dictionary() {
        let values = dictionary(b"st_size\x0012\x00st_ifmt\x00S_IFREG\x00").unwrap();
        assert_eq!(values["st_size"], "12");
        assert_eq!(values["st_ifmt"], "S_IFREG");
    }

    #[test]
    fn rejects_malformed_afc_dictionary() {
        assert!(dictionary(b"st_size\x0012\x00dangling\x00").is_err());
    }
}
