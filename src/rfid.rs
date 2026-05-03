use embassy_futures::select::{select, Either};
use embassy_time::{with_timeout, Duration};
use esp_hal::gpio::Input;
use esp_println::println;

pub struct HIDReader<'a> {
    d0: Input<'a>,
    d1: Input<'a>,
}

impl<'a> HIDReader<'a> {
    pub fn new(d0_pin: Input<'a>, d1_pin: Input<'a>) -> Self {
        Self {
            d0: d0_pin,
            d1: d1_pin,
        }
    }

    pub async fn read_card(&mut self) -> u64 {
        loop {
            let mut bits: u64 = 0;
            let mut count: u8 = 0;

            let first_bit = select(
                self.d0.wait_for_rising_edge(),
                self.d1.wait_for_rising_edge()
            ).await;
            println!("Bit detected");

            match first_bit {
                Either::First(_) => { bits <<= 1; count += 1; }
                Either::Second(_) => { bits = (bits << 1) | 1; count += 1; }
            }

            while count < 35 {
                let bit = with_timeout(
                    Duration::from_millis(50),
                    select(self.d0.wait_for_rising_edge(), self.d1.wait_for_rising_edge())
                ).await;

                match bit {
                    Ok(Either::First(_)) => {
                        bits <<= 1;
                        count += 1;
                    }
                    Ok(Either::Second(_)) => {
                        bits = (bits << 1) | 1;
                        count += 1;
                    }
                    Err(_) => break,
                }
            }

            if count == 35 {
                let card_id = (bits >> 1) & 0x1FFFFF;
                println!("Read ID: {}", card_id);
                return card_id;
            } else if count > 0 {
                println!("Incomplete reading card: {} bits collected", count);
            }
        }
    }
}