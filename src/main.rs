#![no_std]
#![no_main]
#![recursion_limit = "256"]
#![allow(static_mut_refs)]
#![feature(type_alias_impl_trait)]

use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use core::cell::RefCell;
use core::str::FromStr;
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{with_timeout, Duration, Instant, Ticker, Timer};
use embedded_hal_bus::spi::RefCellDevice;
use embedded_sdmmc::{SdCard, VolumeIdx, VolumeManager};
use esp_backtrace as _;
use esp_hal::peripherals::{GPIO10, GPIO11, GPIO12, GPIO13, SPI2};
use esp_hal::rng::{Rng, Trng, TrngSource};
use esp_hal::sha::Sha;
use esp_hal::time::Rate;
use esp_hal::{
    gpio::{Input, InputConfig, Level, Output, OutputConfig},
    interrupt::software::SoftwareInterruptControl,
    system::Stack,
    timer::timg::TimerGroup,
};
use esp_hal::{
    spi::Mode as SpiMode,
    spi::master::{Config as SpiMasterConfig, Spi as SpiMaster},
};
use esp_hal::delay::Delay;
use esp_hal::gpio::{AnyPin, Pin};
use esp_println::println;
use esp_rtos::embassy::Executor;
use heapless::String;
use laser_lockdown_rs::signals::Command;
use laser_lockdown_rs::{net, ntp, sd_utils};
use static_cell::StaticCell;
use laser_lockdown_rs::rfid::HIDReader;
use laser_lockdown_rs::sd::{SdStorage};
use laser_lockdown_rs::sd_utils::{retry_with_backoff, DummyTimeSource};

esp_bootloader_esp_idf::esp_app_desc!();


static ADD_MODE: AtomicBool = AtomicBool::new(false);
static ADD_MODE_ENABLED: Mutex<CriticalSectionRawMutex, Instant> = Mutex::new(Instant::MIN);
const MAX_ADD_MODE_TIME: Duration = Duration::from_secs(20);
const WIFI_TIMEOUT: Duration = Duration::from_secs(30);

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("\n--- PANIC ---");

    if let Some(location) = info.location() {
        println!(
            "Location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    }
    println!("{}", info.message());

    loop {
        core::hint::spin_loop();
    }
}

extern crate alloc;

/// Handles the GPIO and other IO
#[embassy_executor::task]
async fn io(
    gpio4: AnyPin<'static>,
    gpio5: AnyPin<'static>,
    gpio6: AnyPin<'static>,
    gpio7: AnyPin<'static>,
    gpio8: AnyPin<'static>,
    gpio17: AnyPin<'static>,
    gpio18: AnyPin<'static>,
    cmd: &'static Signal<CriticalSectionRawMutex, Command>,
    user_check: &'static Signal<CriticalSectionRawMutex, bool>,
) {
    // Set up GPIO and IO
    println!("Initializing IO");

    // Indicator LED
    println!("Testing LED");
    let mut indicator_led = Output::new(gpio4, Level::Low, OutputConfig::default());
    indicator_led.set_high();
    Timer::after(Duration::from_secs(5)).await;
    indicator_led.set_low();
    println!("Initialized LED");

    // Buzzer
    println!("Testing Buzzer");
    let mut buzzer = Output::new(gpio5, Level::Low, OutputConfig::default());
    buzzer.set_high();
    Timer::after(Duration::from_secs(5)).await;
    buzzer.set_low();
    println!("Initialized Buzzer");

    // Door switch
    let door_sensor = Input::new(gpio6, InputConfig::default());
    println!("Initialized Door Sensor");

    // Actuator
    println!("Testing Actuator");
    let mut door_open = Output::new(gpio7, Level::Low, OutputConfig::default());
    let mut door_close = Output::new(gpio8, Level::Low, OutputConfig::default());
    println!("Initialized Actuator");

    // Card Reader
    println!("Initializing Card Reader");
    let d0 = Input::new(gpio17, InputConfig::default());
    let d1 = Input::new(gpio18, InputConfig::default());

    let mut reader = HIDReader::new(d0, d1);
    println!("Initialized Card Reader");
    loop {
        println!("Waiting for card");
        let code = reader.read_card().await;
        let card_id = code & 0xFFFF;
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
    // Setup SD
    // Start with low frequency for initialization
    let cs = Output::new(gpio10, Level::High, OutputConfig::default());

    // Start with low frequency for initialization
    let spi_bus_config = SpiMasterConfig::default()
        .with_frequency(Rate::from_khz(400)) // 400kHz for initialization
        .with_mode(SpiMode::_0);

    let spi_bus = SpiMaster::new(spi2, spi_bus_config)
        .expect("Failed to initialize SPI bus")
        .with_miso(miso)
        .with_mosi(mosi)
        .with_sck(sclk);

    let shared_spi_bus = RefCell::new(spi_bus);
    let spi_device = RefCellDevice::new(&shared_spi_bus, cs, Delay::new())
        .expect("Failed to create SPI device");

    println!("    SPI bus configured");

    // Initialize SD card with retry logic
    let sdcard = SdCard::new(spi_device, Delay::new());
    println!("Initializing SD Card...");
    let sd_size =
        retry_with_backoff("SD Card initialization", || async { sdcard.num_bytes() }).await;
    if let Some(num_bytes) = sd_size {
        println!(
            "    SD Card ready - size: {} GB",
            num_bytes / 1024 / 1024 / 1024
        );
    } else {
        println!("    SD Card initialization failed");
    }

    // Open volume 0 (main partition)
    let volume_mgr = VolumeManager::new(sdcard, DummyTimeSource);
    let volume0 = if sd_size.is_some() {
        retry_with_backoff("Opening volume 0", || async {
            volume_mgr.open_volume(VolumeIdx(0))
        })
            .await
    } else {
        None
    };
    if volume0.is_some() {
        println!("    Volume 0 opened");
    }

    // Open root directory
    let root_dir = if let Some(ref volume) = volume0 {
        retry_with_backoff("Opening root directory", || async {
            volume.open_root_dir()
        })
            .await
    } else {
        None
    };
    if root_dir.is_some() {
        println!("    Root directory opened");
    }

    // After initializing the SD card, increase the SPI frequency
    shared_spi_bus
        .borrow_mut()
        .apply_config(
            &SpiMasterConfig::default()
                .with_frequency(Rate::from_mhz(2))
                .with_mode(SpiMode::_0),
        )
        .expect("Failed to apply the second SPI configuration");

    let mut sd_device = SdStorage::new(
        &volume_mgr,
        &shared_spi_bus
    );

    // Set user cache
    println!("Generating User Cache");
    let users = sd_device.list_users();
    let _ = net::USERS.init(Mutex::new(users));

    // Set log cache
    println!("Generating Log Cache");
    let logs = sd_device.read_logs();
    let confined: String<1024> = String::from_str(&logs).unwrap();
    let _ = net::LOGS.init(Mutex::new(confined));

    // Set password cache
    println!("Generating Password Cache");
    let _ = net::PSWD.init(Mutex::new(sd_device.get_password()));

    loop {
        let cmd = cmd.wait().await;
        println!("Received SD Command");
        match cmd {
            Command::ClearLog => {
                sd_device.clear_logs();
            }
            Command::AddUserMode => {
                println!("Add User Mode");
                ADD_MODE.store(true, Ordering::Relaxed);
                *ADD_MODE_ENABLED.lock().await = Instant::now();
            }
            Command::AddUser { id } => {
                println!("Adding user {:?}", id);
                sd_device.add_user(id, None).await;
            }
            Command::RemoveUser { id } => {
                println!("Removing user {:?}", id);
                sd_device.remove_user(id).await;
            }
            Command::UpdateUser { id, name } => {
                println!("Updating user {:?} as {}", id, name);
                sd_device.change_name(id, name.to_string()).await;
            }
            Command::RemoveAllUsers => {
                println!("Remove All Users");
               sd_device.remove_all_users();
            }
            Command::IsUser { id } => {
                println!("Is User {:?}", id);
                user_check.signal(net::valid_user(id).await);
            }
            Command::SetPassword { password } => {
                println!("Setting Password");
               sd_device.set_password(password).await;
            }
            Command::LogUser { id } => {
                let timestamp = sd_device.get_timestamp(
                    time_request,
                    time_response,
                ).await;
                let msg = format!("Accessed: {}", id);
                sd_device.log_message(msg, Some(timestamp));
            }
        }
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    // Setup
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Init PSRAM as heap
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);

    // TODO Possibly revert this to safe code
    unsafe {
        #[unsafe(link_section = ".data")]
        static mut WIFI_HEAP: [u8; 80_000] = [0u8; 80_000];

        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            WIFI_HEAP.as_mut_ptr(),
            WIFI_HEAP.len(),
            esp_alloc::MemoryCapability::Internal.into(),
        ));
    }


    esp_println::logger::init_logger(log::LevelFilter::Info);
    println!("Initialized logger");

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
    println!("Initializing RNG");
    let rng = Rng::new();
    static STACK_CELL: StaticCell<Option<embassy_net::Stack<'static>>> = StaticCell::new();

    println!("Starting WiFi");
    let wifi_result = with_timeout(
        WIFI_TIMEOUT,
        laser_lockdown_rs::wifi::start_wifi(radio_init, peripherals.WIFI, rng, &spawner)
    ).await;

    let stack: &'static mut Option<embassy_net::Stack<'static>> = match wifi_result {
        Ok(s) => {
            println!("WiFi connected successfully!");
            STACK_CELL.init(Some(s))
        }
        Err(_) => {
            println!("WiFi connection timed out! Proceeding in offline mode...");
            STACK_CELL.init(None)
        }
    };
    // Init IO
    println!("Initializing second core");
    let io_pins = (
        peripherals.GPIO4,
        peripherals.GPIO5,
        peripherals.GPIO6,
        peripherals.GPIO7,
        peripherals.GPIO8,
        peripherals.GPIO17,
        peripherals.GPIO18,
    );

    let sd_pins = (
        peripherals.GPIO10,
        peripherals.GPIO11,
        peripherals.GPIO12,
        peripherals.GPIO13,
        peripherals.SPI2,
    );

    let cpu_ctrl = peripherals.CPU_CTRL;

    esp_rtos::start_second_core(
        cpu_ctrl,
        sw_int.software_interrupt0,
        sw_int.software_interrupt1,
        app_core_stack,
        move || {
            static EXECUTOR: StaticCell<Executor> = StaticCell::new();
            let executor = EXECUTOR.init(Executor::new());

            executor.run(|spawner| {
                spawner.spawn(io(
                    io_pins.0.degrade(),
                    io_pins.1.degrade(),
                    io_pins.2.degrade(),
                    io_pins.3.degrade(),
                    io_pins.4.degrade(),
                    io_pins.5.degrade(),
                    io_pins.6.degrade(),
                    commands,
                    user_check,
                )).ok();
                spawner.spawn(sd(
                    sd_pins.0,
                    sd_pins.1,
                    sd_pins.2,
                    sd_pins.3,
                    sd_pins.4,
                    commands,
                    user_check,
                    time_request,
                    time_response,
                )).ok();
            });
        },
    );

    // Manage clock
    if let Some(stack) = stack {
        println!("Initializing clock");
        spawner
            .spawn(ntp::start_clock(time_request, time_response, stack))
            .ok();

        // Init web app on main thread
        println!("Initializing web server");
        net::start_web_server(*stack, html_ref).await;
    }
}
