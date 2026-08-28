use super::WindowsSandboxStandaloneOutcome;
use super::WindowsSandboxStandaloneRootOutcome;
use super::validate_native_os_str;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::ffi::OsStringExt;

pub(super) const MAX_FRAME_BYTES: usize = 1024 * 1024;
const STANDALONE_WIRE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WireTokenMode {
    ReadOnly,
    WritableRoots,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct WireNativeString(Vec<u16>);

impl WireNativeString {
    pub(super) fn from_os_str(value: &OsStr) -> Result<Self> {
        validate_native_os_str(value, "native Windows value")?;
        Ok(Self(value.encode_wide().collect()))
    }

    pub(super) fn into_os_string(self) -> Result<OsString> {
        if self.0.contains(&0) {
            anyhow::bail!("standalone helper received a native value containing NUL");
        }
        Ok(OsString::from_wide(&self.0))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "handle")]
pub(super) enum WireStream {
    Handle(u64),
    Null,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireSpawnRequest {
    pub(super) program: WireNativeString,
    pub(super) args: Vec<WireNativeString>,
    pub(super) environment: Vec<(WireNativeString, WireNativeString)>,
    pub(super) cwd: WireNativeString,
    pub(super) state_dir: WireNativeString,
    pub(super) token_mode: WireTokenMode,
    pub(super) capability_sids: Vec<String>,
    pub(super) network_proxy_restricting_sid: Option<String>,
    pub(super) stdin: WireStream,
    pub(super) stdout: WireStream,
    pub(super) stderr: WireStream,
    pub(super) use_private_desktop: bool,
    pub(super) descendant_grace_ms: u64,
    pub(super) force_stop_timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    tag = "type",
    content = "payload",
    deny_unknown_fields
)]
pub(super) enum ParentMessage {
    Spawn(Box<WireSpawnRequest>),
    CommitLaunch,
    ForceTerminate { force_stop_timeout_ms: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    tag = "type",
    content = "payload",
    deny_unknown_fields
)]
pub(super) enum HelperMessage {
    Ready {
        process_id: u32,
    },
    Committed,
    RootExited(WindowsSandboxStandaloneRootOutcome),
    Final(WindowsSandboxStandaloneOutcome),
    Error {
        stage: String,
        message: String,
        windows_error_code: Option<u32>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFrame<T> {
    version: u32,
    message: T,
}

pub(super) fn write_wire_frame<T: Serialize>(writer: &mut File, message: T) -> Result<()> {
    let payload = serde_json::to_vec(&WireFrame {
        version: STANDALONE_WIRE_VERSION,
        message,
    })?;
    if payload.len() > MAX_FRAME_BYTES {
        anyhow::bail!("standalone Windows helper frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    let length = u32::try_from(payload.len()).context("standalone frame length overflow")?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub(super) fn read_wire_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut File,
) -> Result<Option<T>> {
    let mut length = [0u8; 4];
    match reader.read(&mut length[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("one-byte read returned more than one byte"),
        Err(error) => return Err(error.into()),
    }
    reader
        .read_exact(&mut length[1..])
        .context("truncated standalone Windows frame length")?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        anyhow::bail!("standalone Windows helper frame length {length} exceeds {MAX_FRAME_BYTES}");
    }
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .context("truncated standalone Windows frame payload")?;
    let frame: WireFrame<T> =
        serde_json::from_slice(&payload).context("malformed standalone Windows helper JSON")?;
    if frame.version != STANDALONE_WIRE_VERSION {
        anyhow::bail!(
            "unsupported standalone Windows helper protocol version {}",
            frame.version
        );
    }
    Ok(Some(frame.message))
}
