#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]

use chrono::{DateTime, Datelike, FixedOffset, TimeDelta, TimeZone, Timelike, Utc};
use core::cell::{Cell, RefCell};
use core::str::from_utf8;
use core::*;
use cyw43::JoinOptions;
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_sync::blocking_mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Ticker, Timer};
use heapless::Vec;
use rand::RngCore;
use reqwless::client::HttpClient;
use reqwless::request::Method;
use serde::Deserialize;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _, serde_json_core};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const WEATHER_URL: &str = env!("WEATHER_URL");

struct MyType {
    unixtime: u32,
}

// Use blocking Mutex with Cell/RefCell for sharing non-async things
static MUTEX_BLOCKING: blocking_mutex::Mutex<CriticalSectionRawMutex, RefCell<MyType>> =
    blocking_mutex::Mutex::new(RefCell::new(MyType { unixtime: 0 }));

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn print_time_task() -> ! {
    let mut ticker = Ticker::every(Duration::from_secs(1));
    loop {
        ticker.next().await;
        MUTEX_BLOCKING.lock(|x| {
            let mut x_borrow = x.borrow_mut();
            x_borrow.unixtime += 1000;
            defmt::info!("Time: {:?}", x_borrow.unixtime);
        });
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("Hello World!");
    defmt::unwrap!(spawner.spawn(print_time_task()));

    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    // let fw = include_bytes!("../../../../cyw43-firmware/43439A0.bin");
    // let clm = include_bytes!("../../../../cyw43-firmware/43439A0_clm.bin");
    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download 43439A0.bin --binary-format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs download 43439A0_clm.bin --binary-format bin --chip RP2040 --base-address 0x10140000
    let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 230321) };
    let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    defmt::unwrap!(spawner.spawn(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());

    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    defmt::unwrap!(spawner.spawn(net_task(runner)));

    loop {
        match control
            .join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
            .await
        {
            Ok(_) => break,
            Err(err) => {
                defmt::info!("join failed with status={}", err.status);
            }
        }
    }

    defmt::info!("waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    defmt::info!("DHCP is now up!");

    defmt::info!("waiting for link up...");
    while !stack.is_link_up() {
        Timer::after_millis(500).await;
    }
    defmt::info!("Link is up!");

    defmt::info!("waiting for stack to be up...");
    stack.wait_config_up().await;
    defmt::info!("Stack is up!");

    // And now we can use it!

    loop {
        let mut open_weather_rx_buffer = [0; 32768];

        let open_weather = get_open_weather(stack, &mut open_weather_rx_buffer)
            .await
            .unwrap();

        defmt::info!(
            "lat/lon: {:?}, {:?} - timezone: {:?}",
            open_weather.lat,
            open_weather.lon,
            open_weather.timezone
        );

        let mut epoch_rx_buffer = [0; 32768];

        let epoch: i64 = get_epoch(stack, &mut epoch_rx_buffer).await.unwrap();

        let now = time_in_local(open_weather.timezone_offset as i32, epoch);

        let ((h_time, h_temp), (l_time, l_temp)) = high_low_temp(&open_weather, &now);

        defmt::info!(
            "Now: {:?} at {:?}",
            open_weather.current.temp,
            FixedOffsetDateTime(now)
        );
        defmt::info!("High: {:?} at {:?}", h_temp, FixedOffsetDateTime(h_time));
        defmt::info!("Low: {:?} at {:?}", l_temp, FixedOffsetDateTime(l_time));

        Timer::after(Duration::from_secs(20)).await;
    }
}

async fn get_open_weather<'s, 'b: 'w, 'e, 'w>(
    stack: Stack<'s>,
    rx_buffer: &'b mut [u8; 32768],
) -> Result<OpenWeather<'w>, &'e str> {
    let open_weather_body = make_request(WEATHER_URL, stack, rx_buffer).await.unwrap();

    let bytes = open_weather_body.as_bytes();

    match serde_json_core::de::from_slice::<OpenWeather<'w>>(bytes) {
        Ok((output, _used)) => {
            let foo = output;
            Ok(foo)
        }

        Err(_e) => {
            defmt::error!("Failed to parse response body");
            return Err("Failed to parse response body"); // handle the error
        }
    }
}

async fn get_epoch<'s, 'b, 'e>(
    stack: Stack<'s>,
    rx_buffer: &'b mut [u8; 32768],
) -> Result<i64, &'e str> {
    let world_time_url_tmp = WEATHER_URL
        .bytes()
        .chain("/epoch".bytes())
        .collect::<Vec<u8, 200>>();

    let world_time_url = from_utf8(world_time_url_tmp.as_slice()).unwrap();
    let world_time_body = make_request(world_time_url, stack, rx_buffer)
        .await
        .unwrap();

    match serde_json_core::de::from_str::<i64>(world_time_body) {
        Ok((output, _used)) => {
            let foo = output;
            Ok(foo)
        }

        Err(_e) => {
            defmt::error!("Failed to parse response body");
            return Err("Failed to parse response body"); // handle the error
        }
    }
}
#[derive(Deserialize, Debug, Default, defmt::Format, Clone)]
#[serde(default)]
pub struct OpenWeather<'a> {
    pub lat: f32,
    pub lon: f32,
    pub timezone: &'a str,
    pub timezone_offset: f32,
    pub current: Current,
    pub hourly: Vec<Hourly, 48>,
}
#[derive(Deserialize, Debug, Default, defmt::Format, Clone, Copy)]
#[serde(default)]
pub struct Current {
    pub dt: i64,
    pub temp: f32,
    pub feels_like: f32,
}

#[derive(Deserialize, Debug, Default, defmt::Format, Clone, Copy)]
#[serde(default)]
pub struct Hourly {
    pub dt: i64,
    pub temp: f32,
}

#[derive(Deserialize, Debug, Default, Copy, Clone)]
#[serde(default)]
struct FixedOffsetDateTime(DateTime<FixedOffset>);

impl defmt::Format for FixedOffsetDateTime {
    fn format(&self, f: defmt::Formatter) {
        defmt::write!(
            f,
            "{:04}/{:02}/{:02} {:02}:{:02}",
            self.0.year(),
            self.0.month(),
            self.0.day(),
            self.0.hour(),
            self.0.minute()
        );
    }
}

pub fn high_low_temp(
    w: &OpenWeather,
    now: &DateTime<FixedOffset>,
) -> ((DateTime<FixedOffset>, f32), (DateTime<FixedOffset>, f32)) {
    let mut high = &w.hourly[0];
    let mut low = &w.hourly[0];

    let tz_offset = FixedOffset::east_opt(w.timezone_offset as i32).unwrap();

    for h in w.hourly.iter() {
        let ts = Utc.timestamp_opt(h.dt, 0).earliest().unwrap();
        if timestamp_before_now(&ts, &now.to_utc()) {
            continue;
        }

        if timestamp_after_24_hours(&ts, &now.to_utc()) {
            continue;
        }

        if h.temp > high.temp {
            high = h
        }

        if h.temp < low.temp {
            low = h
        }
    }

    let h = tz_offset.timestamp_opt(high.dt, 0).earliest().unwrap();
    let l = tz_offset.timestamp_opt(low.dt, 0).earliest().unwrap();

    ((h, high.temp), (l, low.temp))
}

fn timestamp_before_now(ts: &DateTime<Utc>, now: &DateTime<Utc>) -> bool {
    *ts - *now < TimeDelta::zero()
}

fn timestamp_after_24_hours(ts: &DateTime<Utc>, now: &DateTime<Utc>) -> bool {
    *ts - *now > TimeDelta::try_hours(24).unwrap()
}

async fn make_request<'a, 'b, 'c>(
    url: &str,
    stack: Stack<'a>,
    rx_buffer: &'b mut [u8; 32768],
) -> Result<&'b str, &'c str> {
    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp_client = TcpClient::new(stack, &client_state);
    let dns_client = DnsSocket::new(stack);

    let mut http_client = HttpClient::new(&tcp_client, &dns_client);

    defmt::info!("connecting to {}", url);

    let mut request = match http_client.request(Method::GET, url).await {
        Ok(req) => req,
        Err(e) => {
            defmt::error!("Failed to make HTTP request: {:?}", e);
            return Err("Failed to make HTTP request"); // handle the error
        }
    };

    let response = match request.send(rx_buffer).await {
        Ok(resp) => resp,
        Err(_e) => {
            defmt::error!("Failed to send HTTP request");
            return Err("Failed to send HTTP request"); // handle the error
        }
    };

    let body = match from_utf8(response.body().read_to_end().await.unwrap()) {
        Ok(b) => b,
        Err(_e) => {
            defmt::error!("Failed to read response body");
            return Err("Failed to read response body"); // handle the error
        }
    };
    defmt::info!("Response body: {:?}", &body);

    Ok(body)
}

fn time_in_local(timezone_offset: i32, unixtime: i64) -> DateTime<FixedOffset> {
    let tz_offset = FixedOffset::east_opt(timezone_offset).unwrap();
    tz_offset.timestamp_opt(unixtime, 0).earliest().unwrap()
}
