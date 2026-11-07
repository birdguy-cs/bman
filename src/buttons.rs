use rppal::gpio::Gpio;
use std::{
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

/// What a press pattern means. The same single button (or mouse click in
/// the debug window) produces all of them:
///   1 press   -> Select (enter / play / pause)
///   2 presses -> Down
///   3 presses -> Up (or loop mode on the now-playing screen)
///   4 presses -> Back
///   5 presses -> Action (add to playlist/queue, or remove, depending on the screen)
///   hold      -> Help (explains the controls)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Select,
    Down,
    Up,
    Back,
    Action,
    Help,
}

/// BCM pin number of the physical button. 17 is the Whisplay HAT's button
/// (physical pin 11).
pub const BUTTON_PIN: u8 = 17;

/// Which level means "pressed". None works it out at startup (see
/// detect_wiring), so don't hold the button while the program starts.
/// Some(true): pressed reads high. Some(false): pressed reads low, using the
/// Pi's internal pull-up.
const BUTTON_ACTIVE_LEVEL: Option<bool> = None;

/// How long to wait after the last release before deciding how many presses there were.
const PRESS_GAP: Duration = Duration::from_millis(350);
/// Holding the button this long counts as a long press.
const LONG_PRESS: Duration = Duration::from_millis(800);
const DEBOUNCE: Duration = Duration::from_millis(30);

/// Turns the raw button state (down or up) into `Button`s: counts quick
/// presses, and detects a long press while the button is still held.
pub struct PressCounter {
    count: u32,
    last_release: Instant,
    down_since: Option<Instant>,
    long_fired: bool,
}

impl PressCounter {
    pub fn new() -> Self {
        Self {
            count: 0,
            last_release: Instant::now(),
            down_since: None,
            long_fired: false,
        }
    }

    /// Tell the counter whether the button is down right now. Call it every
    /// poll with the (debounced) state; it works out presses and releases.
    pub fn set_down(&mut self, down: bool) {
        match (down, self.down_since) {
            (true, None) => {
                self.down_since = Some(Instant::now());
                self.long_fired = false;
            }
            (false, Some(_)) => {
                self.down_since = None;
                // A long press was already reported, don't count it as a tap.
                if !self.long_fired {
                    self.count += 1;
                    self.last_release = Instant::now();
                }
                self.long_fired = false;
            }
            _ => {}
        }
    }

    /// Call often. Returns a Button once it is decided: Help as soon as the
    /// long press threshold passes, otherwise the press count after the gap.
    pub fn poll(&mut self) -> Option<Button> {
        if let Some(since) = self.down_since {
            if !self.long_fired && since.elapsed() >= LONG_PRESS {
                self.long_fired = true;
                self.count = 0; // taps before a hold are discarded
                return Some(Button::Help);
            }
            return None; // still held, wait for release
        }

        if self.count == 0 || self.last_release.elapsed() < PRESS_GAP {
            return None;
        }

        let button = match self.count {
            1 => Button::Select,
            2 => Button::Down,
            3 => Button::Up,
            4 => Button::Back,
            _ => Button::Action,
        };

        self.count = 0;
        Some(button)
    }
}

/// How the button is wired.
struct Wiring {
    /// True if pressing the button makes the pin read high.
    active_high: bool,
    /// True if the Pi's internal pull-up is needed.
    pull_up: bool,
}

/// Work out how the button is wired by trying the Pi's pull-up, then its
/// pull-down. If the pin reads the same both times, something on the board
/// is holding it at that level, and the button flips it. If it follows the
/// pull, nothing is holding it: a plain button to ground that needs the pull-up.
fn detect_wiring(gpio: &Gpio, pin: u8) -> Result<Wiring, rppal::gpio::Error> {
    let pulled_up = gpio.get(pin)?.into_input_pullup();
    thread::sleep(Duration::from_millis(20));
    let with_pull_up = pulled_up.is_high();
    drop(pulled_up);

    let pulled_down = gpio.get(pin)?.into_input_pulldown();
    thread::sleep(Duration::from_millis(20));
    let with_pull_down = pulled_down.is_high();
    drop(pulled_down);

    Ok(if with_pull_up == with_pull_down {
        Wiring {
            active_high: !with_pull_up, // idle level is held, pressed is the opposite
            pull_up: false,
        }
    } else {
        Wiring {
            active_high: false,
            pull_up: true,
        }
    })
}

/// Watch the GPIO button on its own thread (a quick tap would be missed by
/// the 10 FPS render loop) and send decoded buttons over a channel.
pub fn spawn_gpio_buttons(pin: u8) -> Result<Receiver<Button>, Box<dyn std::error::Error>> {
    let gpio = Gpio::new()?;

    let wiring = match BUTTON_ACTIVE_LEVEL {
        Some(active_high) => Wiring {
            active_high,
            pull_up: !active_high,
        },
        None => detect_wiring(&gpio, pin)?,
    };
    info!(
        "Button on GPIO {pin}: pressed reads {}",
        if wiring.active_high { "high" } else { "low" }
    );

    let input = if wiring.pull_up {
        gpio.get(pin)?.into_input_pullup()
    } else {
        gpio.get(pin)?.into_input()
    };
    let active_high = wiring.active_high;
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let mut counter = PressCounter::new();
        let mut down = false;
        let mut last_edge = Instant::now();

        loop {
            let raw = input.is_high() == active_high;

            if raw != down && last_edge.elapsed() >= DEBOUNCE {
                down = raw;
                last_edge = Instant::now();
                counter.set_down(down);
            }

            if let Some(button) = counter.poll() {
                if tx.send(button).is_err() {
                    break; // main loop is gone
                }
            }

            thread::sleep(Duration::from_millis(2));
        }
    });

    Ok(rx)
}
