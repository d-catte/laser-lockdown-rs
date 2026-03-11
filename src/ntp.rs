use core::fmt::Write;
use core::net::{IpAddr, SocketAddr};

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{Duration, Instant};
use heapless::String;
use sntpc::{NtpContext, NtpTimestampGenerator, get_time};
use sntpc_net_embassy::UdpSocketWrapper;

/// The main timeserver. It routes clients to their local timeserver
const NTP_SERVER: &str = "pool.ntp.org";
/// How often the time should be resynced
const RESYNC_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);
/// The UTC timezone
const UTC_OFFSET: i8 = -5;

/// This clock gives accurate timings based on the actual time.
/// It is synced on demand after 24 hours.
pub struct Clock<'a> {
    stack: &'a Stack<'a>,
    socket: UdpSocketWrapper<'a>,
    context: NtpContext<Timestamp>,

    base_unix: u64,
    synced_at: Instant,
}

#[derive(Copy, Clone, Default)]
struct Timestamp {
    base: u64,
}

impl NtpTimestampGenerator for Timestamp {
    fn init(&mut self) {}

    fn timestamp_sec(&self) -> u64 {
        self.base
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        0
    }
}

impl<'a> Clock<'a> {
    pub fn new(
        stack: &'a Stack<'a>,
        rx_meta: &'a mut [PacketMetadata],
        rx_buf: &'a mut [u8],
        tx_meta: &'a mut [PacketMetadata],
        tx_buf: &'a mut [u8],
    ) -> Self {
        let mut socket: UdpSocket = UdpSocket::new(*stack, rx_meta, rx_buf, tx_meta, tx_buf);

        socket.bind(0).unwrap();

        Self {
            stack,
            socket: UdpSocketWrapper::new(socket),
            context: NtpContext::new(Timestamp::default()),
            base_unix: 0,
            synced_at: Instant::now(),
        }
    }

    /// Sync with NTP server
    pub async fn sync(&mut self) -> Result<(), ()> {
        let addrs = self
            .stack
            .dns_query(NTP_SERVER, DnsQueryType::A)
            .await
            .map_err(|_| ())?;

        if addrs.is_empty() {
            return Err(());
        }

        let addr: IpAddr = addrs[0].into();
        let socket_addr = SocketAddr::from((addr, 123));

        let result = get_time(socket_addr, &self.socket, self.context)
            .await
            .map_err(|_| ())?;

        let unix = result.sec();

        self.base_unix = unix as u64;
        self.synced_at = Instant::now();

        Ok(())
    }

    /// Get current time
    /// Resync if older than 24h
    pub async fn now(&mut self) -> Result<String<15>, ()> {
        if Instant::now() - self.synced_at > RESYNC_INTERVAL {
            let _ = self.sync().await;
        }

        let unix = ((self.base_unix + (Instant::now() - self.synced_at).as_secs()) as i64
            + 3600 * UTC_OFFSET as i64) as u64;
        let (year, month, day, hour, min) = Self::unix_to_datetime(unix);
        let mut s: String<15> = String::new();
        write!(
            s,
            "{:02}/{:02}/{:02} {:02}:{:02} ",
            month, day, year, hour, min
        )
        .ok();
        Ok(s)
    }

    /// Converts unix time to date time: MM/DD/YY hh:mm
    fn unix_to_datetime(unix: u64) -> (u16, u8, u8, u8, u8) {
        const SECS_PER_DAY: u64 = 86400;

        let days = unix / SECS_PER_DAY;
        let mut secs = unix % SECS_PER_DAY;

        let hour = (secs / 3600) as u8;
        secs %= 3600;
        let minute = (secs / 60) as u8;

        // Date conversion (Unix epoch: 1970-01-01)
        let z = days as i64 + 719468;
        let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = mp + if mp < 10 { 3 } else { -9 };
        let year = y + if m <= 2 { 1 } else { 0 };

        (year as u16, m as u8, d as u8, hour, minute)
    }
}
