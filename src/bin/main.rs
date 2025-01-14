#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]

use chrono::{DateTime, Datelike, FixedOffset, TimeDelta, TimeZone, Timelike, Utc};
use core::fmt::{Display, Formatter};
use core::marker::PhantomData;
use core::str::from_utf8;
use core::*;
use cyw43::JoinOptions;
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
use heapless::Vec;
use rand::RngCore;
use reqwless::client::HttpClient;
use reqwless::request::Method;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _, serde_json_core};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const WEATHER_URL: &str = env!("WEATHER_URL");

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

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("Hello World!");

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
        let mut rx_buffer = [0; 32768];

        let client_state = TcpClientState::<1, 1024, 1024>::new();
        let tcp_client = TcpClient::new(stack, &client_state);
        let dns_client = DnsSocket::new(stack);

        let mut http_client = HttpClient::new(&tcp_client, &dns_client);
        let url = WEATHER_URL;

        defmt::info!("connecting to {}", &url);

        let mut request = match http_client.request(Method::GET, &url).await {
            Ok(req) => req,
            Err(e) => {
                defmt::error!("Failed to make HTTP request: {:?}", e);
                return; // handle the error
            }
        };

        let response = match request.send(&mut rx_buffer).await {
            Ok(resp) => resp,
            Err(_e) => {
                defmt::error!("Failed to send HTTP request");
                return; // handle the error;
            }
        };

        let body = match from_utf8(response.body().read_to_end().await.unwrap()) {
            Ok(b) => b,
            Err(_e) => {
                defmt::error!("Failed to read response body");
                return; // handle the error
            }
        };
        defmt::info!("Response body: {:?}", &body);

        let bytes = body.as_bytes();
        match serde_json_core::de::from_slice::<OpenWeather>(bytes) {
            Ok((output, _used)) => {
                defmt::info!(
                    "lat/lon: {:?}, {:?} - timezone: {:?}",
                    output.lat,
                    output.lon,
                    output.timezone
                );

                defmt::info!("current: {:?}", output.current);

                let ((h_time, h_temp), (l_time, l_temp)) = high_low_temp(&output);

                defmt::info!("High: {:?} at {:?}", h_temp, FixedOffsetDateTime(h_time));
                defmt::info!("Low: {:?} at {:?}", l_temp, FixedOffsetDateTime(l_time));
            }
            Err(_e) => {
                defmt::error!("Failed to parse response body");
                return; // handle the error
            }
        }

        Timer::after(Duration::from_secs(20)).await;
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct OpenWeather<'a> {
    pub lat: f32,
    pub lon: f32,
    pub timezone: &'a str,
    pub timezone_offset: f32,
    pub current: Current<'a>,
    pub minutely: MinutelyBuffer,
    #[serde(borrow)]
    pub hourly: Vec<Hourly<'a>, 48>,
    // #[serde(borrow)]
    // pub daily: [Daily<'a>; 8],
}

#[derive(Debug, defmt::Format)]
pub struct MinutelyBuffer([Minutely; 60]);

impl<'de> Deserialize<'de> for MinutelyBuffer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MinutelyBufferVisitor {
            marker: PhantomData<fn() -> [Minutely; 60]>,
        }

        impl MinutelyBufferVisitor {
            fn new() -> Self {
                MinutelyBufferVisitor {
                    marker: PhantomData,
                }
            }
        }

        impl<'de> Visitor<'de> for MinutelyBufferVisitor {
            // The type that our Visitor is going to produce.
            type Value = MinutelyBuffer;

            // Format a message stating what data this Visitor expects to receive.
            fn expecting(&self, formatter: &mut Formatter) -> core::fmt::Result {
                formatter.write_str("a very special map")
            }

            // Deserialize MyMap from an abstract "map" provided by the
            // Deserializer. The MapAccess input is a callback provided by
            // the Deserializer to let us see each entry in the map.
            fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
            where
                S: SeqAccess<'de>,
            {
                let mut buf = MinutelyBuffer(
                    [Minutely {
                        dt: 0,
                        precipitation: 0.0,
                    }; 60],
                );

                for i in 0..60 {
                    buf.0[i] = match (seq.next_element())? {
                        Some(val) => val,
                        None => {
                            return Err(de::Error::custom("ran out of elements to deserialize"));
                        }
                    };
                }

                // TODO: return an error if there were more than 60 input elements?

                // seq.end()?;

                Ok(buf)
            }
        }

        deserializer.deserialize_seq(MinutelyBufferVisitor::new())
    }
}

impl Default for MinutelyBuffer {
    fn default() -> Self {
        MinutelyBuffer(
            [Minutely {
                dt: 0,
                precipitation: 0.0,
            }; 60],
        )
    }
}

#[derive(Deserialize, Debug, Default, defmt::Format)]
#[serde(default)]
pub struct Current<'a> {
    pub dt: i64,
    pub sunrise: i64,
    pub sunset: i64,
    pub temp: f32,
    pub feels_like: f32,
    pub pressure: f32,
    pub humidity: f32,
    pub dew_point: f32,
    pub uvi: f32,
    pub clouds: i32,
    pub visibility: i32,
    pub wind_speed: f32,
    pub wind_deg: f32,
    pub wind_gust: f32,
    pub rain: Rain,
    pub snow: Snow,
    #[serde(borrow)]
    pub weather: [Weather<'a>; 1],
}

#[derive(Deserialize, Debug, Default, defmt::Format)]
#[serde(default)]
pub struct Rain {
    #[serde(rename = "1h")]
    pub one_hour: f32,
}

#[derive(Deserialize, Debug, Default, defmt::Format)]
#[serde(default)]
pub struct Snow {
    #[serde(rename = "1h")]
    pub one_hour: f32,
}

#[derive(Deserialize, Debug, Default, defmt::Format)]
#[serde(default)]
pub struct Weather<'a> {
    pub id: i32,
    pub main: Main,
    pub description: &'a str,
    pub icon: &'a str,
}

#[derive(
    Default, Clone, Copy, Deserialize, Debug, PartialEq, Eq, PartialOrd, Ord, defmt::Format,
)]
pub enum Main {
    Thunderstorm,
    Drizzle,
    Rain,
    Snow,
    Mist,
    Smoke,
    Haze,
    Dust,
    Fog,
    Sand,
    Ash,
    Squall,
    Tornado,
    #[default]
    Clear,
    Clouds,
}

impl Display for Main {
    fn fmt(&self, f: &mut Formatter) -> core::fmt::Result {
        match self {
            Main::Thunderstorm => write!(f, "Thunderstorm"),
            Main::Drizzle => write!(f, "Drizzle"),
            Main::Rain => write!(f, "Rain"),
            Main::Snow => write!(f, "Snow"),
            Main::Mist => write!(f, "Mist"),
            Main::Smoke => write!(f, "Smoke"),
            Main::Haze => write!(f, "Haze"),
            Main::Dust => write!(f, "Dust"),
            Main::Fog => write!(f, "Fog"),
            Main::Sand => write!(f, "Sand"),
            Main::Ash => write!(f, "Ash"),
            Main::Squall => write!(f, "Squall"),
            Main::Tornado => write!(f, "Tornado"),
            Main::Clear => write!(f, "Clear"),
            Main::Clouds => write!(f, "Clouds"),
        }
    }
}

#[derive(Deserialize, Debug, Default, Copy, Clone, defmt::Format)]
#[serde(default)]
pub struct Minutely {
    pub dt: i64,
    pub precipitation: f32,
}

#[derive(Deserialize, Debug, Default, defmt::Format)]
#[serde(default)]
pub struct Hourly<'a> {
    pub dt: i64,
    pub temp: f32,
    pub feels_like: f32,
    pub pressure: f32,
    pub humidity: f32,
    pub dew_point: f32,
    pub uvi: f32,
    pub clouds: i32,
    pub visibility: i32,
    pub wind_speed: f32,
    pub wind_deg: f32,
    pub wind_gust: f32,
    pub pop: f32,
    pub rain: Rain,
    pub snow: Snow,
    #[serde(borrow)]
    pub weather: [Weather<'a>; 1],
}

#[derive(Deserialize, Debug, Default, defmt::Format)]
#[serde(default)]
pub struct Daily<'a> {
    pub dt: i64,
    pub sunrise: i64,
    pub sunset: i64,
    pub moonrise: i64,
    pub moonset: i64,
    pub moonphase: f32,
    pub temp: Temp,
    pub feels_like: FeelsLike,
    pub pressure: f32,
    pub humidity: f32,
    pub dew_point: f32,
    pub uvi: f32,
    pub pop: f32,
    pub clouds: i32,
    pub wind_speed: f32,
    pub wind_deg: f32,
    pub wind_gust: f32,
    pub rain: f32,
    pub snow: f32,
    #[serde(borrow)]
    pub weather: [Weather<'a>; 1],
}

#[derive(Deserialize, Debug, Default, Copy, Clone, defmt::Format)]
#[serde(default)]
pub struct Temp {
    pub morn: f32,
    pub day: f32,
    pub eve: f32,
    pub night: f32,
    pub min: f32,
    pub max: f32,
}

#[derive(Deserialize, Debug, Default, Copy, Clone, defmt::Format)]
#[serde(default)]
pub struct FeelsLike {
    pub morn: f32,
    pub day: f32,
    pub eve: f32,
    pub night: f32,
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
) -> ((DateTime<FixedOffset>, f32), (DateTime<FixedOffset>, f32)) {
    let mut high = &w.hourly[0];
    let mut low = &w.hourly[0];

    let tz_offset = FixedOffset::east_opt(w.timezone_offset as i32).unwrap();

    let now = tz_offset.timestamp_opt(w.current.dt, 0).earliest().unwrap();

    let nt = FixedOffsetDateTime(now);

    defmt::info!("Current time: {:?}", nt);

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
