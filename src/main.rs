#[macro_use]
extern crate dotenv_codegen;

use core::pin::pin;
use std::num::Wrapping;
use std::str::FromStr;
use std::sync::mpsc;
use std::time::Duration;

use embassy_futures::select::{select, Either};
use esp_idf_hal::modem::Modem;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::mqtt::client::*;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::timer::{EspAsyncTimer, EspTaskTimerService, EspTimerService};
use esp_idf_svc::wifi::*;
use log::*;
use once_cell::sync::Lazy;
use smart_leds::hsv::{hsv2rgb, Hsv};
use smart_leds::RGB8;
use smart_leds_trait::SmartLedsWrite;
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

const SSID: &str = dotenv!("WLAN_SSID");
const PASSWORD: &str = dotenv!("WLAN_PASSWORD");
const MQTT_URL: &str = dotenv!("MQTT_URL");
const MQTT_USERNAME: &str = dotenv!("MQTT_USERNAME");
const MQTT_PASSWORD: &str = dotenv!("MQTT_PASSWORD");
const MQTT_CLIENT_ID: &str = dotenv!("MQTT_CLIENT_ID");

const MQTT_TOPIC_MODE: &str = "lamps/tube/mode";
const MQTT_TOPIC_RGB: &str = "lamps/tube/rgb";
const MQTT_TOPIC_HSV: &str = "lamps/tube/hsv";
const MQTT_TOPIC_HEX: &str = "lamps/tube/hex";
const MQTT_TOPIC_WARM: &str = "lamps/tube/warm";
const MQTT_TOPIC_PROGRESS: &str = "lamps/tube/progress";
const MQTT_TOPIC_WHEEL_SPEED: &str = "lamps/tube/wheel_speed";

const NUM_LEDS: usize = 5;
static RAINBOW_COLORS: Lazy<[RGB8; NUM_LEDS]> = Lazy::new(|| {
    let mut rainbow_colors = [RGB8 { r: 0, g: 0, b: 0 }; NUM_LEDS];
    for (idx, color) in rainbow_colors.iter_mut().enumerate() {
        *color = hsv2rgb(Hsv {
            hue: (idx * 255 / NUM_LEDS) as u8,
            sat: 255,
            val: 255,
        });
    }
    rainbow_colors
});
static WARM_COLORS: Lazy<[RGB8; u8::MAX as usize + 1]> = Lazy::new(|| {
    let mut warm_colors = [RGB8 { r: 0, g: 0, b: 0 }; u8::MAX as usize + 1];
    for (idx, color) in warm_colors.iter_mut().enumerate() {
        *color = hsv2rgb(Hsv {
            hue: 40,
            sat: 160,
            val: idx as u8,
        });
    }
    warm_colors
});

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let sys_loop = EspSystemEventLoop::take().expect("Failed to take system event loop.");
    let timer_service = EspTimerService::new().expect("Failed to create timer service.");
    let nvs = EspDefaultNvsPartition::take().expect("Failed to take NVS partition.");

    let peripherals = Peripherals::take().expect("Failed to take peripherals.");
    let led_pin = peripherals.pins.gpio19;
    let rmt_channel = peripherals.rmt.channel0;
    let mut led_driver =
        Ws2812Esp32Rmt::new(rmt_channel, led_pin).expect("Failed to create LED driver.");

    esp_idf_svc::hal::task::block_on(async {
        let _wlan = connect_wlan(&sys_loop, &timer_service, &nvs, peripherals.modem)
            .await
            .expect("Failed to connect to WLAN.");

        let (mut client, mut conn) = connect_mqtt().expect("Failed to connect to MQTT broker.");

        let mut timer = timer_service.timer_async()?;
        run_mqtt_and_led_loop(&mut client, &mut conn, &mut timer, &mut led_driver).await
    })
    .unwrap();
}

#[derive(Debug)]
enum LampMode {
    Color,
    Rainbow,
    Space,
    Progress,
}

impl core::str::FromStr for LampMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "color" => Ok(LampMode::Color),
            "rainbow" => Ok(LampMode::Rainbow),
            "space" => Ok(LampMode::Space),
            "progress" => Ok(LampMode::Progress),
            _ => Err(()),
        }
    }
}

#[derive(Debug)]
enum Command {
    Mode(LampMode),
    Rgb(u8, u8, u8),
    Hsv(u8, u8, u8),
    Warm(u8),
    Progress(u8),
    WheelSpeed(u16),
}

#[derive(Debug)]
struct Lampstate {
    mode: LampMode,
    color: RGB8,
    wheel_pos: Wrapping<u16>,
    wheel_speed: u16,
    progress: u8,
}

impl Lampstate {
    fn new() -> Self {
        Self {
            mode: LampMode::Color,
            color: RGB8 { r: 0, g: 0, b: 0 },
            wheel_pos: Wrapping(0),
            wheel_speed: 300,
            progress: 0,
        }
    }
}

async fn run_mqtt_and_led_loop(
    client: &mut EspAsyncMqttClient,
    connection: &mut EspAsyncMqttConnection,
    timer: &mut EspAsyncTimer,
    led_driver: &mut Ws2812Esp32Rmt<'_>,
) -> Result<(), EspError> {
    let mut lampstate = Lampstate::new();
    let (tx, rx) = mpsc::channel();

    info!("About to start the MQTT connection.");

    let res = select(
        pin!(async move {
            info!("MQTT Listening for messages.");
            while let Ok(event) = connection.next().await {
                handle_mqtt_event(&event.payload(), &tx);
            }
            info!("Connection closed");
            Ok(())
        }),
        pin!(async move {
            for topic in [
                MQTT_TOPIC_MODE,
                MQTT_TOPIC_RGB,
                MQTT_TOPIC_HSV,
                MQTT_TOPIC_HEX,
                MQTT_TOPIC_WARM,
                MQTT_TOPIC_PROGRESS,
                MQTT_TOPIC_WHEEL_SPEED,
            ]
            .iter()
            {
                loop {
                    if let Err(e) = client.subscribe(topic, QoS::AtMostOnce).await {
                        error!("Failed to subscribe to topic \"{topic}\": {e}, retrying...");
                        timer.after(Duration::from_millis(500)).await?;
                        continue;
                    }
                    info!("Subscribed to topic \"{topic}\"");
                    break;
                }
            }

            timer.after(Duration::from_millis(500)).await?;
            loop {
                tube_lamp_tick(led_driver, &mut lampstate, &rx)?;
                timer.after(Duration::from_millis(10)).await?;
            }
        }),
    )
    .await;

    match res {
        Either::First(res) => res,
        Either::Second(res) => res,
    }
}

fn handle_mqtt_event(event_payload: &EventPayload<'_, EspError>, tx: &mpsc::Sender<Command>) {
    match event_payload {
        EventPayload::Received {
            id,
            topic,
            data,
            details: _,
        } => {
            if let Ok(msg) = core::str::from_utf8(data) {
                info!("Received MQTT message from id \"{id}\" on topic \"{topic:?}\": {msg:?}.");
                match topic {
                    Some(MQTT_TOPIC_MODE) => {
                        info!("Received mode change message.");
                        if let Ok(lamp_mode) = LampMode::from_str(msg) {
                            info!("Parsed lamp mode={lamp_mode:?}");
                            tx.send(Command::Mode(lamp_mode)).unwrap();
                        } else {
                            warn!("Could not parse lamp mode.");
                        }
                    }
                    Some(MQTT_TOPIC_RGB) => {
                        info!("Received color change (RGB) message.");
                        let mut msg_conv_successful = false;
                        let msg_split: Vec<_> =
                            msg.split(",").take(3).map(|s| s.parse::<u8>()).collect();
                        if msg_split.len() == 3 {
                            if let Ok(red) = msg_split[0] {
                                if let Ok(green) = msg_split[1] {
                                    if let Ok(blue) = msg_split[2] {
                                        info!(
                                            "Parsed RGB values: red={red}, green={green}, blue={blue}."
                                        );
                                        tx.send(Command::Rgb(red, green, blue)).unwrap();
                                        tx.send(Command::Mode(LampMode::Color)).unwrap();
                                        msg_conv_successful = true;
                                    }
                                }
                            }
                        }
                        if !msg_conv_successful {
                            warn!("Could not parse RGB values.");
                        }
                    }
                    Some(MQTT_TOPIC_HSV) => {
                        info!("Received color change (HSV) message.");
                        let mut msg_conv_successful = false;
                        let msg_split: Vec<_> =
                            msg.split(",").take(3).map(|s| s.parse::<u8>()).collect();
                        if msg_split.len() == 3 {
                            if let Ok(hue) = msg_split[0] {
                                if let Ok(sat) = msg_split[1] {
                                    if let Ok(val) = msg_split[2] {
                                        info!(
                                            "Parsed HSV values: hue={hue}, sat={sat}, val={val}."
                                        );
                                        tx.send(Command::Hsv(hue, sat, val)).unwrap();
                                        tx.send(Command::Mode(LampMode::Color)).unwrap();
                                        msg_conv_successful = true;
                                    }
                                }
                            }
                        }
                        if !msg_conv_successful {
                            warn!("Could not parse HSV values.");
                        }
                    }
                    Some(MQTT_TOPIC_HEX) => {
                        info!("Received color change (HEX) message.");
                        let mut msg_conv_successful = false;
                        let hex = msg.trim_start_matches('#');
                        if hex.len() == 6 {
                            if let Ok(red) = u8::from_str_radix(&hex[0..2], 16) {
                                if let Ok(green) = u8::from_str_radix(&hex[2..4], 16) {
                                    if let Ok(blue) = u8::from_str_radix(&hex[4..6], 16) {
                                        info!(
                                            "Parsed HEX values: red={red}, green={green}, blue={blue}."
                                        );
                                        tx.send(Command::Rgb(red, green, blue)).unwrap();
                                        tx.send(Command::Mode(LampMode::Color)).unwrap();
                                        msg_conv_successful = true;
                                    }
                                }
                            }
                        }
                        if !msg_conv_successful {
                            warn!("Could not parse HEX values.");
                        }
                    }
                    Some(MQTT_TOPIC_WARM) => {
                        info!("Received color change (warm) message.");
                        if let Ok(warm_brightness) = msg.parse::<u8>() {
                            info!("Parsed warm brightness: {warm_brightness}.");
                            tx.send(Command::Warm(warm_brightness)).unwrap();
                        } else {
                            warn!("Could not parse warm brightness.");
                        }
                    }
                    Some(MQTT_TOPIC_PROGRESS) => {
                        info!("Received progress change message.");
                        if let Ok(progress) = msg.parse::<u8>() {
                            info!("Parsed progress: {progress}.");
                            tx.send(Command::Progress(progress)).unwrap();
                        } else {
                            warn!("Could not parse progress.");
                        }
                    }
                    Some(MQTT_TOPIC_WHEEL_SPEED) => {
                        info!("Received wheel change message.");
                        if let Ok(wheel_speed) = msg.parse::<u16>() {
                            info!("Parsed wheel speed: {wheel_speed}.");
                            tx.send(Command::WheelSpeed(wheel_speed)).unwrap();
                        } else {
                            warn!("Could not parse wheel speed.");
                        }
                    }
                    Some(topic) => {
                        error!("Unexpected MQTT topic: \"{topic}\".");
                    }
                    None => {
                        error!("No MQTT topic.");
                    }
                }
            } else {
                error!("Failed to parse MQTT message from id \"{id}\" on topic \"{topic:?}\".");
            }
        }
        _ => {
            info!("Other MQTT event occurred: {event_payload}");
        }
    }
}

fn tube_lamp_tick(
    led_driver: &mut Ws2812Esp32Rmt,
    lamp_state: &mut Lampstate,
    rx: &mpsc::Receiver<Command>,
) -> Result<(), EspError> {
    debug!("Tube lamp tick");

    let led_driver_res = match lamp_state.mode {
        LampMode::Color => led_driver.write(std::iter::repeat(lamp_state.color).take(NUM_LEDS)),
        LampMode::Rainbow => led_driver.write(
            std::iter::repeat(
                RAINBOW_COLORS
                    [lamp_state.wheel_pos.0 as usize * RAINBOW_COLORS.len() / u16::MAX as usize],
            )
            .take(NUM_LEDS),
        ),
        LampMode::Space => led_driver.write((0..RAINBOW_COLORS.len()).map(|idx| {
            let space_idx = (idx
                + (lamp_state.wheel_pos.0 as usize * RAINBOW_COLORS.len() / u16::MAX as usize))
                % RAINBOW_COLORS.len();
            RAINBOW_COLORS[space_idx]
        })),
        LampMode::Progress => {
            let num_green_leds = lamp_state.progress as usize * NUM_LEDS / u8::MAX as usize;
            let color_iter = std::iter::repeat(RGB8 { r: 0, g: 255, b: 0 })
                .take(num_green_leds)
                .chain(
                    std::iter::repeat(RGB8 { r: 255, g: 0, b: 0 }).take(NUM_LEDS - num_green_leds),
                );
            led_driver.write(color_iter)
        }
    };
    led_driver_res.expect("Could not write to LED driver.");

    if let Ok(command) = rx.try_recv() {
        info!("Received in lamp tick command: {command:?}");
        update_lamp_state(lamp_state, command);
    }

    lamp_state.wheel_pos += lamp_state.wheel_speed;
    Ok(())
}

fn update_lamp_state(lamp_state: &mut Lampstate, command: Command) {
    match command {
        Command::Mode(lamp_mode) => {
            info!("Set lamp mode to: {lamp_mode:?}");
            lamp_state.mode = lamp_mode;
        }
        Command::Hsv(hue, sat, val) => {
            info!("Set lamp color to: hue={hue} sat={sat} val={val}");
            lamp_state.color = hsv2rgb(Hsv { hue, sat, val });
        }
        Command::Rgb(red, green, blue) => {
            info!("Set lamp color to: red={red} green={green} blue={blue}");
            lamp_state.color = RGB8 {
                r: red,
                g: green,
                b: blue,
            };
        }
        Command::Warm(warm_brightness) => {
            info!("Set warm brightness to: {warm_brightness}");
            lamp_state.color = WARM_COLORS[warm_brightness as usize];
        }
        Command::Progress(progress) => {
            info!("Set progress to: {progress}");
            lamp_state.progress = progress;
        }
        Command::WheelSpeed(wheel_speed) => {
            info!("Set wheel speed to: {wheel_speed}");
            lamp_state.wheel_speed = wheel_speed;
        }
    }
    info!("New lamp state: {lamp_state:?}");
}

fn connect_mqtt() -> Result<(EspAsyncMqttClient, EspAsyncMqttConnection), EspError> {
    let (mqtt_client, mqtt_conn) = EspAsyncMqttClient::new(
        MQTT_URL,
        &MqttClientConfiguration {
            client_id: Some(MQTT_CLIENT_ID),
            username: Some(MQTT_USERNAME),
            password: Some(MQTT_PASSWORD),
            ..Default::default()
        },
    )?;
    info!("MQTT client created.");

    Ok((mqtt_client, mqtt_conn))
}

async fn connect_wlan(
    sys_loop: &EspSystemEventLoop,
    timer_service: &EspTaskTimerService,
    nvs: &EspDefaultNvsPartition,
    modem: Modem,
) -> Result<EspWifi<'static>, EspError> {
    let mut esp_wlan = EspWifi::new(modem, sys_loop.clone(), Some(nvs.clone()))?;
    let mut wlan = AsyncWifi::wrap(&mut esp_wlan, sys_loop.clone(), timer_service.clone())?;

    wlan.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: SSID.try_into().unwrap(),
        password: PASSWORD.try_into().unwrap(),
        ..Default::default()
    }))?;

    wlan.start().await?;
    info!("WLAN started.");
    wlan.connect().await?;
    info!("WLAN connected.");
    wlan.wait_netif_up().await?;
    info!("WLAN netif up.");

    Ok(esp_wlan)
}
