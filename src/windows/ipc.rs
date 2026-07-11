use std::{
    collections::hash_map::DefaultHasher,
    ffi::OsStr,
    hash::{Hash, Hasher},
    io,
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    ptr,
    sync::mpsc,
    thread,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
    },
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    },
};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

pub fn pipe_name() -> String {
    let identity = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    format!(
        r"\\.\pipe\GiantFileExchange.Commands.{:016x}",
        hasher.finish()
    )
}

pub fn start_pipe_server(sender: mpsc::Sender<Vec<PathBuf>>) -> Result<thread::JoinHandle<()>> {
    let name = wide(OsStr::new(&pipe_name()));
    thread::Builder::new()
        .name("file-exchange-ipc".to_owned())
        .spawn(move || {
            loop {
                let handle = unsafe {
                    CreateNamedPipeW(
                        name.as_ptr(),
                        PIPE_ACCESS_INBOUND,
                        PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                        PIPE_UNLIMITED_INSTANCES,
                        4096,
                        MAX_MESSAGE_BYTES as u32,
                        0,
                        ptr::null(),
                    )
                };
                if handle == INVALID_HANDLE_VALUE {
                    thread::sleep(std::time::Duration::from_millis(250));
                    continue;
                }
                let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) } != 0
                    || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                if connected {
                    let mut buffer = vec![0_u8; MAX_MESSAGE_BYTES];
                    let mut read = 0_u32;
                    if unsafe {
                        ReadFile(
                            handle,
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                            &mut read,
                            ptr::null_mut(),
                        )
                    } != 0
                    {
                        buffer.truncate(read as usize);
                        if let Ok(paths) = serde_json::from_slice::<Vec<PathBuf>>(&buffer) {
                            let _ = sender.send(paths);
                        }
                    }
                }
                unsafe {
                    DisconnectNamedPipe(handle);
                    CloseHandle(handle);
                }
            }
        })
        .context("无法启动单实例命名管道")
}

pub fn forward_paths(paths: &[PathBuf]) -> Result<()> {
    let payload = serde_json::to_vec(paths)?;
    if payload.len() > MAX_MESSAGE_BYTES {
        bail!("文件参数过多，无法发送给已运行的应用");
    }
    let name = wide(OsStr::new(&pipe_name()));
    let ready = unsafe { WaitNamedPipeW(name.as_ptr(), 3000) };
    if ready == 0 {
        return Err(io::Error::last_os_error()).context("已运行的应用尚未准备好接收文件");
    }
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("无法连接已运行的应用");
    }
    let mut written = 0_u32;
    let succeeded = unsafe {
        WriteFile(
            handle,
            payload.as_ptr(),
            payload.len() as u32,
            &mut written,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if succeeded == 0 || written as usize != payload.len() {
        return Err(io::Error::last_os_error()).context("无法将文件发送给已运行的应用");
    }
    Ok(())
}
