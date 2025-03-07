#[macro_use]
extern crate dotenv_codegen;

use core::pin::pin;
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
use smart_leds::hsv::{hsv2rgb, Hsv};
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

async fn run_mqtt_and_led_loop(
    client: &mut EspAsyncMqttClient,
    connection: &mut EspAsyncMqttConnection,
    timer: &mut EspAsyncTimer,
    led_driver: &mut Ws2812Esp32Rmt<'_>,
) -> Result<(), EspError> {
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
                tube_lamp_tick(led_driver, &rx)?;
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

#[derive(Debug)]
enum Command {
    Mode,
    Rgb,
    Hsv(u8, u8, u8),
    Hex,
    Warm,
    Progress,
    Wheel,
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
                        warn!("Not yet implemented.");
                        tx.send(Command::Mode).unwrap();
                    }
                    Some(MQTT_TOPIC_RGB) => {
                        info!("Received color change (RGB) message.");
                        warn!("Not yet implemented.");
                        tx.send(Command::Rgb).unwrap();
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
                        warn!("Not yet implemented.");
                        tx.send(Command::Hex).unwrap();
                    }
                    Some(MQTT_TOPIC_WARM) => {
                        info!("Received color change (warm) message.");
                        warn!("Not yet implemented.");
                        tx.send(Command::Warm).unwrap();
                    }
                    Some(MQTT_TOPIC_PROGRESS) => {
                        info!("Received progress change message.");
                        warn!("Not yet implemented.");
                        tx.send(Command::Progress).unwrap();
                    }
                    Some(MQTT_TOPIC_WHEEL_SPEED) => {
                        info!("Received wheel change message.");
                        warn!("Not yet implemented.");
                        tx.send(Command::Wheel).unwrap();
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
    rx: &mpsc::Receiver<Command>,
) -> Result<(), EspError> {
    debug!("Tube lamp tick");

    if let Ok(command) = rx.try_recv() {
        info!("Received in lamp tick command: {command:?}");
        match command {
            Command::Hsv(hue, sat, val) => {
                led_driver
                    .write(
                        std::iter::repeat(hsv2rgb(Hsv {
                            hue: hue,
                            sat: sat,
                            val: val,
                        }))
                        .take(NUM_LEDS),
                    )
                    .expect("Could not write to LED driver.");
            }
            _ => {
                warn!("Command not yet implemented.");
            }
        }
    }
    Ok(())
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
