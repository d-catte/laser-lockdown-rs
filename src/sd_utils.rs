use embassy_sync::mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::once_lock::OnceLock;
use embassy_time::{Duration, Timer};
use esp_hal::sha::Sha;

/// Maximum number of retries for SD card operations
pub const MAX_RETRIES: u8 = 4;

/// The SD card's SHA implementation for hashing passwords
pub static SHA_INSTANCE: OnceLock<Mutex<CriticalSectionRawMutex, Sha>> = OnceLock::new();

/// Retry operations with 500ms backoff, useful for SD card initialization
pub async fn retry_with_backoff<T, E, F, Fut>(operation_name: &str, mut operation: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: core::fmt::Debug,
{
    for attempt in 1..=MAX_RETRIES {
        match operation().await {
            Ok(result) => return Some(result),
            Err(e) => {
                esp_println::println!(
                    "{} failed: {:?} - Retry {}/{}",
                    operation_name,
                    e,
                    attempt,
                    MAX_RETRIES
                );
                if attempt >= MAX_RETRIES {
                    esp_println::println!(
                        "{} failed after {} retries",
                        operation_name,
                        MAX_RETRIES
                    );
                    return None;
                }
                Timer::after(Duration::from_millis(500)).await;
            }
        }
    }
    None
}

/// Dummy time source for embedded-sdmmc (use RTC for real timestamps)
pub struct DummyTimeSource;

impl embedded_sdmmc::TimeSource for DummyTimeSource {
    fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
        embedded_sdmmc::Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}
