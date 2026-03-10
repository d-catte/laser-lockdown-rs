use embassy_time::{Duration, Instant, Timer};
use embedded_io::Read;

#[derive(Clone, Copy, Default, Debug)]
pub struct RFIDData {
    pub raw: [u8; 5],
    pub valid: bool,
}

pub struct SeeedRfid<UART> {
    uart: UART,
    data: RFIDData,
    refresh_freq: Duration,
    last_scan: Instant,
}

impl<UART> SeeedRfid<UART>
where
    UART: Read,
{
    pub fn new(uart: UART, refresh_freq: Duration) -> Self {
        Self {
            uart,
            data: RFIDData::default(),
            refresh_freq,
            last_scan: Instant::MIN,
        }
    }

    /// Blocking read of one RFID frame
    pub async fn read(&mut self) -> Option<u32> {
        self.wait_for_scan_window().await;
        let mut buffer = [0u8; 5];

        if self.uart.read(buffer.as_mut()).is_err() {
            return None;
        }

        if Self::check_checksum(&buffer) {
            self.data.raw = buffer;
            self.data.valid = true;
            Some(Self::card_number(&buffer))
        } else {
            self.data.valid = false;
            None
        }
    }

    async fn wait_for_scan_window(&self) {
        let now = Instant::now();
        let next_allowed = self.last_scan + self.refresh_freq;

        if now < next_allowed {
            Timer::after(next_allowed - now).await;
        }
    }

    fn check_checksum(data: &[u8; 5]) -> bool {
        data[4] == (data[0] ^ data[1] ^ data[2] ^ data[3])
    }

    fn card_number(data: &[u8; 5]) -> u32 {
        ((data[0] as u32) << 24)
            | ((data[1] as u32) << 16)
            | ((data[2] as u32) << 8)
            | (data[3] as u32)
    }

    pub fn raw_data(&self) -> RFIDData {
        self.data
    }
}
