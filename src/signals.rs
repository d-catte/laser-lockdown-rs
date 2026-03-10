use heapless::String;

/// Commands are used to send data and tasks from the networking thread to the IO thread
#[derive(Clone)]
pub enum Command {
    /// Delete all logged messages
    ClearLog,
    /// Puts the device into "add mode"
    AddUserMode,
    /// Adds a user to the database with a default name
    AddUser { id: u32 },
    /// Removes a user with the specified id from the database
    RemoveUser { id: u32 },
    /// Updates the user data on the SD card
    UpdateUser { id: u32, name: String<32> },
    /// Removes all users from the database
    RemoveAllUsers,
    /// If the user with the specified id is authorized
    IsUser { id: u32 },
    /// Sets a new hashed password and its salt, overriding the old one
    SetPassword { hash: [u8; 32], salt: [u8; 16] },
    /// Logs when the user opens the door
    LogUser { id: u32 },
}
