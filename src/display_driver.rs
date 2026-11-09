use crate::buttons::{key_button, Button, PressCounter, Typed};
use crate::render::{Display, Framebuffer, Led};
use crate::terminal::TerminalDisplay;
use crate::whisplay::WhisplayDisplay;

use minifb::{InputCallback, Key, KeyRepeat, MouseButton, Window, WindowOptions};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Names accepted by `driver=` in config.txt.
pub const DRIVERS: &[&str] = &["whisplay", "window", "terminal"];

/// Build the display for a driver name. To add a new display, write a struct
/// that implements `Display` (see whisplay.rs), add its name to DRIVERS, and
/// add a match arm.
pub fn create_display(driver: &str) -> Result<Box<dyn Display>, Box<dyn std::error::Error>> {
    match driver.trim().to_lowercase().as_str() {
        "whisplay" => Ok(Box::new(WhisplayDisplay::new()?)),
        "window" => Ok(Box::new(WindowDisplay::new(240, 280)?)),
        "terminal" => Ok(Box::new(TerminalDisplay::new()?)),
        other => Err(format!(
            "Unknown display driver '{other}'. Available drivers: {}",
            DRIVERS.join(", ")
        )
        .into()),
    }
}

/// `driver` plus a copy in the terminal (`bman --terminal`). If `driver` is
/// the terminal itself, or can't start here (no Whisplay attached, no desktop
/// for the window), the terminal is used on its own.
pub fn create_display_with_terminal(
    driver: &str,
) -> Result<Box<dyn Display>, Box<dyn std::error::Error>> {
    if driver.trim().eq_ignore_ascii_case("terminal") {
        return create_display("terminal");
    }
    // Before the terminal takes over the screen, so a failure is readable.
    let primary = match create_display(driver) {
        Ok(display) => Some(display),
        Err(e) => {
            warn!("'{driver}' display unavailable ({e}); using the terminal only");
            None
        }
    };
    let terminal = create_display("terminal")?;
    Ok(match primary {
        Some(primary) => Box::new(Mirror { primary, terminal }),
        None => terminal,
    })
}

// ---------------------------------------------------------------------------
// Mirror: the real screen plus the terminal
// ---------------------------------------------------------------------------

/// Shows every frame on both displays and takes buttons from both. The
/// primary display decides the backlight and GPIO; closing either quits.
struct Mirror {
    primary: Box<dyn Display>,
    terminal: Box<dyn Display>,
}

impl Display for Mirror {
    fn size(&self) -> (u32, u32) {
        self.primary.size()
    }

    fn present(&mut self, framebuffer: &Framebuffer) -> Result<(), Box<dyn std::error::Error>> {
        self.terminal.present(framebuffer)?;
        self.primary.present(framebuffer)
    }

    fn poll_buttons(&mut self) -> Vec<Button> {
        let mut buttons = self.primary.poll_buttons();
        buttons.extend(self.terminal.poll_buttons());
        buttons
    }

    fn set_typing(&mut self, typing: bool) {
        self.primary.set_typing(typing);
        self.terminal.set_typing(typing);
    }

    fn poll_typed(&mut self) -> Vec<Typed> {
        let mut typed = self.primary.poll_typed();
        typed.extend(self.terminal.poll_typed());
        typed
    }

    fn is_open(&self) -> bool {
        self.primary.is_open() && self.terminal.is_open()
    }

    fn wants_gpio_buttons(&self) -> bool {
        self.primary.wants_gpio_buttons()
    }

    fn set_brightness(&mut self, percent: u8) -> u8 {
        self.terminal.set_brightness(percent);
        self.primary.set_brightness(percent)
    }

    fn set_idle(&mut self, idle: bool) {
        self.primary.set_idle(idle);
        self.terminal.set_idle(idle);
    }

    fn set_led(&mut self, led: Led) {
        self.primary.set_led(led);
        self.terminal.set_led(led);
    }

    fn frame_interval(&self) -> Duration {
        self.primary.frame_interval().min(self.terminal.frame_interval())
    }
}

// ---------------------------------------------------------------------------
// Window (testing on a PC)
// ---------------------------------------------------------------------------

// Left mouse clicks act as the button: click 1-5 times quickly, or hold
// the mouse button for help, same as the real button. The keyboard works
// too: arrows, Enter, Esc/Backspace and the same letter/digit keys as the
// terminal driver; on the keyboard screen typing goes straight into the text.
pub struct WindowDisplay {
    window: Window,
    width: u32,
    height: u32,
    counter: PressCounter,
    /// Characters from minifb's text callback (follows shift and layout).
    chars: Rc<RefCell<Vec<char>>>,
    typing: bool,
    typed: Vec<Typed>,
}

/// Collects typed characters for `WindowDisplay`.
struct CharSink(Rc<RefCell<Vec<char>>>);

impl InputCallback for CharSink {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(c) = char::from_u32(uni_char).filter(|c| !c.is_control()) {
            self.0.borrow_mut().push(c);
        }
    }
}

impl WindowDisplay {
    pub fn new(width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let mut window = Window::new(
            "BMan debug",
            width as usize,
            height as usize,
            WindowOptions {
                resize: false,
                ..WindowOptions::default()
            },
        )?;
        let chars = Rc::new(RefCell::new(Vec::new()));
        window.set_input_callback(Box::new(CharSink(chars.clone())));

        Ok(Self {
            window,
            width,
            height,
            counter: PressCounter::new(),
            chars,
            typing: false,
            typed: Vec::new(),
        })
    }
}

impl Display for WindowDisplay {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(
        &mut self,
        framebuffer: &Framebuffer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.window.update_with_buffer(
            &framebuffer.pixels,
            framebuffer.width as usize,
            framebuffer.height as usize,
        )?;

        Ok(())
    }

    fn poll_buttons(&mut self) -> Vec<Button> {
        let down = self.window.get_mouse_down(MouseButton::Left);
        self.counter.set_down(down);
        let mut buttons: Vec<Button> = self.counter.poll().into_iter().collect();

        // Keys that aren't text (held keys repeat).
        for key in self.window.get_keys_pressed(KeyRepeat::Yes) {
            match key {
                Key::Up => buttons.push(Button::Up),
                Key::Down => buttons.push(Button::Down),
                Key::Escape => buttons.push(Button::Back),
                Key::Enter | Key::NumPadEnter if self.typing => self.typed.push(Typed::Enter),
                Key::Backspace if self.typing => self.typed.push(Typed::Erase),
                Key::Right | Key::Enter | Key::NumPadEnter if !self.typing => {
                    buttons.push(Button::Select)
                }
                Key::Left | Key::Backspace if !self.typing => buttons.push(Button::Back),
                _ => {}
            }
        }

        let chars = std::mem::take(&mut *self.chars.borrow_mut());
        if self.typing {
            self.typed.extend(chars.into_iter().map(Typed::Char));
        } else {
            buttons.extend(chars.into_iter().filter_map(key_button));
        }
        buttons
    }

    fn set_typing(&mut self, typing: bool) {
        self.typing = typing;
    }

    fn poll_typed(&mut self) -> Vec<Typed> {
        std::mem::take(&mut self.typed)
    }

    fn is_open(&self) -> bool {
        self.window.is_open()
    }

    fn wants_gpio_buttons(&self) -> bool {
        false
    }

    // Faster loop so quick mouse clicks aren't missed.
    fn frame_interval(&self) -> Duration {
        Duration::from_millis(16)
    }
}
