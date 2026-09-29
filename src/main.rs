use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Length, Limits, Subscription};
use cosmic::widget::{button, column, container, icon, row, slider, space, text};
use cosmic::{app, Action, Application, Element, Task};
use std::path::{Path, PathBuf};
use std::process::Command as SysCommand;
use std::time::Duration;

pub fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt::init();

    // Launch as a COSMIC Panel Applet
    cosmic::applet::run::<BatteryApplet>(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum PowerProfile {
    PowerSaver,
    Balanced,
    Performance,
}

/// Mirrors /sys/class/power_supply/BAT*/status.
/// Note: with a charge limit active, a plugged-in laptop reports
/// "Not charging" (at the limit) or "Full", so "plugged in" is
/// anything other than Discharging.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BatteryState {
    Charging,
    Full,
    NotCharging,
    Discharging,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    TogglePopup,
    PopupClosed(Id),
    SetDisplayBrightness(u32),
    SetKeyboardBrightness(u32),
    SetPowerProfile(PowerProfile),
    SetChargeLimit(u32),
    ApplyChargeLimit,
}

pub struct BatteryApplet {
    core: app::Core,
    popup: Option<Id>,
    battery_percentage: u32,
    battery_state: BatteryState,
    display_brightness: u32,
    keyboard_brightness: u32,
    power_profile: PowerProfile,
    charge_limit: u32,
    /// True between the first slider movement and its release
    charge_limit_dragging: bool,
    /// Current charge/discharge rate in watts
    power_watts: Option<f64>,
    /// Minutes until charged (to limit/full) or until empty
    time_remaining: Option<u32>,
}

impl BatteryApplet {
    // ---------- queries ----------

    fn query_battery_status() -> (u32, BatteryState) {
        let Some(dir) = Self::battery_dir() else {
            return (100, BatteryState::Discharging);
        };

        let percentage = Self::read_sysfs(&dir, "capacity")
            .map(|v| v as u32)
            .unwrap_or(100);

        let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
        let state = match status.trim().to_ascii_lowercase().as_str() {
            "charging" => BatteryState::Charging,
            "full" => BatteryState::Full,
            "not charging" => BatteryState::NotCharging,
            _ => BatteryState::Discharging,
        };

        (percentage, state)
    }

    /// Current power profile as reported by power-profiles-daemon.
    fn query_power_profile() -> Option<PowerProfile> {
        let out = SysCommand::new("powerprofilesctl").arg("get").output().ok()?;
        let s = String::from_utf8(out.stdout).ok()?;
        match s.trim() {
            "power-saver" => Some(PowerProfile::PowerSaver),
            "balanced" => Some(PowerProfile::Balanced),
            "performance" => Some(PowerProfile::Performance),
            _ => None,
        }
    }

    /// Charge limit straight from the kernel (asus-wmi exposes this file).
    fn query_charge_limit() -> Option<u32> {
        let dir = Self::battery_dir()?;
        Self::read_sysfs(&dir, "charge_control_end_threshold").map(|v| v as u32)
    }

    fn query_display_brightness() -> u32 {
        SysCommand::new("brightnessctl")
            .arg("i")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|s| {
                s.lines()
                    .find(|line| line.contains("Current brightness:"))
                    .and_then(|line| line.split('(').nth(1))
                    .and_then(|s| s.split('%').next())
                    .and_then(|val| val.parse::<u32>().ok())
            })
            .unwrap_or(50)
    }

    fn query_keyboard_brightness() -> u32 {
        let val = SysCommand::new("brightnessctl")
            .args(["--device", "asus::kbd_backlight", "get"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);

        match val {
            0 => 0,
            1 => 33,
            2 => 67,
            _ => 100,
        }
    }

    /// First battery (BAT0, BAT1, ...) in name order, so the choice is stable.
    fn battery_dir() -> Option<PathBuf> {
        let mut batteries: Vec<PathBuf> = std::fs::read_dir("/sys/class/power_supply")
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("BAT"))
            .map(|e| e.path())
            .collect();
        batteries.sort();
        batteries.into_iter().next()
    }

    fn read_sysfs(dir: &Path, name: &str) -> Option<u64> {
        std::fs::read_to_string(dir.join(name))
            .ok()?
            .trim()
            .parse::<i64>()
            .ok()
            .map(|v| v.unsigned_abs())
    }

    /// Current battery power draw/charge rate in watts.
    fn query_power_watts() -> Option<f64> {
        let dir = Self::battery_dir()?;
        if let Some(p) = Self::read_sysfs(&dir, "power_now") {
            return Some(p as f64 / 1_000_000.0);
        }
        let i = Self::read_sysfs(&dir, "current_now")?;
        let v = Self::read_sysfs(&dir, "voltage_now")?;
        Some(i as f64 * v as f64 / 1e12)
    }

    /// Minutes until the battery reaches `target_pct` (charging), or until it
    /// is empty when `target_pct` is None (discharging). Uses energy/power
    /// (uWh / uW) or charge/current (uAh / uA); either ratio is in hours.
    fn query_minutes(target_pct: Option<u32>) -> Option<u32> {
        let dir = Self::battery_dir()?;
        let read3 = |a: &str, b: &str, c: &str| {
            Some((
                Self::read_sysfs(&dir, a)?,
                Self::read_sysfs(&dir, b)?,
                Self::read_sysfs(&dir, c)?,
            ))
        };
        let (now, full, rate) = read3("energy_now", "energy_full", "power_now")
            .or_else(|| read3("charge_now", "charge_full", "current_now"))?;

        if rate == 0 {
            return None;
        }
        let delta = match target_pct {
            Some(pct) => {
                let goal = full * pct as u64 / 100;
                if now >= goal {
                    return None;
                }
                goal - now
            }
            None => now,
        };
        let minutes = (delta * 60 / rate).max(1);
        // Ignore absurd values from a near-zero reading
        if minutes > 24 * 60 {
            None
        } else {
            Some(minutes as u32)
        }
    }

    /// Refresh rate and time estimate. When charging, the target is the
    /// charge limit, or 100% when the limit is off or a top up is active.
    fn refresh_power_stats(&mut self) {
        self.power_watts = Self::query_power_watts();
        self.time_remaining = match self.battery_state {
            BatteryState::Charging => {
                let target = self.charge_limit.min(100);
                Self::query_minutes(Some(target))
            }
            BatteryState::Discharging => Self::query_minutes(None),
            _ => None,
        };
    }

    fn format_minutes(m: u32) -> String {
        if m >= 60 {
            format!("{}h {}m", m / 60, m % 60)
        } else {
            format!("{}m", m)
        }
    }

    fn status_label(&self) -> &'static str {
        match self.battery_state {
            BatteryState::Charging => "Charging",
            BatteryState::Discharging => "Discharging",
            BatteryState::Full => "Fully Charged",
            BatteryState::NotCharging => {
                if self.charge_limit < 100 {
                    "Charge Limit Reached"
                } else {
                    "Not Charging"
                }
            }
        }
    }

    /// Re-read everything that can be changed outside the applet.
    fn refresh_settings(&mut self) {
        if let Some(profile) = Self::query_power_profile() {
            self.power_profile = profile;
        }
        if !self.charge_limit_dragging {
            if let Some(limit) = Self::query_charge_limit() {
                self.charge_limit = limit;
            }
        }
    }

    // ---------- actions ----------

    fn apply_display_brightness(val: u32) {
        let _ = SysCommand::new("brightnessctl")
            .arg("set")
            .arg(format!("{}%", val.max(5)))
            .status();
    }

    fn apply_keyboard_brightness(val: u32) {
        let step = match val {
            0..=15 => 0,
            16..=50 => 1,
            51..=84 => 2,
            _ => 3,
        };
        let _ = SysCommand::new("brightnessctl")
            .args(["--device", "asus::kbd_backlight", "set", &step.to_string()])
            .status();
    }

    fn snap_kbd_percentage(val: u32) -> u32 {
        match val {
            0..=15 => 0,
            16..=50 => 33,
            51..=84 => 67,
            _ => 100,
        }
    }

    fn apply_power_profile(profile: &PowerProfile) {
        let mode = match profile {
            PowerProfile::PowerSaver => "power-saver",
            PowerProfile::Balanced => "balanced",
            PowerProfile::Performance => "performance",
        };
        let _ = SysCommand::new("powerprofilesctl")
            .args(["set", mode])
            .status();
    }

    fn apply_charge_limit(limit: u32) {
        let _ = SysCommand::new("asusctl")
            .args(["battery", "limit", &limit.to_string()])
            .status();
    }

    // ---------- icons ----------

    /// Panel icon, using the same icon set as the stock COSMIC battery applet.
    /// Levels: 0, 5, 10, 20, 35, 50, 65, 80, 90, 100.
    /// "limited" variants (levels 0-80 only) are used while a charge limit is active.
    /// "charging" variants are used whenever the laptop is plugged in.
    fn panel_icon_name(pct: u32, state: BatteryState, limited: bool) -> &'static str {
        macro_rules! bat {
            ($lvl:literal, $limited:expr, $charging:expr) => {
                match ($limited, $charging) {
                    (false, false) => concat!("cosmic-applet-battery-level-", $lvl, "-symbolic"),
                    (false, true) => {
                        concat!("cosmic-applet-battery-level-", $lvl, "-charging-symbolic")
                    }
                    (true, false) => {
                        concat!("cosmic-applet-battery-level-", $lvl, "-limited-symbolic")
                    }
                    (true, true) => concat!(
                        "cosmic-applet-battery-level-",
                        $lvl,
                        "-limited-charging-symbolic"
                    ),
                }
            };
        }

        // Any plugged-in state (charging, at the limit, or full) uses the
        // "charging" icons, so plugged and unplugged look different.
        let charging = state != BatteryState::Discharging;

        match pct {
            96..=100 => {
                if charging {
                    "cosmic-applet-battery-level-100-charging-symbolic"
                } else {
                    "cosmic-applet-battery-level-100-symbolic"
                }
            }
            81..=95 => {
                if charging {
                    "cosmic-applet-battery-level-90-charging-symbolic"
                } else {
                    "cosmic-applet-battery-level-90-symbolic"
                }
            }
            66..=80 => bat!("80", limited, charging),
            51..=65 => bat!("65", limited, charging),
            36..=50 => bat!("50", limited, charging),
            21..=35 => bat!("35", limited, charging),
            15..=20 => bat!("20", limited, charging),
            10..=14 => bat!("10", limited, charging),
            6..=9 => bat!("5", limited, charging),
            _ => bat!("0", limited, charging),
        }
    }

    /// Screen brightness icon (COSMIC applet set)
    fn display_icon_name(pct: u32) -> &'static str {
        match pct {
            0 => "cosmic-applet-battery-display-brightness-off-symbolic",
            1..=33 => "cosmic-applet-battery-display-brightness-low-symbolic",
            34..=66 => "cosmic-applet-battery-display-brightness-medium-symbolic",
            _ => "cosmic-applet-battery-display-brightness-high-symbolic",
        }
    }

    // ---------- popup UI ----------

    /// One line: label on the left, value on the right
    fn info_row<'a>(label: &'static str, value: String) -> Element<'a, Message> {
        row![text::heading(label), space::horizontal(), text(value)]
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into()
    }

    /// One line: [icon] [slider ------] [percent]
    fn slider_row<'a>(
        icon_name: &'static str,
        slider: Element<'a, Message>,
        value: u32,
    ) -> Element<'a, Message> {
        let label_icon = icon::from_name(icon_name).size(20).symbolic(true).icon();

        // Fixed-width, right-aligned percentage so all sliders end at the same x
        let percent = container(text(format!("{}%", value)))
            .width(Length::Fixed(44.0))
            .align_x(Horizontal::Right);

        row![label_icon, slider, percent]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
    }

    /// Renders the popup dropdown interface
    fn view_popup(&self) -> Element<'_, Message> {
        // Status, rate and time lines (label left, value right)
        let (rate_label, time_label) = if self.battery_state == BatteryState::Discharging {
            ("Discharge Rate", "Time Until Empty")
        } else {
            ("Charge Rate", "Time Until Charged")
        };
        let rate_value = match self.power_watts {
            Some(w) => format!("{:.1} W", w),
            None => "—".to_string(),
        };
        let time_value = match self.time_remaining {
            Some(m) => Self::format_minutes(m),
            None => "—".to_string(),
        };
        let stats_section = column![
            Self::info_row(
                "Status",
                format!("{}% ({})", self.battery_percentage, self.status_label()),
            ),
            Self::info_row(rate_label, rate_value),
            Self::info_row(time_label, time_value),
        ]
        .spacing(4);

        // Power profiles (selected one is highlighted)
        let saver_btn = if self.power_profile == PowerProfile::PowerSaver {
            button::suggested("Battery")
        } else {
            button::standard("Battery")
        }
        .on_press(Message::SetPowerProfile(PowerProfile::PowerSaver));

        let balanced_btn = if self.power_profile == PowerProfile::Balanced {
            button::suggested("Balanced")
        } else {
            button::standard("Balanced")
        }
        .on_press(Message::SetPowerProfile(PowerProfile::Balanced));

        let perf_btn = if self.power_profile == PowerProfile::Performance {
            button::suggested("Performance")
        } else {
            button::standard("Performance")
        }
        .on_press(Message::SetPowerProfile(PowerProfile::Performance));

        let profile_buttons = row![saver_btn, balanced_btn, perf_btn]
            .spacing(8)
            .align_y(Alignment::Center);

        let profile_section = container(profile_buttons)
            .width(Length::Fill)
            .align_x(Horizontal::Center);

        let disp_row = Self::slider_row(
            Self::display_icon_name(self.display_brightness),
            slider(5..=100, self.display_brightness, Message::SetDisplayBrightness)
                .step(5u32)
                .into(),
            self.display_brightness,
        );

        let kbd_row = Self::slider_row(
            "input-keyboard-symbolic",
            slider(0..=100, self.keyboard_brightness, Message::SetKeyboardBrightness).into(),
            self.keyboard_brightness,
        );

        // Charge limit: 50-100% in 5% steps, applied when the slider is released
        let limit_icon = if self.charge_limit >= 100 {
            "security-high-symbolic"
        } else {
            "security-medium-symbolic"
        };
        let limit_row = Self::slider_row(
            limit_icon,
            slider(50..=100, self.charge_limit, Message::SetChargeLimit)
                .step(5u32)
                .on_release(Message::ApplyChargeLimit)
                .into(),
            self.charge_limit,
        );

        let content = column![stats_section, profile_section, disp_row, kbd_row, limit_row]
            .spacing(16)
            .padding(16);

        container(content)
            .width(Length::Fixed(360.0))
            .align_x(Horizontal::Left)
            .align_y(Vertical::Top)
            .into()
    }
}

impl Application for BatteryApplet {
    type Executor = cosmic::iced::executor::Default;
    type Message = Message;
    type Flags = ();

    const APP_ID: &'static str = "cosmic-applet-superpower";

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn init(core: app::Core, _flags: ()) -> (Self, Task<Action<Self::Message>>) {
        let (battery_percentage, battery_state) = Self::query_battery_status();
        let display_brightness = Self::query_display_brightness();
        let keyboard_brightness = Self::query_keyboard_brightness();

        let mut applet = BatteryApplet {
            core,
            popup: None,
            battery_percentage,
            battery_state,
            display_brightness,
            keyboard_brightness,
            power_profile: Self::query_power_profile().unwrap_or(PowerProfile::Balanced),
            charge_limit: Self::query_charge_limit().unwrap_or(100),
            charge_limit_dragging: false,
            power_watts: None,
            time_remaining: None,
        };
        applet.refresh_power_stats();

        (applet, Task::none())
    }

    fn update(&mut self, message: Self::Message) -> Task<Action<Self::Message>> {
        match message {
            Message::Tick => {
                let (pct, state) = Self::query_battery_status();
                self.battery_percentage = pct;
                self.battery_state = state;

                self.refresh_power_stats();

                // Pick up changes made elsewhere while the popup is visible
                if self.popup.is_some() {
                    self.refresh_settings();
                }
            }
            Message::TogglePopup => {
                return if let Some(id) = self.popup.take() {
                    destroy_popup(id)
                } else {
                    // Refresh everything so the popup opens with current values
                    let (pct, state) = Self::query_battery_status();
                    self.battery_percentage = pct;
                    self.battery_state = state;
                    self.display_brightness = Self::query_display_brightness();
                    self.keyboard_brightness = Self::query_keyboard_brightness();
                    self.charge_limit_dragging = false;
                    self.refresh_settings();
                    self.refresh_power_stats();

                    let new_id = Id::unique();
                    self.popup = Some(new_id);

                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE
                        .min_width(300.0)
                        .max_width(500.0)
                        .min_height(200.0)
                        .max_height(800.0);

                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    self.charge_limit_dragging = false;
                }
            }
            Message::SetDisplayBrightness(val) => {
                self.display_brightness = val;
                Self::apply_display_brightness(val);
            }
            Message::SetKeyboardBrightness(val) => {
                let snapped = Self::snap_kbd_percentage(val);
                if snapped != self.keyboard_brightness {
                    self.keyboard_brightness = snapped;
                    Self::apply_keyboard_brightness(snapped);
                }
            }
            Message::SetPowerProfile(profile) => {
                self.power_profile = profile.clone();
                Self::apply_power_profile(&profile);
            }
            Message::SetChargeLimit(limit) => {
                // Only update the UI while dragging; applied on release
                self.charge_limit_dragging = true;
                self.charge_limit = limit;
            }
            Message::ApplyChargeLimit => {
                self.charge_limit_dragging = false;
                Self::apply_charge_limit(self.charge_limit);
                self.refresh_power_stats();
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        cosmic::iced::time::every(Duration::from_secs(5)).map(|_| Message::Tick)
    }

    /// Called when the compositor closes the popup (click outside, Escape)
    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    /// The panel button
    fn view(&self) -> Element<'_, Self::Message> {
        self.core
            .applet
            .icon_button(Self::panel_icon_name(
                self.battery_percentage,
                self.battery_state,
                self.charge_limit < 100,
            ))
            .on_press_down(Message::TogglePopup)
            .into()
    }

    /// The popup surface, rendered when a popup window exists
    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        self.core
            .applet
            .popup_container(self.view_popup())
            .limits(
                Limits::NONE
                    .min_width(1.0)
                    .min_height(1.0)
                    .max_width(500.0)
                    .max_height(1000.0),
            )
            .into()
    }
}
