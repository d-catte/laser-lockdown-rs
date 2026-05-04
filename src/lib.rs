#![no_std]
#![feature(impl_trait_in_assoc_type)]
extern crate alloc;

pub mod net;
pub mod ntp;
pub mod sd_utils;
pub mod signals;
pub mod wifi;
pub mod rfid;
pub mod sd;
pub mod buzzer;

#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}
