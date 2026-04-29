#![no_std]
#![no_main]
#![recursion_limit = "256"]

use alloc::boxed::Box;
use core::cell::RefCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_hal_bus::spi::RefCellDevice;
use embedded_sdmmc::{SdCard, VolumeManager};
use esp_backtrace as _;
use esp_hal::peripherals::{GPIO4, GPIO5, GPIO6, GPIO7, GPIO10, GPIO11, GPIO12, GPIO13, GPIO17, GPIO18, SPI2, UART1, GPIO8};
use esp_hal::rng::{Rng, Trng, TrngSource};
use esp_hal::sha::Sha;
use esp_hal::time::Rate;
use esp_hal::{
    Blocking,
    gpio::{Input, InputConfig, Level, Output, OutputConfig},
    interrupt::software::SoftwareInterruptControl,
    system::Stack,
    timer::timg::TimerGroup,
    uart::{Config, Uart},
};
use esp_hal::{
    delay::Delay as EspHalDelay,
    spi::Mode as SpiMode,
    spi::master::{Config as SpiMasterConfig, Spi as SpiMaster},
};
use esp_println::println;
use esp_rtos::embassy::Executor;
use heapless::String;
use laser_lockdown_rs::rfid::SeeedRfid;
use laser_lockdown_rs::sd_utils::DummyTimeSource;
use laser_lockdown_rs::signals::Command;
use laser_lockdown_rs::{net, ntp, sd, sd_utils};
use sd::SD;
use sd::SPI_BUS;
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

const CARD_READER_DELAY: Duration = Duration::from_secs(5);

static ADD_MODE: AtomicBool = AtomicBool::new(false);
static ADD_MODE_ENABLED: Mutex<CriticalSectionRawMutex, Instant> = Mutex::new(Instant::MIN);
const MAX_ADD_MODE_TIME: Duration = Duration::from_secs(20);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern crate alloc;

/// Handles the GPIO and other IO
#[embassy_executor::task]
async fn io(
    gpio4: GPIO4<'static>,
    gpio5: GPIO5<'static>,
    gpio6: GPIO6<'static>,
    gpio7: GPIO7<'static>,
    gpio8: GPIO8<'static>,
    uart1: UART1<'static>,
    gpio17: GPIO17<'static>,
    gpio18: GPIO18<'static>,
    cmd: &'static Signal<CriticalSectionRawMutex, Command>,
    user_check: &'static Signal<CriticalSectionRawMutex, bool>,
) {
    // Set up GPIO and IO
    let mut indicator_led = Output::new(gpio4, Level::Low, OutputConfig::default());
    let mut buzzer = Output::new(gpio5, Level::Low, OutputConfig::default());
    let door_sensor = Input::new(gpio6, InputConfig::default());
    let mut door_open = Output::new(gpio7, Level::Low, OutputConfig::default());
    let mut door_close = Output::new(gpio8, Level::Low, OutputConfig::default());
    let uart1 = Uart::new(uart1, Config::default())
        .unwrap()
        .with_rx(gpio18)
        .with_tx(gpio17);
    let mut keycard_reader = SeeedRfid::new(uart1, CARD_READER_DELAY);
    loop {
        if let Some(card_id) = keycard_reader.read().await {
            // Check if add mode is enabled
            if ADD_MODE.load(Ordering::Relaxed) {
                ADD_MODE.store(false, Ordering::Relaxed);

                // Check if the add mode hasn't expired
                if Instant::now().duration_since(*ADD_MODE_ENABLED.lock().await) < MAX_ADD_MODE_TIME
                {
                    cmd.signal(Command::AddUser { id: card_id });

                    for _ in 0..3 {
                        indicator_led.set_high();
                        buzzer.set_high();
                        Timer::after(Duration::from_millis(500)).await;

                        indicator_led.set_low();
                        buzzer.set_low();
                        Timer::after(Duration::from_millis(500)).await;
                    }
                    continue;
                }
            }

            // Door closed
            if door_sensor.is_high() {
                let hashed_id = net::hash_id(card_id).await;
                cmd.signal(Command::IsUser { id: hashed_id });
                let user_exists = user_check.wait().await;
                if user_exists {
                    cmd.signal(Command::LogUser { id: hashed_id });

                    door_open.set_high();
                    // TODO Determine how long it takes to open/close the door
                    Timer::after(Duration::from_millis(2000)).await;
                    door_open.set_low();

                    // Flash light/buzzer every 0.25s
                    let mut ticker = Ticker::every(Duration::from_millis(250));
                    let mut ticks = 0;
                    loop {
                        if door_sensor.is_high() {
                            indicator_led.set_high();
                            buzzer.set_high();
                            ticker.next().await;
                            indicator_led.set_low();
                            buzzer.set_low();
                            ticker.next().await;
                        }
                        ticks += 1;
                        // Relock door after 10s
                        if ticks == 20 {
                            // Stop ticker
                            let mut ticker = Ticker::every(Duration::from_millis(250));
                            // Wait for door to close
                            while door_sensor.is_low() {
                                ticker.next().await;
                            }
                            // Close door
                            door_close.set_high();
                            // TODO Determine how long it takes to open/close the door
                            Timer::after(Duration::from_millis(2000)).await;
                            door_close.set_low();
                            break;
                        }
                    }
                } else {
                    let timer = Timer::after(Duration::from_secs(1));
                    indicator_led.set_high();
                    buzzer.set_high();
                    timer.await;
                    indicator_led.set_low();
                    buzzer.set_low();
                }
            }
        }
    }
}

/// Handles SD card operations
#[embassy_executor::task]
async fn sd(
    gpio10: GPIO10<'static>,
    mosi: GPIO11<'static>,
    sclk: GPIO12<'static>,
    miso: GPIO13<'static>,
    spi2: SPI2<'static>,
    cmd: &'static Signal<CriticalSectionRawMutex, Command>,
    user_check: &'static Signal<CriticalSectionRawMutex, bool>,
    time_request: &'static Signal<CriticalSectionRawMutex, ()>,
    time_response: &'static Signal<CriticalSectionRawMutex, Option<String<15>>>,
) {
    let cs = Output::new(gpio10, Level::High, OutputConfig::default());
    let spi_bus_config = SpiMasterConfig::default()
        .with_frequency(Rate::from_khz(400))
        .with_mode(SpiMode::_0);
    let spi_bus = SpiMaster::new(spi2, spi_bus_config)
        .unwrap()
        .with_miso(miso)
        .with_mosi(mosi)
        .with_sck(sclk);
    let spi_bus_ref: &'static RefCell<SpiMaster<Blocking>> = SPI_BUS.init(RefCell::new(spi_bus));
    let spi_device = RefCellDevice::new(spi_bus_ref, cs, EspHalDelay::new()).unwrap();
    let sdcard = SdCard::new(spi_device, EspHalDelay::new());
    let volume_mgr = VolumeManager::new(sdcard, DummyTimeSource);
    let vol_mgr: &'static RefCell<VolumeManager<sd::SdDevice, DummyTimeSource>> =
        sd::VOLUME_MGR.init(RefCell::new(volume_mgr));
    let sd = SD::new(vol_mgr, spi_bus_ref);

    // Set user cache
    if let Ok(users) = sd.list_users() {
        let _ = net::USERS.init(Mutex::new(users));
    }

    // Set log cache
    if let Ok(log) = sd.get_log() {
        let _ = net::LOGS.init(Mutex::new(log));
    }

    // Set password cache
    if let Ok(hash) = sd.get_password().await {
        let _ = net::PSWD.init(Mutex::new(hash));
    }

    let mut logging_buffer: String<64> = String::new();
    let mut msg_buffer: String<49> = String::new();

    loop {
        let cmd = cmd.wait().await;
        match cmd {
            Command::ClearLog => {
                let result = sd.clear_log();
                if result.is_err() {
                    time_request.signal(());
                    let response = time_response.wait().await;
                    if let Some(date) = response {
                        sd.append(date, "Failed to clear log.", &mut logging_buffer);
                    }
                }
            }
            Command::AddUserMode => {
                ADD_MODE.store(true, Ordering::Relaxed);
                *ADD_MODE_ENABLED.lock().await = Instant::now();
            }
            Command::AddUser { id } => {
                let result = sd.add_user(id).await;
                if result.is_err() {
                    time_request.signal(());
                    let response = time_response.wait().await;
                    if let Some(date) = response {
                        msg_buffer.clear();
                        msg_buffer.push_str("Failed to add ").unwrap();
                        write!(msg_buffer, "{}", id).unwrap();
                        sd.append(date, msg_buffer.as_str(), &mut logging_buffer);
                    }
                }
            }
            Command::RemoveUser { id } => {
                let result = sd.remove_user(id);
                if result.is_err() {
                    time_request.signal(());
                    let response = time_response.wait().await;
                    if let Some(date) = response {
                        msg_buffer.clear();
                        msg_buffer.push_str("Failed to remove ").unwrap();
                        write!(msg_buffer, "{:?}", id).unwrap();
                        sd.append(date, msg_buffer.as_str(), &mut logging_buffer);
                    }
                }
            }
            Command::UpdateUser { id, name } => {
                let result = sd.edit_user_name(id, &name);
                if result.is_err() {
                    time_request.signal(());
                    let response = time_response.wait().await;
                    if let Some(date) = response {
                        msg_buffer.clear();
                        msg_buffer.push_str("Failed to edit ").unwrap();
                        write!(msg_buffer, "{:?}", id).unwrap();
                        sd.append(date, msg_buffer.as_str(), &mut logging_buffer);
                    }
                }
            }
            Command::RemoveAllUsers => {
                let result = sd.remove_all_users();
                if result.is_err() {
                    time_request.signal(());
                    let response = time_response.wait().await;
                    if let Some(date) = response {
                        sd.append(date, "Failed to remove users.", &mut logging_buffer);
                    }
                }
            }
            Command::IsUser { id } => {
                user_check.signal(net::valid_user(id).await);
            }
            Command::SetPassword { hash } => {
                let result = sd.set_password(hash);
                if result.is_err() {
                    time_request.signal(());
                    let response = time_response.wait().await;
                    if let Some(date) = response {
                        sd.append(date, "Failed to set password.", &mut logging_buffer);
                    }
                }
            }
            Command::LogUser { id } => {
                time_request.signal(());
                let response = time_response.wait().await;
                if let Some(date) = response {
                    msg_buffer.clear();
                    msg_buffer.push_str("Accessed: ").unwrap();
                    write!(msg_buffer, "{:?}", id).unwrap();
                    sd.append(date, msg_buffer.as_str(), &mut logging_buffer);
                }
            }
        }
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    // Setup
    esp_println::logger::init_logger(log::LevelFilter::Info);
    println!("Initialized logger");
    println!("Initializing peripherals");
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Init PSRAM as heap
    println!("Initializing PSRAM");
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);

    // Allocate memory for networking packets
    println!("Allocating heap for networking");
    esp_alloc::heap_allocator!(size: 98767);

    // Init HTML
    let data = Box::leak(alloc::string::String::from(include_str!("index.html")).into_boxed_str());
    let html_ref = net::HTML_DATA.init(data);

    // Intercore communication
    static COMMANDS: StaticCell<Signal<CriticalSectionRawMutex, Command>> = StaticCell::new();
    let commands = &*COMMANDS.init(Signal::new());
    static USER_CHECK: StaticCell<Signal<CriticalSectionRawMutex, bool>> = StaticCell::new();
    let user_check = &*USER_CHECK.init(Signal::new());

    static TIME_REQUEST: StaticCell<Signal<CriticalSectionRawMutex, ()>> = StaticCell::new();
    let time_request = &*TIME_REQUEST.init(Signal::new());
    static TIME_RESPONSE: StaticCell<Signal<CriticalSectionRawMutex, Option<String<15>>>> =
        StaticCell::new();
    let time_response = &*TIME_RESPONSE.init(Signal::new());

    let _ = sd_utils::SHA_INSTANCE.init(Mutex::new(Sha::new(peripherals.SHA)));

    // Set caches
    let _ = net::_RNG_SOURCE.init(TrngSource::new(peripherals.RNG, peripherals.ADC1));
    let _ = net::CMD.init(commands);
    let _ = net::RAND.init(Mutex::new(Trng::try_new().unwrap()));

    static APP_CORE_STACK: StaticCell<Stack<8192>> = StaticCell::new();
    let app_core_stack = APP_CORE_STACK.init(Stack::new());
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    // Init WiFi Stack
    println!("Initializing WiFi");
    let radio_init = &*laser_lockdown_rs::mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller")
    );
    let rng = Rng::new();
    static STACK_CELL: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();
    let stack: &'static mut embassy_net::Stack<'static> = STACK_CELL.init(
        laser_lockdown_rs::wifi::start_wifi(radio_init, peripherals.WIFI, rng, &spawner).await,
    );

    // Init IO
    println!("Initializing second core");
    esp_rtos::start_second_core(
        peripherals.CPU_CTRL,
        sw_int.software_interrupt0,
        sw_int.software_interrupt1,
        app_core_stack,
        move || {
            static EXECUTOR: StaticCell<Executor> = StaticCell::new();
            let executor = EXECUTOR.init(Executor::new());
            executor.run(|spawner| {
                spawner
                    .spawn(io(
                        peripherals.GPIO4,
                        peripherals.GPIO5,
                        peripherals.GPIO6,
                        peripherals.GPIO7,
                        peripherals.GPIO8,
                        peripherals.UART1,
                        peripherals.GPIO17,
                        peripherals.GPIO18,
                        commands,
                        user_check,
                    ))
                    .ok();
                spawner
                    .spawn(sd(
                        peripherals.GPIO10,
                        peripherals.GPIO11,
                        peripherals.GPIO12,
                        peripherals.GPIO13,
                        peripherals.SPI2,
                        commands,
                        user_check,
                        time_request,
                        time_response,
                    ))
                    .ok();
            });
        },
    );

    // Manage clock
    println!("Initializing clock");
    spawner
        .spawn(ntp::start_clock(time_request, time_response, stack))
        .ok();

    // Init web app on main thread
    println!("Initializing web server");
    net::start_web_server(*stack, html_ref).await;
}
