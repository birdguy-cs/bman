//! Driver for the PiSugar Whisplay HAT's screen: a 1.69" ST7789 LCD, 240x280.
//!
//! This talks to the panel directly over SPI0, so it works without the
//! Whisplay Python driver or its daemon. Don't run `whisplay-daemon` at the
//! same time (it owns the same SPI/GPIO pins). If it's installed:
//!   sudo systemctl disable --now whisplay-daemon
//!
//! Pins (BCM numbers, from PiSugar's Whisplay pin table):
//!   SPI0 MOSI  GPIO 10 (physical 19)
//!   SPI0 SCLK  GPIO 11 (physical 23)
//!   SPI0 CE0   GPIO 8  (physical 24)
//!   DC         GPIO 27 (physical 13)
//!   RESET      GPIO 4  (physical 7)
//!   Backlight  GPIO 22 (physical 15, "LCDCTRL")
//!   Button     GPIO 17 (physical 11), read in buttons.rs
//!   RGB LED    red GPIO 25 (physical 22), green GPIO 24 (physical 18),
//!              blue GPIO 23 (physical 16); all active low
//!
//! Needs SPI enabled (`dtparam=spi=on`, which the Whisplay installer does).
//!
//! The Whisplay's I2S audio chip uses GPIO 18-21, so it can't share those pins
//! with PWM audio. See audio.rs for the pins used for stereo output instead.

use crate::render::{Display, Framebuffer, Led};
use rppal::gpio::{Gpio, OutputPin};
use rppal::spi::{Bus, Mode, SlaveSelect, Spi};
use std::{thread, time::Duration};

const PIN_DC: u8 = 27;
const PIN_RESET: u8 = 4;
const PIN_BACKLIGHT: u8 = 22;
const PIN_LED_RED: u8 = 25;
const PIN_LED_GREEN: u8 = 24;
const PIN_LED_BLUE: u8 = 23;

/// The RGB LED lights when its pin is low (PiSugar's driver writes
/// `100 - value` as the duty cycle).
const LED_ACTIVE_LOW: bool = true;

/// The backlight is switched by the pin going low. If the screen stays dark
/// after starting (or is on when it should be off), flip this.
const BACKLIGHT_ACTIVE_LOW: bool = true;

/// Backlight dimming uses software PWM on the backlight pin (GPIO 22 isn't a
/// hardware PWM pin). Higher is less visible but takes more CPU on the Pi.
const BACKLIGHT_PWM_HZ: f64 = 1000.0;

/// The backlight is never dimmed below this percentage, because software PWM
/// flickers at low duty cycles. Anything dimmer is done by scaling the
/// picture's colours instead. If you see flicker, raise it; 100 turns
/// backlight PWM off completely, so all dimming is done with colour.
const BACKLIGHT_MIN_PCT: u8 = 50;

/// SPI clock. The Pi picks the nearest speed at or below this. If the picture
/// has noise or flickering pixels, try 25_000_000.
const SPI_HZ: u32 = 50_000_000;

/// spidev only accepts about 4 KiB per write by default.
const SPI_CHUNK: usize = 4096;

const WIDTH: u32 = 240;
const HEIGHT: u32 = 280;

/// The panel's 280 rows sit 20 rows down in the ST7789's 320-row memory.
const ROW_OFFSET: u16 = 20;

/// Screen orientation/colour order (register 0x36). 0x00 is upright RGB.
/// If the picture is mirrored or upside down try 0x40 (mirror), 0x80 (flip)
/// or 0xC0 (rotate 180).
const MADCTL: u8 = 0x00;

pub struct WhisplayDisplay {
    spi: Spi,
    dc: OutputPin,
    reset: OutputPin,
    backlight: OutputPin,
    /// Red, green, blue.
    led: [OutputPin; 3],
    /// How bright the backlight itself runs, BACKLIGHT_MIN_PCT-100.
    backlight_pct: u8,
    /// Whether the backlight is meant to be on (false while idle).
    lit: bool,
    /// The frame being built, as RGB565 bytes.
    buf: Vec<u8>,
    /// The frame currently on the screen. Frames identical to it aren't sent.
    last: Vec<u8>,
}

impl WhisplayDisplay {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let gpio = Gpio::new()?;

        let dc = gpio.get(PIN_DC)?.into_output_low();
        let reset = gpio.get(PIN_RESET)?.into_output_high();

        // Keep the backlight off until the first picture is ready, and leave
        // it where we put it when the program exits (see Drop).
        let mut backlight = if BACKLIGHT_ACTIVE_LOW {
            gpio.get(PIN_BACKLIGHT)?.into_output_high()
        } else {
            gpio.get(PIN_BACKLIGHT)?.into_output_low()
        };
        backlight.set_reset_on_drop(false);

        // Off to start. Left as it is on exit, so "shutting down" red stays
        // lit until the power goes; main turns it off on a normal quit.
        let led_pin = |pin: u8| -> Result<OutputPin, rppal::gpio::Error> {
            let mut p = if LED_ACTIVE_LOW {
                gpio.get(pin)?.into_output_high()
            } else {
                gpio.get(pin)?.into_output_low()
            };
            p.set_reset_on_drop(false);
            Ok(p)
        };
        let led = [led_pin(PIN_LED_RED)?, led_pin(PIN_LED_GREEN)?, led_pin(PIN_LED_BLUE)?];

        let spi = Spi::new(Bus::Spi0, SlaveSelect::Ss0, SPI_HZ, Mode::Mode0)?;

        let frame_bytes = (WIDTH * HEIGHT * 2) as usize;
        let mut display = Self {
            spi,
            dc,
            reset,
            backlight,
            led,
            backlight_pct: 100,
            lit: false,
            buf: vec![0; frame_bytes],
            last: vec![0; frame_bytes],
        };

        display.reset_panel();
        display.init_panel()?;

        // Start from black so leftover memory contents never show.
        display.set_window()?;
        write_spi(&mut display.spi, &display.last)?;

        display.set_backlight(true);
        Ok(display)
    }

    fn set_backlight(&mut self, on: bool) {
        self.lit = on;
        let _ = self.backlight.clear_pwm();

        // Off and full brightness are plain pin levels, no PWM needed.
        if !on || self.backlight_pct >= 100 {
            if on != BACKLIGHT_ACTIVE_LOW {
                self.backlight.set_high();
            } else {
                self.backlight.set_low();
            }
            return;
        }

        // The pin is high for `duty` of each cycle. For an active-low
        // backlight "on" is the low part, so invert.
        let on_fraction = self.backlight_pct as f64 / 100.0;
        let duty = if BACKLIGHT_ACTIVE_LOW { 1.0 - on_fraction } else { on_fraction };
        if self.backlight.set_pwm_frequency(BACKLIGHT_PWM_HZ, duty).is_err() {
            // PWM unavailable: run at full brightness instead.
            self.backlight_pct = 100;
            if BACKLIGHT_ACTIVE_LOW {
                self.backlight.set_low();
            } else {
                self.backlight.set_high();
            }
        }
    }

    fn reset_panel(&mut self) {
        self.reset.set_high();
        thread::sleep(Duration::from_millis(10));
        self.reset.set_low();
        thread::sleep(Duration::from_millis(10));
        self.reset.set_high();
        thread::sleep(Duration::from_millis(120));
    }

    fn command(&mut self, command: u8) -> Result<(), Box<dyn std::error::Error>> {
        self.dc.set_low();
        write_spi(&mut self.spi, &[command])
    }

    fn command_data(&mut self, command: u8, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        self.command(command)?;
        self.dc.set_high();
        write_spi(&mut self.spi, data)
    }

    /// Standard setup for a 1.69" ST7789V2 panel (the same family as the
    /// Waveshare 1.69" module the Whisplay screen matches).
    fn init_panel(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.command_data(0x36, &[MADCTL])?;
        self.command_data(0x3A, &[0x05])?; // 16 bits per pixel (RGB565)

        self.command_data(0xB2, &[0x0B, 0x0B, 0x00, 0x33, 0x35])?;
        self.command_data(0xB7, &[0x11])?;
        self.command_data(0xBB, &[0x35])?;
        self.command_data(0xC0, &[0x2C])?;
        self.command_data(0xC2, &[0x01])?;
        self.command_data(0xC3, &[0x0D])?;
        self.command_data(0xC4, &[0x20])?;
        self.command_data(0xC6, &[0x13])?;
        self.command_data(0xD0, &[0xA4, 0xA1])?;
        self.command_data(0xD6, &[0xA1])?;

        // Gamma
        self.command_data(
            0xE0,
            &[
                0xF0, 0x06, 0x0B, 0x0A, 0x09, 0x26, 0x29, 0x33, 0x41, 0x18, 0x16, 0x15, 0x29, 0x2D,
            ],
        )?;
        self.command_data(
            0xE1,
            &[
                0xF0, 0x04, 0x08, 0x08, 0x07, 0x03, 0x28, 0x32, 0x40, 0x3B, 0x19, 0x18, 0x2A, 0x2E,
            ],
        )?;
        self.command_data(0xE4, &[0x25, 0x00, 0x00])?;

        self.command(0x21)?; // display inversion on (needed for IPS panels)

        self.command(0x11)?; // sleep out
        thread::sleep(Duration::from_millis(120));
        self.command(0x29)?; // display on
        thread::sleep(Duration::from_millis(20));

        Ok(())
    }

    /// Point the panel's memory at the whole visible area, then start a write.
    fn set_window(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let x1 = (WIDTH - 1) as u16;
        let y0 = ROW_OFFSET;
        let y1 = ROW_OFFSET + (HEIGHT - 1) as u16;

        self.command_data(0x2A, &[0, 0, (x1 >> 8) as u8, x1 as u8])?;
        self.command_data(0x2B, &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8])?;
        self.command(0x2C)?; // memory write
        Ok(())
    }
}

impl Display for WhisplayDisplay {
    fn size(&self) -> (u32, u32) {
        (WIDTH, HEIGHT)
    }

    fn present(&mut self, framebuffer: &Framebuffer) -> Result<(), Box<dyn std::error::Error>> {
        if framebuffer.width != WIDTH || framebuffer.height != HEIGHT {
            return Err(format!(
                "Whisplay screen is {WIDTH}x{HEIGHT}, got a {}x{} frame",
                framebuffer.width, framebuffer.height
            )
            .into());
        }

        // 0xAARRGGBB -> RGB565, high byte first.
        for (out, &pixel) in self.buf.chunks_exact_mut(2).zip(&framebuffer.pixels) {
            let r = ((pixel >> 16) & 0xFF) as u16;
            let g = ((pixel >> 8) & 0xFF) as u16;
            let b = (pixel & 0xFF) as u16;
            let rgb565 = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3);

            out[0] = (rgb565 >> 8) as u8;
            out[1] = rgb565 as u8;
        }

        // Nothing changed: skip the ~134 KB transfer.
        if self.buf == self.last {
            return Ok(());
        }

        self.set_window()?;
        self.dc.set_high();
        write_spi(&mut self.spi, &self.buf)?;

        std::mem::swap(&mut self.buf, &mut self.last);
        Ok(())
    }

    fn set_brightness(&mut self, percent: u8) -> u8 {
        // 0 happens at the end of the fade to sleep (all done in colour).
        let percent = percent.min(100);

        // Dim the backlight as far as is flicker-free, and have the picture
        // make up the rest (e.g. 25% = backlight 50% x colours 50%).
        let backlight = percent.max(BACKLIGHT_MIN_PCT.clamp(10, 100));
        self.backlight_pct = backlight;
        if self.lit {
            self.set_backlight(true);
        }

        // If PWM failed, the backlight is back at 100%, so do it all in colour.
        let backlight = self.backlight_pct as u32;
        (percent as u32 * 100 / backlight).min(100) as u8
    }

    /// Screen off while idle: panel and backlight both switched off.
    fn set_idle(&mut self, idle: bool) {
        if idle {
            let _ = self.command(0x28); // display off
            self.set_backlight(false);
        } else {
            let _ = self.command(0x29); // display on
            self.set_backlight(true);
        }
    }

    fn set_led(&mut self, led: Led) {
        let on = match led {
            Led::Off => [false; 3],
            Led::Red => [true, false, false],
            Led::Green => [false, true, false],
            Led::Blue => [false, false, true],
        };
        for (pin, on) in self.led.iter_mut().zip(on) {
            if on != LED_ACTIVE_LOW {
                pin.set_high();
            } else {
                pin.set_low();
            }
        }
    }

    // Buttons on real hardware come from the GPIO thread in buttons.rs.
}

impl Drop for WhisplayDisplay {
    fn drop(&mut self) {
        let _ = self.command(0x28); // display off
        self.set_backlight(false);
    }
}

/// Write bytes to SPI in pieces small enough for spidev.
fn write_spi(spi: &mut Spi, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    for chunk in data.chunks(SPI_CHUNK) {
        let written = spi.write(chunk)?;
        if written != chunk.len() {
            return Err("SPI write was cut short".into());
        }
    }
    Ok(())
}
