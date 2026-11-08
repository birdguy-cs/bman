use std::fs;

/// Where audio goes when the system-audio test path isn't forced on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Hardware PWM on the GPIO pins.
    Wired,
    /// The system audio device, which is the Bluetooth headphones once connected.
    Wireless,
}

impl Output {
    pub fn toggle(self) -> Self {
        match self {
            Output::Wired => Output::Wireless,
            Output::Wireless => Output::Wired,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Output::Wired => "Wired",
            Output::Wireless => "Wireless",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "wired" => Some(Output::Wired),
            "wireless" => Some(Output::Wireless),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub volume: f32,
    /// Which display driver to use: "whisplay" or "window".
    /// See display_driver.rs for the list.
    pub driver: String,
    /// Which battery driver to use (default "auto").
    /// See battery.rs for the list.
    pub battery: String,
    /// Always play through the system audio device (for testing on a PC).
    pub use_system_audio: bool,
    /// Main UI colour as 0xFFRRGGBB. In config.txt: accent=R,G,B (0-255 each).
    pub accent: u32,
    pub output: Output,
    /// Seconds without a button press before the screen goes idle (30-300).
    pub screen_timeout: u32,
    /// Backlight brightness in percent (10-100).
    pub brightness: u8,
    /// Wi-Fi networks to save in NetworkManager at startup: (SSID, password).
    /// In config.txt: wifi=SSID,password (one line per network; leave the
    /// password empty for an open network).
    pub wifi: Vec<(String, String)>,
    /// Look for a new release now and then (never installs on its own).
    pub update_check: bool,
    /// System (output) volume in percent, re-applied at startup and when
    /// Bluetooth headphones connect. None until set in Settings > Volume, so
    /// bman leaves the system's volume alone unless asked.
    pub system_volume: Option<u8>,
}

/// The original blue: 42, 111, 219.
const DEFAULT_ACCENT: u32 = 0xFF2A6FDB;

pub fn load_conf() -> Config {
    let mut conf = Config {
        volume: 1.0,
        driver: "whisplay".to_string(),
        battery: crate::battery::DEFAULT_BATTERY_DRIVER.to_string(),
        use_system_audio: false,
        accent: DEFAULT_ACCENT,
        output: Output::Wired,
        screen_timeout: 60,
        brightness: 100,
        wifi: Vec::new(),
        update_check: true,
        system_volume: None,
    };

    let Ok(config) = fs::read_to_string("config.txt") else {
        return conf;
    };

    let mut driver_set = false;
    let mut legacy_window = false;

    for line in config.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        match key.trim() {
            "volume" => {
                conf.volume = value.trim().parse::<f32>().unwrap_or(1.0);
            }
            "driver" => {
                conf.driver = value.trim().to_lowercase();
                driver_set = true;
            }
            "battery" => {
                conf.battery = value.trim().to_lowercase();
            }
            // Older config files used render_window=true instead of a driver.
            "render_window" => {
                legacy_window = value.trim().parse::<bool>().unwrap_or(false);
            }
            "use_system_audio" => {
                conf.use_system_audio = value.trim().parse::<bool>().unwrap_or(false);
            }
            "accent" => {
                conf.accent = parse_rgb(value).unwrap_or(DEFAULT_ACCENT);
            }
            "output" => {
                conf.output = Output::parse(value).unwrap_or(Output::Wired);
            }
            "brightness" => {
                conf.brightness = value.trim().parse::<u8>().unwrap_or(100).clamp(10, 100);
            }
            "wifi" => {
                // Split at the first comma: SSIDs with commas aren't supported,
                // passwords with commas are.
                let (ssid, password) = value.split_once(',').unwrap_or((value, ""));
                let ssid = ssid.trim();
                if !ssid.is_empty() {
                    conf.wifi.push((ssid.to_string(), password.trim().to_string()));
                }
            }
            "system_volume" => {
                conf.system_volume = value.trim().parse::<u8>().ok().map(|v| v.min(100));
            }
            "update_check" => {
                conf.update_check = value.trim().parse::<bool>().unwrap_or(true);
            }
            "screen_timeout" => {
                conf.screen_timeout = value.trim().parse::<u32>().unwrap_or(60).clamp(30, 300);
            }
            _ => {}
        }
    }

    if !driver_set && legacy_window {
        conf.driver = "window".to_string();
    }

    conf
}

/// Parse "R,G,B" (each 0-255). Returns None if it isn't valid.
fn parse_rgb(value: &str) -> Option<u32> {
    let mut parts = value.split(',').map(|p| p.trim().parse::<u8>());

    let r = parts.next()?.ok()?;
    let g = parts.next()?.ok()?;
    let b = parts.next()?.ok()?;
    if parts.next().is_some() {
        return None; // too many values
    }

    Some(0xFF000000 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
}

pub fn save_conf(conf: &Config) {
    let mut contents = format!(
        "driver={}\nbattery={}\nvolume={}\nuse_system_audio={}\naccent={},{},{}\noutput={}\nscreen_timeout={}\nbrightness={}\nupdate_check={}\n",
        conf.driver,
        conf.battery,
        conf.volume,
        conf.use_system_audio,
        (conf.accent >> 16) & 0xFF,
        (conf.accent >> 8) & 0xFF,
        conf.accent & 0xFF,
        conf.output.label().to_lowercase(),
        conf.screen_timeout,
        conf.brightness,
        conf.update_check,
    );
    if let Some(v) = conf.system_volume {
        contents.push_str(&format!("system_volume={v}\n"));
    }
    for (ssid, password) in &conf.wifi {
        contents.push_str(&format!("wifi={ssid},{password}\n"));
    }

    // Write to a temp file and rename, so pulling the plug mid-write can't
    // leave a half-written config.
    let tmp = "config.txt.tmp";
    let result = fs::write(tmp, contents).and_then(|_| fs::rename(tmp, "config.txt"));
    if let Err(e) = result {
        error!("Failed to save config file: {e}");
    }
}
