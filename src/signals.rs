use heapless::String;

/// Commands are used to send data and tasks from the networking thread to the IO thread
#[derive(Clone)]
pub enum Command {
    /// Delete all logged messages
    ClearLog,
    /// Puts the device into "add mode"
    AddUserMode,
    /// Adds a user to the database with a default name
    AddUser { id: u64 },
    /// Removes a user with the specified id from the database
    RemoveUser { id: u64 },
    /// Updates the user data on the SD card
    UpdateUser { id: u64, name: String<35> },
    /// Removes all users from the database
    RemoveAllUsers,
    /// If the user with the specified id is authorized
    IsUser { id: u64 },
    /// Sets a new unhashed password, overriding the old one.
    /// It will be hashed automatically
    SetPassword { password: alloc::string::String},
    /// Logs when the user opens the door
    LogUser { id: u64 },
}
