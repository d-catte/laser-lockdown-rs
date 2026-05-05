use crate::net;
use crate::net::{hash_string, UserInfo, LOGS, USERS};
use crate::sd_utils::{retry_with_backoff, DummyTimeSource};
use alloc::format;
use alloc::string::{String, ToString};
use core::cell::RefCell;
use core::str::FromStr;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embedded_hal_bus::spi::RefCellDevice;
use embedded_sdmmc::{Mode, SdCard, VolumeIdx, VolumeManager};
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::spi::master::Spi;
use esp_hal::Blocking;
use esp_println::println;
use static_cell::StaticCell;

pub const LOG_PACKET_CHAR_COUNT: usize = 1024;
const LOG_MAX_SIZE: u32 = 4096;

pub type SdDevice =
SdCard<RefCellDevice<'static, Spi<'static, Blocking>, Output<'static>, Delay>, Delay>;

/// Interface for SD card's SPI
pub static SPI_BUS: StaticCell<RefCell<Spi<Blocking>>> = StaticCell::new();

/// File manager for the SD card
pub static VOLUME_MGR: StaticCell<RefCell<VolumeManager<SdDevice, DummyTimeSource>>> =
    StaticCell::new();

#[derive(Copy, Clone)]
pub enum FileNameEnum {
    Log,
    Users,
    Password,
    Temp,
}

impl FileNameEnum {
    fn as_str(&self) -> &'static str {
        match self {
            FileNameEnum::Log => "LOG.TXT",
            FileNameEnum::Users => "USER.TXT",
            FileNameEnum::Password => "PSWD.TXT",
            FileNameEnum::Temp => "TMP.TXT",
        }
    }
}

pub struct SdStorage<'a> {
    _spi_bus: &'a RefCell<Spi<'a, Blocking>>,
    volume_mgr: &'a VolumeManager<SdCard<RefCellDevice<'a, Spi<'a, Blocking>, Output<'a>, Delay>, Delay>, DummyTimeSource>,
}

impl<'a> SdStorage<'a> {

    pub fn new(
        volume_mgr: &'a VolumeManager<SdCard<RefCellDevice<'a, Spi<'a, Blocking>, Output<'a>, Delay>, Delay>, DummyTimeSource>,
        spi_bus_ref: &'a RefCell<Spi<'a, Blocking>>,
    ) -> Self {
        Self {
            _spi_bus: spi_bus_ref,
            volume_mgr,
        }
    }

    async fn write(&mut self, file_name: FileNameEnum, data: &str, overwrite: bool) {
        let mode = if overwrite {
            Mode::ReadWriteCreateOrTruncate
        } else {
            Mode::ReadWriteCreateOrAppend
        };

        retry_with_backoff("SD_Write", || async {
            let volume = self.volume_mgr
                .open_volume(VolumeIdx(0))
                .map_err(|_| "VolOpenErr")?;

            let root = volume
                .open_root_dir()
                .map_err(|_| "RootOpenErr")?;

            let f = root
                .open_file_in_dir(file_name.as_str(), mode)
                .map_err(|_| "FileOpenErr")?;

            f.write(data.as_bytes()).map_err(|_| "WriteErr")?;

            f.flush().map_err(|_| "FlushErr")?;
            f.close().map_err(|_| "CloseErr")?;
            root.close().map_err(|_| "CloseErr")?;
            volume.close().map_err(|_| "CloseErr")?;

            Ok::<(), &'static str>(())
        })
            .await;
    }

    async fn read(&mut self, file: FileNameEnum, line_number: Option<usize>) -> String {
        let result = retry_with_backoff("SD_Read", || async {
            let mut output = String::new();
            let mut buffer = [0u8; 128];
            let mut current_line = 0;

            let volume = self.volume_mgr.open_volume(VolumeIdx(0))
                .map_err(|_| "VolOpenErr")?;

            let root = volume.open_root_dir()
                .map_err(|_| "RootOpenErr")?;

            let f = match root.open_file_in_dir(file.as_str(), Mode::ReadOnly) {
                Ok(file_handle) => file_handle,
                Err(_) => {
                    root.open_file_in_dir(file.as_str(), Mode::ReadWriteCreateOrAppend)
                        .map_err(|_| "FileCreateErr")?
                }
            };

            while !f.is_eof() {
                let read_count = f.read(&mut buffer).map_err(|_| "ReadErr")?;
                if read_count == 0 { break; }

                let contents = core::str::from_utf8(&buffer[..read_count]).unwrap_or("");

                if let Some(target) = line_number {
                    for line in contents.lines() {
                        if current_line == target {
                            f.close().map_err(|_| "CloseErr")?;
                            root.close().map_err(|_| "CloseErr")?;
                            volume.close().map_err(|_| "CloseErr")?;
                            return Ok::<String, &'static str>(String::from(line));
                        }
                        current_line += 1;
                    }
                } else {
                    output.push_str(contents);
                }
            }

            f.close().map_err(|_| "CloseErr")?;
            root.close().map_err(|_| "CloseErr")?;
            volume.close().map_err(|_| "CloseErr")?;

            Ok::<String, &'static str>(output)
        }).await;

        result.unwrap_or_else(|| String::from(""))
    }

    /// Gets the last line of a file
    pub async fn read_last_line(&mut self, file: FileNameEnum) -> Option<String> {
        retry_with_backoff("SD_Read_Last_Line", || async {
            let volume = self.volume_mgr.open_volume(VolumeIdx(0))
                .map_err(|_| "VolOpenErr")?;

            let root = volume.open_root_dir()
                .map_err(|_| "RootOpenErr")?;
            
            let f = match root.open_file_in_dir(file.as_str(), Mode::ReadOnly) {
                Ok(file_handle) => file_handle,
                Err(_) => {
                    root.open_file_in_dir(file.as_str(), Mode::ReadWriteCreateOrAppend)
                        .map_err(|_| "FileCreateErr")?
                }
            };

            let file_len = f.length();
            if file_len == 0 {
                f.close().map_err(|_| "CloseErr")?;
                root.close().map_err(|_| "CloseErr")?;
                volume.close().map_err(|_| "CloseErr")?;
                return Ok::<Option<String>, &'static str>(None);
            }

            let mut buffer = [0u8; 128];
            let mut last_line = String::new();
            
            let mut pos = file_len;
            let mut found_newline = false;

            while pos > 0 && !found_newline {
                let chunk_size = core::cmp::min(pos, buffer.len() as u32);
                pos -= chunk_size;

                f.seek_from_start(pos).map_err(|_| "SeekErr")?;
                let bytes_read = f.read(&mut buffer[..chunk_size as usize]).map_err(|_| "ReadErr")?;

                let chunk = &buffer[..bytes_read];
                
                for &byte in chunk.iter().rev() {
                    if byte == b'\n' {
                        if !last_line.is_empty() {
                            found_newline = true;
                            break;
                        }
                    } else if byte != b'\r' {
                        last_line.insert(0, byte as char);
                    }
                }
            }
            
            f.close().map_err(|_| "CloseErr")?;
            root.close().map_err(|_| "CloseErr")?;
            volume.close().map_err(|_| "CloseErr")?;

            if last_line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(last_line))
            }
        }).await.flatten()
    }

    fn remove(&mut self, file: FileNameEnum) {
        if let Ok(volume) = self.volume_mgr.open_volume(VolumeIdx(0)) && let Ok(root) = volume.open_root_dir() {
            let _ = root.delete_file_in_dir(file.as_str());
        }
    }

    async fn truncate(&mut self, file_enum: FileNameEnum, threshold: u32, truncate_to: u32) {
        // Read the "keep" portion (the end of the file)
        let keep_content = self.read_from_end(file_enum, truncate_to as usize).await;

        if let Ok(volume) = self.volume_mgr.open_volume(VolumeIdx(0))
            && let Ok(root) = volume.open_root_dir() {
            // 1. Check file size
            let file_size = if let Ok(file) = root.open_file_in_dir(file_enum.as_str(), Mode::ReadOnly) {
                file.length()
            } else {
                0
            };

            // 2. If it exceeds threshold, perform the truncation
            if file_size > threshold {
                // Re-open in Truncate mode to wipe it and write the kept portion
                if let Ok(file) = root.open_file_in_dir(file_enum.as_str(), Mode::ReadWriteCreateOrTruncate) {
                    let _ = file.write(keep_content.as_bytes());
                    println!("File {} truncated ({} -> {})", file_enum.as_str(), file_size, truncate_to);
                }
            }
        }
    }

    async fn read_from_end(&mut self, file_enum: FileNameEnum, num_chars: usize) -> String {
        let result = retry_with_backoff("SD_Read_End", || async {
            let mut output = String::new();

            let volume = self.volume_mgr.open_volume(VolumeIdx(0))
                .map_err(|_| "VolOpenErr")?;

            let root = volume.open_root_dir()
                .map_err(|_| "RootOpenErr")?;

            let file = match root.open_file_in_dir(file_enum.as_str(), Mode::ReadOnly) {
                Ok(file_handle) => file_handle,
                Err(_) => {
                    root.open_file_in_dir(file_enum.as_str(), Mode::ReadWriteCreateOrAppend)
                        .map_err(|_| "FileCreateErr")?
                }
            };

            let file_len = file.length() as usize;

            let start_pos = file_len.saturating_sub(num_chars);
            file.seek_from_start(start_pos as u32).map_err(|_| "SeekErr")?;

            let mut buffer = [0u8; 256];
            while !file.is_eof() {
                let n = file.read(&mut buffer).map_err(|_| "ReadErr")?;
                if n == 0 { break; }

                let s = core::str::from_utf8(&buffer[..n]).unwrap_or("");
                output.push_str(s)
            }

            file.close().map_err(|_| "CloseErr")?;
            root.close().map_err(|_| "CloseErr")?;
            volume.close().map_err(|_| "CloseErr")?;

            Ok::<String, &'static str>(output)
        }).await;

        result.unwrap_or_else(|| String::from(""))
    }

    /// Determines if the file has data in it
    pub fn is_file_empty(&self, file_enum: FileNameEnum) -> bool {
        let Ok(volume) = self.volume_mgr.open_volume(VolumeIdx(0)) else {
            return true;
        };

        let Ok(root) = volume.open_root_dir() else {
            let _ = volume.close();
            return true;
        };

        let length = {
            let file_result = root
                .open_file_in_dir(file_enum.as_str(), Mode::ReadOnly)
                .or_else(|_| {
                    root.open_file_in_dir(
                        file_enum.as_str(),
                        Mode::ReadWriteCreateOrAppend,
                    )
                });

            match file_result {
                Ok(f) => {
                    let len = f.length();
                    let _ = f.close();
                    Some(len)
                }
                Err(_) => None,
            }
        };

        let _ = root.close();
        let _ = volume.close();

        match length {
            Some(len) => len == 0,
            None => true,
        }
    }

    // LOGS

    /// Deletes the log file from the SD card
    pub fn clear_logs(&mut self) {
        self.remove(FileNameEnum::Log);
    }

    /// Reads the last 1024 characters of the log file
    pub async fn read_logs(&mut self) -> String {
        self.read_from_end(FileNameEnum::Log, LOG_PACKET_CHAR_COUNT).await
    }

    async fn read_logs_raw(&mut self) -> String {
        self.read(FileNameEnum::Log, None).await
    }

    /// Logs a message to the logs
    pub async fn log_message(&mut self, message: String, timestamp: Option<heapless::String<17>>) {
        println!("Attempting to write log to file");
        let log_file = FileNameEnum::Log;

        // Check current file size
        let mut current_size = 0;
        if let Ok(volume) = self.volume_mgr.open_volume(VolumeIdx(0))
            && let Ok(root) = volume.open_root_dir()
            && let Ok(file) = root.open_file_in_dir(log_file.as_str(), Mode::ReadOnly) {
            current_size = file.length();
        }

        // If size >= threshold, truncate the file
        if current_size >= LOG_MAX_SIZE {
            self.truncate(log_file, LOG_MAX_SIZE, LOG_PACKET_CHAR_COUNT as u32).await;
            println!("Log threshold reached. Truncated to {} bytes.", LOG_PACKET_CHAR_COUNT);
        }

        // Append the new message
        let formatted_message = if let Some(timestamp) = timestamp {
            format!("{} {}\n",timestamp, message)
        } else {
            format!("{}\n", message)
        };
        self.write(log_file, &formatted_message, false).await;
        println!("Log message sent");
        let file_contents = self.read_logs_raw().await;
        println!("Setting Cache to {}", file_contents);
        if let Some(mutex) = LOGS.try_get() {
            if let Ok(mut logs_guard) = mutex.try_lock() {
                *logs_guard = heapless::String::from_str(&*file_contents).unwrap();
                println!("Updated logs cache: {:?}", logs_guard);
            }
        }
    }

    /// Gets the current time as a timestamp
    pub async fn get_timestamp(
        &self,
        time_request: &'static Signal<CriticalSectionRawMutex, ()>,
        time_response: &'static Signal<CriticalSectionRawMutex, Option<heapless::String<17>>>,
    ) -> heapless::String<17> {
        time_request.signal(());
        let response = time_response.wait().await;
        if let Some(date) = response {
            date
        } else {
            heapless::String::new()
        }
    }

    // Users
    /// Adds a user to the database
    pub async fn add_user(&mut self, id: u64, name: Option<String>) {
        let hashed_id = net::hash_id(id).await;
        println!("Adding user with hashed id {}", hashed_id);
        // Line: "id:name\n"
        let entry = if let Some(name) = name {
            format!("{}:{}\n", hashed_id, name)
        } else {
           format!("{}:NULL\n", hashed_id)
        };

        self.write(FileNameEnum::Users, &entry, false).await;
        let user_data = UserInfo {
            id: hashed_id,
            name: heapless::String::from_str("NULL").unwrap(),
        };
        let mut users = USERS.get().await.lock().await;
        let _ = users.push(user_data);
        println!("Added user");
    }

    /// Removes the specified user
    pub async fn remove_user(&mut self, id: u64) {
        let hashed_id = net::hash_id(id).await;
        let id_str = hashed_id.to_string();
        let content = self.read(FileNameEnum::Users, None).await;
        let mut new_content = String::with_capacity(content.len());

        for line in content.lines() {
            if let Some((line_id, _)) = line.split_once(':')
                && line_id != id_str {
                new_content.push_str(line);
                new_content.push('\n');
            }
        }

        // Overwrite the file with the filtered content
        self.write(FileNameEnum::Users, &new_content, true).await;
        let mut users = USERS.get().await.lock().await;
        users.retain(|user| user.id != hashed_id);
    }

    /// Modifies the name of an existing user
    pub async fn change_name(&mut self, id: u64, new_name: String) {
        let hashed_id = net::hash_id(id).await;
        let id_str = hashed_id.to_string();
        let content = self.read(FileNameEnum::Users, None).await;
        let mut new_content = String::with_capacity(content.len());
        let mut found = false;

        for line in content.lines() {
            if let Some((line_id, _)) = line.split_once(':') {
                if line_id == id_str {
                    new_content.push_str(&format!("{}:{}\n", id, new_name));
                    found = true;
                } else {
                    new_content.push_str(line);
                    new_content.push('\n');
                }
            }
        }

        if found {
            self.write(FileNameEnum::Users, &new_content, true).await;
        }
    }

    /// Lists all users inside the database
    pub async fn list_users(&mut self) -> heapless::vec::Vec<UserInfo, 32> {
        let mut users = heapless::vec::Vec::new();

        let content = self.read(FileNameEnum::Users, None).await;

        for line in content.lines() {
            // Lines are formatted as "id:name"
            if let Some((id_str, name)) = line.split_once(':')
                && let Ok(id) = id_str.parse::<u64>() {
                let user = UserInfo {
                    id,
                    name: heapless::String::from_str(name).unwrap(),
                };
                let _ = users.push(user);
            }
        }

        users
    }

    /// Removes all users from the database
    pub async fn remove_all_users(&mut self) {
        self.remove(FileNameEnum::Users);
        let mut users = USERS.get().await.lock().await;
        users.clear();
    }

    // Password

    /// Writes a new password
    pub async fn set_password(&mut self, password: String) {
        let hash = hash_string(&password).await;
        let parsed_password = hash.to_string();
        self.write(FileNameEnum::Password, &parsed_password, true).await;
    }

    /// Gets the hashed password
    pub async fn get_password(&mut self) -> String {
        self.read(FileNameEnum::Password, None).await
    }
}