use std::num::Wrapping;
use std::sync::mpsc;
use std::time::Duration;
use std::{iter, pin, str, str::FromStr};

use dotenv_codegen::dotenv;
use embassy_futures::select;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::{
    cpu::Core, modem::Modem, peripherals::Peripherals, rmt::config::TransmitConfig,
    rmt::TxRmtDriver, task,
};
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::mqtt::client::{
    EspAsyncMqttClient, EspAsyncMqttConnection, EventPayload, MqttClientConfiguration, QoS,
};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{self, EspError};
use esp_idf_svc::timer::{EspTaskTimerService, EspTimerService, Task};
use esp_idf_svc::wifi::{AsyncWifi, ClientConfiguration, Configuration, EspWifi};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use smart_leds::{hsv, RGB8};
use smart_leds_trait::SmartLedsWrite;
use ws2812_esp32_rmt_driver::Ws2812Esp32Rmt;

const WLAN_SSID: &str = dotenv!("WLAN_SSID");
const WLAN_PASSWORD: &str = dotenv!("WLAN_PASSWORD");
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

const NUM_LEDS: usize = 86;

static RAINBOW_COLORS: Lazy<[RGB8; NUM_LEDS]> = Lazy::new(|| {
    let mut rainbow_colors = [RGB8 { r: 0, g: 0, b: 0 }; NUM_LEDS];
    for (idx, color) in rainbow_colors.iter_mut().enumerate() {
        *color = hsv::hsv2rgb(hsv::Hsv {
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
        *color = hsv::hsv2rgb(hsv::Hsv {
            hue: 10,
            sat: 230,
            val: idx as u8,
        });
    }
    warm_colors
});

fn main() {
    sys::link_patches();
    EspLogger::initialize_default();

    let sys_loop = EspSystemEventLoop::take().expect("Failed to take system event loop.");
    let timer_service = EspTimerService::new().expect("Failed to create timer service.");
    let nvs = EspDefaultNvsPartition::take().expect("Failed to take NVS partition.");

    let peripherals = Peripherals::take().expect("Failed to take peripherals.");
    let led_pin = peripherals.pins.gpio19;
    let rmt_channel = peripherals.rmt.channel0;

    let touch_thresholds = match init_touch_sensor() {
        Ok(touch_thresholds) => Some(touch_thresholds),
        Err(err) => {
            error!(
                "Failed to initialize touch sensor: {} (code={}).",
                err.msg, err.code
            );
            None
        }
    };

    let mut lamp_state = LampState::new();

    let (mqtt_cmd_sender, touch_cmd_receiver) = mpsc::channel();
    let (touch_cmd_sender, lamp_cmd_receiver) = mpsc::channel();

    task::thread::ThreadSpawnConfiguration {
        name: Some("LEDS\0".as_bytes()),
        stack_size: 0x4000,
        pin_to_core: Some(Core::Core1),
        priority: 24,
        ..Default::default()
    }
    .set()
    .expect("Cannot set thread spawn config");

    std::thread::spawn(move || -> ! {
        let rmt_config = TransmitConfig::new().clock_divider(1).mem_block_num(8);
        let rmt_driver = TxRmtDriver::new(rmt_channel, led_pin, &rmt_config)
            .expect("Failed to create RMT driver.");
        let mut led_driver =
            Ws2812Esp32Rmt::new_with_rmt_driver(rmt_driver).expect("Failed to create LED driver.");
        loop {
            lamp_tick(&mut led_driver, &mut lamp_state, &lamp_cmd_receiver);
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    task::block_on(async {
        let _wlan = connect_wlan(&sys_loop, &timer_service, &nvs, peripherals.modem)
            .await
            .expect("Failed to connect to WLAN.");

        let (mut client, mut conn) = connect_mqtt().expect("Failed to connect to MQTT broker.");

        run_mqtt_touch_loop(
            &mut client,
            &mut conn,
            &timer_service,
            &mqtt_cmd_sender,
            &touch_cmd_receiver,
            &touch_cmd_sender,
            touch_thresholds,
        )
        .await
    })
    .unwrap();
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum LampMode {
    Off,
    Color,
    Rainbow,
    Space,
    Progress,
}

impl FromStr for LampMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" => Ok(LampMode::Off),
            "color" => Ok(LampMode::Color),
            "rainbow" => Ok(LampMode::Rainbow),
            "space" => Ok(LampMode::Space),
            "progress" => Ok(LampMode::Progress),
            _ => Err(()),
        }
    }
}

impl LampMode {
    fn next(&self) -> LampMode {
        match self {
            LampMode::Off => LampMode::Color,
            LampMode::Color => LampMode::Rainbow,
            LampMode::Rainbow => LampMode::Space,
            LampMode::Space => LampMode::Off,
            LampMode::Progress => LampMode::Off,
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
struct LampState {
    mode: LampMode,
    color: RGB8,
    wheel_pos: Wrapping<u16>,
    wheel_speed: u16,
    progress: u8,
}

impl LampState {
    fn new() -> Self {
        Self {
            mode: LampMode::Off,
            color: WARM_COLORS[(u8::MAX / 5) as usize],
            wheel_pos: Wrapping(0),
            wheel_speed: 300,
            progress: 0,
        }
    }
}

#[derive(Debug)]
struct TouchState {
    is_touched_mode: bool,
    is_touched_bright: bool,
    current_lamp_mode: LampMode,
    current_warm_brightness: u8,
}

impl TouchState {
    fn new() -> Self {
        Self {
            is_touched_mode: false,
            is_touched_bright: false,
            current_lamp_mode: LampMode::Off,
            current_warm_brightness: u8::MAX / 5,
        }
    }
}

async fn run_mqtt_touch_loop(
    client: &mut EspAsyncMqttClient,
    connection: &mut EspAsyncMqttConnection,
    timer_service: &EspTimerService<Task>,
    mqtt_cmd_sender: &mpsc::Sender<Command>,
    touch_cmd_receiver: &mpsc::Receiver<Command>,
    touch_cmd_sender: &mpsc::Sender<Command>,
    touch_thresholds: Option<(u16, u16)>,
) -> Result<(), EspError> {
    info!("About to start the MQTT connection.");
    let mut touch_state = TouchState::new();
    let mut touch_timer = timer_service.timer_async()?;

    let res: select::Either<Result<(), EspError>, Result<(), EspError>> = select::select(
        pin::pin!(async move {
            info!("Listening for MQTT messages.");
            while let Ok(event) = connection.next().await {
                handle_mqtt_event(&event.payload(), &mqtt_cmd_sender);
            }
            info!("Connection closed");
            Ok(())
        }),
        pin::pin!(async move {
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
                        touch_timer.after(Duration::from_millis(500)).await?;
                        continue;
                    }
                    info!("Subscribed to topic \"{topic}\"");
                    break;
                }
            }
            info!("Start touch sensor loop.");
            loop {
                touch_sensor_tick(
                    touch_thresholds,
                    &mut touch_state,
                    &touch_cmd_receiver,
                    &touch_cmd_sender,
                );
                touch_timer.after(Duration::from_millis(10)).await?;
            }
        }),
    )
    .await;

    match res {
        select::Either::First(res) => res,
        select::Either::Second(res) => res,
    }
}

fn handle_mqtt_event(
    event_payload: &EventPayload<'_, EspError>,
    cmd_sender: &mpsc::Sender<Command>,
) {
    match event_payload {
        EventPayload::Received {
            id,
            topic,
            data,
            details: _,
        } => {
            if let Ok(msg) = str::from_utf8(data) {
                info!("Received MQTT message from id \"{id}\" on topic \"{topic:?}\": {msg:?}.");
                match topic {
                    Some(MQTT_TOPIC_MODE) => {
                        info!("Received mode change message.");
                        if let Ok(lamp_mode) = LampMode::from_str(msg) {
                            info!("Parsed lamp mode={lamp_mode:?}");
                            cmd_sender.send(Command::Mode(lamp_mode)).unwrap();
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
                                        cmd_sender.send(Command::Mode(LampMode::Color)).unwrap();
                                        cmd_sender.send(Command::Rgb(red, green, blue)).unwrap();
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
                                        cmd_sender.send(Command::Mode(LampMode::Color)).unwrap();
                                        cmd_sender.send(Command::Hsv(hue, sat, val)).unwrap();
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
                                        cmd_sender.send(Command::Mode(LampMode::Color)).unwrap();
                                        cmd_sender.send(Command::Rgb(red, green, blue)).unwrap();
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
                            cmd_sender.send(Command::Mode(LampMode::Color)).unwrap();
                            cmd_sender.send(Command::Warm(warm_brightness)).unwrap();
                        } else {
                            warn!("Could not parse warm brightness.");
                        }
                    }
                    Some(MQTT_TOPIC_PROGRESS) => {
                        info!("Received progress change message.");
                        if let Ok(progress) = msg.parse::<u8>() {
                            info!("Parsed progress: {progress}.");
                            cmd_sender.send(Command::Progress(progress)).unwrap();
                        } else {
                            warn!("Could not parse progress.");
                        }
                    }
                    Some(MQTT_TOPIC_WHEEL_SPEED) => {
                        info!("Received wheel change message.");
                        if let Ok(wheel_speed) = msg.parse::<u16>() {
                            info!("Parsed wheel speed: {wheel_speed}.");
                            cmd_sender.send(Command::WheelSpeed(wheel_speed)).unwrap();
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

fn touch_sensor_tick(
    touch_thresholds: Option<(u16, u16)>,
    touch_state: &mut TouchState,
    cmd_receiver: &mpsc::Receiver<Command>,
    cmd_sender: &mpsc::Sender<Command>,
) {
    debug!("Touch sensor tick.");
    if let Some((touch_threshold_mode, touch_threshold_bright)) = touch_thresholds {
        match touch_sensor_read() {
            Ok((touch_value_mode, touch_value_bright)) => {
                if touch_value_mode < touch_threshold_mode {
                    if !touch_state.is_touched_mode {
                        info!("Touch detected (mode).");
                        touch_state.is_touched_mode = true;

                        let new_lamp_state = touch_state.current_lamp_mode.next();
                        touch_state.current_lamp_mode = new_lamp_state;
                        cmd_sender.send(Command::Mode(new_lamp_state)).unwrap();
                    }
                } else if touch_state.is_touched_mode {
                    touch_state.is_touched_mode = false;
                }

                if touch_value_bright < touch_threshold_bright {
                    if !touch_state.is_touched_bright {
                        info!("Touch detected (bright).");
                        touch_state.is_touched_bright = true;

                        let new_warm_brightness = if touch_state.current_warm_brightness == u8::MAX
                        {
                            0
                        } else if touch_state.current_warm_brightness
                            > (u8::MAX as u16 * 4 / 5) as u8
                        {
                            u8::MAX
                        } else {
                            touch_state.current_warm_brightness + u8::MAX / 5
                        };
                        touch_state.current_lamp_mode = LampMode::Color;
                        touch_state.current_warm_brightness = new_warm_brightness;
                        cmd_sender.send(Command::Mode(LampMode::Color)).unwrap();
                        cmd_sender.send(Command::Warm(new_warm_brightness)).unwrap();
                    }
                } else if touch_state.is_touched_bright {
                    touch_state.is_touched_bright = false;
                }
            }
            Err(err) => {
                error!(
                    "Failed to read touch sensor: {} (code: {})",
                    err.msg, err.code
                );
            }
        }
    }

    for command in cmd_receiver.try_iter() {
        info!("Received in touch tick: {command:?}");
        match command {
            Command::Mode(new_mode) => touch_state.current_lamp_mode = new_mode,
            Command::Warm(new_brightness) => touch_state.current_warm_brightness = new_brightness,
            _ => (),
        }
        cmd_sender.send(command).unwrap();
    }
}

fn lamp_tick(
    led_driver: &mut Ws2812Esp32Rmt,
    lamp_state: &mut LampState,
    cmd_receiver: &mpsc::Receiver<Command>,
) {
    debug!("Tube lamp tick");

    let led_driver_res = match lamp_state.mode {
        LampMode::Off => led_driver.write(iter::repeat(RGB8 { r: 0, g: 0, b: 0 }).take(NUM_LEDS)),
        LampMode::Color => led_driver.write(iter::repeat(lamp_state.color).take(NUM_LEDS)),
        LampMode::Rainbow => led_driver.write(
            iter::repeat(
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
        LampMode::Progress => led_driver.write((0..NUM_LEDS).map(|idx| {
            let hue: u8 = if idx < lamp_state.progress as usize * NUM_LEDS / u8::MAX as usize {
                85
            } else {
                0
            };
            let wave_idx = (NUM_LEDS - idx
                + lamp_state.wheel_pos.0 as usize * NUM_LEDS / u16::MAX as usize)
                % NUM_LEDS;
            let sat: u8 = 255 - (40 * wave_idx / NUM_LEDS) as u8;
            hsv::hsv2rgb(hsv::Hsv { hue, sat, val: 255 })
        })),
    };
    led_driver_res.expect("Could not write to LED driver.");

    for command in cmd_receiver.try_iter() {
        info!("Received in lamp tick: {command:?}");
        update_lamp_state(lamp_state, command);
    }

    lamp_state.wheel_pos += lamp_state.wheel_speed;
}

fn update_lamp_state(lamp_state: &mut LampState, command: Command) {
    match command {
        Command::Mode(lamp_mode) => {
            info!("Set lamp mode to: {lamp_mode:?}");
            lamp_state.mode = lamp_mode;
        }
        Command::Hsv(hue, sat, val) => {
            info!("Set lamp color to: hue={hue} sat={sat} val={val}");
            lamp_state.color = hsv::hsv2rgb(hsv::Hsv { hue, sat, val });
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
    if (lamp_state.mode == LampMode::Color) && (lamp_state.color == RGB8 { r: 0, g: 0, b: 0 }) {
        lamp_state.mode = LampMode::Off;
        lamp_state.color = WARM_COLORS[(u8::MAX / 5) as usize];
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
        ssid: WLAN_SSID.try_into().unwrap(),
        password: WLAN_PASSWORD.try_into().unwrap(),
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

struct TouchPadErr {
    code: i32,
    msg: &'static str,
}

fn init_touch_sensor() -> Result<(u16, u16), TouchPadErr> {
    let res = unsafe { sys::touch_pad_init() };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad init failed.",
        });
    }
    info!("Touch pad init success.");

    let res = unsafe {
        sys::touch_pad_set_voltage(
            sys::touch_high_volt_t_TOUCH_HVOLT_2V7,
            sys::touch_low_volt_t_TOUCH_LVOLT_0V5,
            sys::touch_volt_atten_t_TOUCH_HVOLT_ATTEN_1V,
        )
    };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad set voltage failed.",
        });
    }
    info!("Touch pad set voltage success.");

    let res = unsafe { sys::touch_pad_config(sys::touch_pad_t_TOUCH_PAD_NUM0, 0) };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad 0 config failed.",
        });
    }
    let res = unsafe { sys::touch_pad_config(sys::touch_pad_t_TOUCH_PAD_NUM2, 0) };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad 1 config failed.",
        });
    }
    info!("Touch pad config success.");

    let res = unsafe { sys::touch_pad_filter_start(10) };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad filter start failed.",
        });
    }
    info!("Touch pad filter start success.");

    let (init_touch_value_0, init_touch_value_1) = touch_sensor_read()?;
    info!("Initial touch pad filtered values: 0:{init_touch_value_0}, 1:{init_touch_value_1}");

    let touch_threshold_0 = init_touch_value_0 * 2 / 3;
    let touch_threshold_1 = init_touch_value_1 * 2 / 3;
    info!("Touch threshold value: 0:{touch_threshold_0}, 1:{touch_threshold_1}");

    Ok((touch_threshold_0, touch_threshold_1))
}

fn touch_sensor_read() -> Result<(u16, u16), TouchPadErr> {
    let mut touch_value_0: u16 = 0;
    let mut touch_value_1: u16 = 0;
    let res = unsafe {
        sys::touch_pad_read_filtered(sys::touch_pad_t_TOUCH_PAD_NUM0, &mut touch_value_0)
    };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad 0 filtered read failed.",
        });
    }
    let res = unsafe {
        sys::touch_pad_read_filtered(sys::touch_pad_t_TOUCH_PAD_NUM2, &mut touch_value_1)
    };
    if res != sys::ESP_OK {
        return Err(TouchPadErr {
            code: res,
            msg: "Touch pad 1 filtered read failed.",
        });
    }
    Ok((touch_value_0, touch_value_1))
}
