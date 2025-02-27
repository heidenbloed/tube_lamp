#[macro_use]
extern crate dotenv_codegen;

use core::pin::pin;
use core::time::Duration;

use embassy_futures::select::{select, Either};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::*;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::timer::{EspAsyncTimer, EspTaskTimerService, EspTimerService};
use esp_idf_svc::wifi::*;
use log::*;

const SSID: &str = dotenv!("WLAN_SSID");
const PASSWORD: &str = dotenv!("WLAN_PASSWORD");
const MQTT_URL: &str = dotenv!("MQTT_URL");
const MQTT_USERNAME: &str = dotenv!("MQTT_USERNAME");
const MQTT_PASSWORD: &str = dotenv!("MQTT_PASSWORD");
const MQTT_CLIENT_ID: &str = dotenv!("MQTT_CLIENT_ID");
const MQTT_TOPIC: &str = dotenv!("MQTT_TOPIC");

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let sys_loop = EspSystemEventLoop::take().unwrap();
    let timer_service = EspTimerService::new().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();

    esp_idf_svc::hal::task::block_on(async {
        let _wlan = connect_wlan(&sys_loop, &timer_service, &nvs)
            .await
            .expect("Failed to connect to WLAN.");

        let (mut client, mut conn) = connect_mqtt().expect("Failed to connect to MQTT broker.");

        let mut timer = timer_service.timer_async()?;
        run(&mut client, &mut conn, &mut timer, MQTT_TOPIC).await
    })
    .unwrap();
}

async fn run(
    client: &mut EspAsyncMqttClient,
    connection: &mut EspAsyncMqttConnection,
    timer: &mut EspAsyncTimer,
    topic: &str,
) -> Result<(), EspError> {
    info!("About to start the MQTT connection.");

    let res = select(
        pin!(async move {
            info!("MQTT Listening for messages.");
            while let Ok(event) = connection.next().await {
                info!("[Queue] Event: {}", event.payload());
            }
            info!("Connection closed");
            Ok(())
        }),
        pin!(async move {
            loop {
                if let Err(e) = client.subscribe(topic, QoS::AtMostOnce).await {
                    error!("Failed to subscribe to topic \"{topic}\": {e}, retrying...");
                    timer.after(Duration::from_millis(500)).await?;
                    continue;
                }
                info!("Subscribed to topic \"{topic}\"");
                timer.after(Duration::from_millis(500)).await?;
                loop {
                    let sleep_secs = 2;
                    info!("Now sleeping for {sleep_secs}s...");
                    timer.after(Duration::from_secs(sleep_secs)).await?;
                }
            }
        }),
    )
    .await;

    match res {
        Either::First(res) => res,
        Either::Second(res) => res,
    }
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
) -> Result<EspWifi<'static>, EspError> {
    let peripherals = Peripherals::take()?;
    let mut esp_wlan = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs.clone()))?;
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
