use crate::buttons::{Button, PressCounter};
use crate::render::{Display, Framebuffer};
use crate::terminal::TerminalDisplay;
use crate::whisplay::WhisplayDisplay;

use minifb::{MouseButton, Window, WindowOptions};
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

// ---------------------------------------------------------------------------
// Window (testing on a PC)
// ---------------------------------------------------------------------------

// Left mouse clicks act as the button: click 1-5 times quickly, or hold
// the mouse button for help, same as the real button.
pub struct WindowDisplay {
    window: Window,
    width: u32,
    height: u32,
    counter: PressCounter,
}

impl WindowDisplay {
    pub fn new(width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let window = Window::new(
            "BMan debug",
            width as usize,
            height as usize,
            WindowOptions {
                resize: false,
                ..WindowOptions::default()
            },
        )?;

        Ok(Self {
            window,
            width,
            height,
            counter: PressCounter::new(),
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

        self.counter.poll().into_iter().collect()
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
