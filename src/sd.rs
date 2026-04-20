use crate::net;
use crate::net::{UserInfo, SALT};
use crate::sd_utils::DummyTimeSource;
use core::cell::RefCell;
use core::fmt::Write;
use embedded_hal_bus::spi::RefCellDevice;
use embedded_sdmmc::{Directory, Mode as FileMode, SdCard, VolumeIdx, VolumeManager};
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::time::{Instant, Rate};
use esp_hal::{
    Blocking,
    spi::Mode as SpiMode,
    spi::master::{Config as SpiMasterConfig, Spi as SpiMaster},
};
use heapless::{String, Vec};
use static_cell::StaticCell;

/// The implementation of the SD card IO
pub type SdDevice =
    SdCard<RefCellDevice<'static, SpiMaster<'static, Blocking>, Output<'static>, Delay>, Delay>;

/// Interface for SD card's SPI
pub static SPI_BUS: StaticCell<RefCell<SpiMaster<Blocking>>> = StaticCell::new();

/// File manager for the SD card
pub static VOLUME_MGR: StaticCell<RefCell<VolumeManager<SdDevice, DummyTimeSource>>> =
    StaticCell::new();

/// The authorized users database
const USERS_FILE: &str = "users.dat";

/// The temporary user database created when modifying the existing database
const TEMP_FILE: &str = "users.tmp";

/// The file where all logs are stored
const LOG_FILE: &str = "log.txt";

/// The file where the hashed passwords are stored
const PASSWORD_FILE: &str = "pswd.txt";

/// The maximum size a name can be in UTF-8 characters
const MAX_NAME_LEN: usize = 32;

/// The max size, in characters, that are sent from the log file to the client
const LOG_SNAPSHOT_SIZE: usize = 4096;

/// The max size the physical log file can be before being trimmed
const LOG_MAX_SIZE: usize = LOG_SNAPSHOT_SIZE * 2;

/// The default password when no password is set
const DEFAULT_PASSWORD: &str = "admin";

/// The name of any newly added user
const DEFAULT_NEW_USER: &str = "New User";

/// This is the interface for the SD card. The SD card holds all the information for the application
/// that must persist over power cycles. This includes the following information:
/// - The hashed admin password for the website
/// - 32 user entries (8 byte id, 32 character name)
/// - A log file up to 4096 characters
pub struct SD {
    spi_bus: &'static RefCell<SpiMaster<'static, Blocking>>,
    volume_mgr: &'static RefCell<VolumeManager<SdDevice, DummyTimeSource>>,
}

impl SD {
    /// Create a new SD card instance.
    pub fn new(
        volume_mgr: &'static RefCell<VolumeManager<SdDevice, DummyTimeSource>>,
        spi_bus_ref: &'static RefCell<SpiMaster<Blocking>>,
    ) -> Self {
        SD {
            spi_bus: spi_bus_ref,
            volume_mgr,
        }
    }

    /// Initialize the SD card and set the SPI frequency to 2 MHz.
    pub fn init(&mut self) {
        // After initializing the SD card, increase the SPI frequency
        self.spi_bus
            .borrow_mut()
            .apply_config(
                &SpiMasterConfig::default()
                    .with_frequency(Rate::from_mhz(2))
                    .with_mode(SpiMode::_0),
            )
            .expect("Failed to apply the second SPI configuration");
    }

    /// Example:
    /// ```
    /// sd.with_root_dir(|root| {
    ///     let mut file = root
    ///         .open_file_in_dir("log.txt", FileMode::ReadWriteCreateOrAppend)
    ///         .unwrap();
    ///
    ///     file.write(b"Hello\n").unwrap();
    ///     file.flush().unwrap();
    /// });
    /// ```
    pub fn with_root_dir<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Directory<SdDevice, DummyTimeSource, 4, 4, 1>) -> R,
    {
        let vol_mgr = self.volume_mgr.borrow();
        let volume = vol_mgr.open_volume(VolumeIdx(0)).unwrap();

        let mut root = volume.open_root_dir().unwrap();

        f(&mut root)
    }

    /// Appends to the end of the log.
    /// If the log hits >8192 bytes then the last 4096 bytes are deleted. Realistically this should
    /// happen fairly infrequently
    fn append_to_log(&self, message: &str) {
        let timestamp = Instant::now().duration_since_epoch().as_secs();

        self.with_root_dir(|root| {
            let mut file = root
                .open_file_in_dir(LOG_FILE, FileMode::ReadWriteCreateOrAppend)
                .unwrap();

            let size = file.length() as usize;

            if size >= LOG_MAX_SIZE {
                let start = size - LOG_SNAPSHOT_SIZE;
                file.seek_from_start(start as u32).unwrap();

                let mut buf = [0u8; LOG_SNAPSHOT_SIZE];
                let read = file.read(&mut buf).unwrap();

                let mut slice = &buf[..read];

                // Remove partial first line
                if let Some(pos) = slice.iter().position(|b| *b == b'\n') {
                    slice = &slice[pos + 1..];
                } else {
                    slice = &[];
                }

                root.delete_file_in_dir(LOG_FILE).ok();

                let new_file = root
                    .open_file_in_dir(LOG_FILE, FileMode::ReadWriteCreateOrAppend)
                    .unwrap();

                new_file.write(slice).unwrap();
                new_file.flush().unwrap();

                file = new_file;
            }

            // Append new entry
            let mut buffer = [0u8; 128];
            let mut writer = BufferWriter::new(&mut buffer);

            write!(writer, "{} ", timestamp).unwrap();
            writer.write_str(message).unwrap();
            writer.write_str("\n").unwrap();

            let len = writer.len();

            file.write(&buffer[..len]).unwrap();
            file.flush().unwrap();
        });
    }

    /// Appends to the log with a timestamp
    pub fn append(&self, time: String<15>, message: &str, buffer: &mut String<64>) {
        buffer.clear();
        writeln!(buffer, "{},{}", time, message).ok();
        self.append_to_log("Failed to clear log");
    }

    /// Clears the entire log file
    pub fn clear_log(&self) -> Result<(), ()> {
        self.with_root_dir(|root| {
            root.delete_file_in_dir(LOG_FILE).map_err(|_| ())?;
            Ok(())
        })
    }

    /// Gets the latest 4096 characters, disregarding incomplete lines if cut off by the character limit
    pub fn get_log(&self) -> Result<String<LOG_SNAPSHOT_SIZE>, ()> {
        self.with_root_dir(|root| {
            let file = match root.open_file_in_dir(LOG_FILE, FileMode::ReadOnly) {
                Ok(f) => f,
                Err(_) => return Ok(String::new()),
            };

            let file_size = file.length() as usize;

            let start = file_size.saturating_sub(LOG_SNAPSHOT_SIZE);

            if start > 0 {
                file.seek_from_start(start as u32).map_err(|_| ())?;
            }

            let mut buffer = [0u8; LOG_SNAPSHOT_SIZE];
            let read = file.read(&mut buffer).map_err(|_| ())?;

            if read == 0 {
                return Ok(String::new());
            }

            let mut slice = &buffer[..read];

            // If we started in the middle of the file, drop the first partial line
            if start > 0 {
                if let Some(pos) = slice.iter().position(|b| *b == b'\n') {
                    slice = &slice[pos + 1..];
                } else {
                    // No newline found → all data is partial
                    return Ok(String::new());
                }
            }

            let text = core::str::from_utf8(slice).map_err(|_| ())?;

            let mut log: String<LOG_SNAPSHOT_SIZE> = String::new();
            log.push_str(text).map_err(|_| ())?;

            Ok(log)
        })
    }

    /// Sets the hashed password and salt, then stores it in the file
    pub fn set_password(&self, hash: [u8; 32]) -> Result<(), ()> {
        self.with_root_dir(|root| {
            // Remove old password file if it exists
            root.delete_file_in_dir(PASSWORD_FILE).ok();

            let file = root
                .open_file_in_dir(PASSWORD_FILE, FileMode::ReadWriteCreateOrAppend)
                .map_err(|_| ())?;

            file.write(&hash).map_err(|_| ())?;
            file.flush().map_err(|_| ())?;

            Ok(())
        })
    }

    /// Gets the hashed password and salt from the file
    pub async fn get_password(&self) -> Result<[u8; 32], ()> {
        let result = self.with_root_dir(|root| {
            let file = root.open_file_in_dir(PASSWORD_FILE, FileMode::ReadOnly);

            let mut hash = [0u8; 32];

            match file {
                Ok(file) => {
                    let read_hash = file.read(&mut hash).map_err(|_| ())?;

                    if read_hash != 32 {
                        return Err(());
                    }

                    Ok(Some(hash))
                }
                Err(_) => Ok(None),
            }
        })?;

        if let Some(v) = result {
            return Ok(v);
        }

        // File didn't exist
        let hash = net::hash_password(DEFAULT_PASSWORD, &SALT).await;

        self.set_password(hash)?;

        Ok(hash)
    }

    /// Adds a new user to the database of authorized users
    pub fn add_user(&self, id: u32) -> Result<(), ()> {
        self.with_root_dir(|root| {
            let file = root
                .open_file_in_dir(USERS_FILE, FileMode::ReadWriteCreateOrAppend)
                .map_err(|_| ())?;

            let mut entry = [0u8; 4 + MAX_NAME_LEN];

            // ID
            entry[..4].copy_from_slice(&id.to_le_bytes());

            // Name
            let name_bytes = DEFAULT_NEW_USER.as_bytes();
            entry[4..4 + name_bytes.len()].copy_from_slice(name_bytes);

            file.write(&entry).map_err(|_| ())?;
            file.flush().map_err(|_| ())?;

            Ok(())
        })
    }

    /// Lists all users (up to 32) that are in the database
    pub fn list_users(&self) -> Result<Vec<UserInfo, 32>, ()> {
        self.with_root_dir(|root| {
            let mut file = root
                .open_file_in_dir(USERS_FILE, FileMode::ReadOnly)
                .map_err(|_| ())?;

            let mut users: Vec<UserInfo, 32> = Vec::new();

            while let Some(user) = Self::read_entry(&mut file)? {
                users.push(user).map_err(|_| ())?;
            }

            Ok(users)
        })
    }

    /// Replaces a user's name with the specified id.
    /// **NOTE**: This is an expensive operation, use infrequently
    pub fn edit_user_name(&self, id: u32, new_name: &String<32>) -> Result<(), ()> {
        self.with_root_dir(|root| {
            let old_file = root
                .open_file_in_dir(USERS_FILE, FileMode::ReadOnly)
                .map_err(|_| ())?;

            let new_file = root
                .open_file_in_dir(TEMP_FILE, FileMode::ReadWriteCreateOrAppend)
                .map_err(|_| ())?;

            let mut entry = [0u8; 4 + MAX_NAME_LEN];

            loop {
                let read = old_file.read(&mut entry).map_err(|_| ())?;

                if read == 0 {
                    break;
                }

                if read != entry.len() {
                    return Err(()); // corrupted entry
                }

                let entry_id = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);

                if entry_id == id {
                    // overwrite name
                    let mut name_buf = [0u8; MAX_NAME_LEN];
                    let name_bytes = new_name.as_bytes();
                    name_buf[..name_bytes.len()].copy_from_slice(name_bytes);

                    entry[4..].copy_from_slice(&name_buf);
                }

                new_file.write(&entry).map_err(|_| ())?;
            }

            new_file.flush().map_err(|_| ())?;

            // Replace old file
            root.delete_file_in_dir(USERS_FILE).map_err(|_| ())?;

            self.rename_file(TEMP_FILE, USERS_FILE)?;

            Ok(())
        })
    }

    /// Renames a file by copying all of its contents to a new file
    pub fn rename_file(&self, old_name: &str, new_name: &str) -> Result<(), ()> {
        self.with_root_dir(|root| {
            let old_file = root
                .open_file_in_dir(old_name, FileMode::ReadOnly)
                .map_err(|_| ())?;

            let new_file = root
                .open_file_in_dir(new_name, FileMode::ReadWriteCreateOrAppend)
                .map_err(|_| ())?;

            let mut buffer = [0u8; 256];

            loop {
                let read = old_file.read(&mut buffer).map_err(|_| ())?;
                if read == 0 {
                    break;
                }

                new_file.write(&buffer[..read]).map_err(|_| ())?;
            }

            new_file.flush().map_err(|_| ())?;

            root.delete_file_in_dir(old_name).map_err(|_| ())?;

            Ok(())
        })
    }

    /// Removes a user from the authorized users database
    pub fn remove_user(&self, id: u32) -> Result<(), ()> {
        self.with_root_dir(|root| {
            let mut original = match root.open_file_in_dir(USERS_FILE, FileMode::ReadOnly) {
                Ok(f) => f,
                Err(_) => return Ok(()),
            };

            let temp = root
                .open_file_in_dir(TEMP_FILE, FileMode::ReadWriteCreateOrTruncate)
                .map_err(|_| ())?;

            // Copy all except target ID
            while let Some(user) = Self::read_entry(&mut original)? {
                if user.id != id {
                    temp.write(&user.id.to_le_bytes()).map_err(|_| ())?;
                    temp.write(user.name.as_bytes()).map_err(|_| ())?;
                } else {
                    break;
                }
            }

            temp.flush().map_err(|_| ())?;

            drop(original);
            drop(temp);

            // Delete original
            root.delete_file_in_dir(USERS_FILE).map_err(|_| ())?;

            // Rename to original file
            self.rename_file(TEMP_FILE, USERS_FILE)?;

            Ok(())
        })
    }

    /// Clears the authorized users database
    pub fn remove_all_users(&self) -> Result<(), ()> {
        self.with_root_dir(|root| {
            root.delete_file_in_dir(USERS_FILE).map_err(|_| ())?;
            Ok(())
        })
    }

    /// Gets the next entry in the authorized users database
    fn read_entry(
        file: &mut embedded_sdmmc::File<SdDevice, DummyTimeSource, 4, 4, 1>,
    ) -> Result<Option<UserInfo>, ()> {
        let mut entry = [0u8; 4 + MAX_NAME_LEN];

        let read = file.read(&mut entry).map_err(|_| ())?;

        if read == 0 {
            return Ok(None);
        }

        if read < entry.len() {
            return Err(()); // corrupted file
        }

        // Extract ID
        let id = u32::from_le_bytes(entry[..4].try_into().unwrap());

        // Extract name bytes
        let name_bytes = &entry[4..4 + MAX_NAME_LEN];

        // Find real length (remove zero padding)
        let len = name_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(MAX_NAME_LEN);

        // Convert to String<32>
        let mut name: String<32> = String::new();
        name.push_str(core::str::from_utf8(&name_bytes[..len]).map_err(|_| ())?)
            .map_err(|_| ())?;

        Ok(Some(UserInfo { id, name }))
    }
}

struct BufferWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> BufferWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn len(&self) -> usize {
        self.pos
    }
}

impl<'a> Write for BufferWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = self.pos + bytes.len();

        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }

        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }
}
