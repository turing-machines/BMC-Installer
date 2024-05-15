use std::{
    fs,
    sync::mpsc::{self, channel},
    thread,
    time::Duration,
};

const STATUS_LED_PATH: &str = "/sys/class/leds/fp:sys/brightness";

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum LedState {
    /// All LEDs on, for the specified duration
    On(Duration),

    /// All LEDs off, for the specified duration
    Off(Duration),

    /// A custom combination of FP LED and swport LEDs, for the specified duration
    Custom(Duration, bool, [bool; 4]),
}
use LedState::*;

impl LedState {
    pub fn get_duration(self) -> Duration {
        match self {
            On(x) => x,
            Off(x) => x,
            Custom(x, _, _) => x,
        }
    }

    pub fn get_leds(self) -> (bool, [bool; 4]) {
        match self {
            On(_) => (true, [true; 4]),
            Off(_) => (false, [false; 4]),
            Custom(_, x, y) => (x, y),
        }
    }
}

pub const LED_READY: &[LedState] = &[
    On(Duration::from_millis(1500)),
    Off(Duration::from_millis(1500)),
];

pub const LED_BUSY: &[LedState] = &[
    Custom(Duration::from_millis(66), true, [true, false, false, false]),
    Custom(
        Duration::from_millis(66),
        false,
        [false, true, false, false],
    ),
    Custom(Duration::from_millis(66), true, [false, false, true, false]),
    Custom(
        Duration::from_millis(66),
        false,
        [false, false, false, true],
    ),
    Custom(
        Duration::from_millis(66),
        true,
        [false, false, false, false],
    ),
    Custom(
        Duration::from_millis(66),
        false,
        [false, false, false, false],
    ),
    Custom(
        Duration::from_millis(66),
        true,
        [false, false, false, false],
    ),
    Custom(
        Duration::from_millis(66),
        false,
        [false, false, false, false],
    ),
];

pub const LED_DONE: &[LedState] = &[
    On(Duration::from_millis(100)),
    Off(Duration::from_millis(200)),
    On(Duration::from_millis(100)),
    Off(Duration::from_millis(2600)),
];

const DIT: Duration = Duration::from_millis(150);
const DAH: Duration = Duration::from_millis(450);
const GAP: Duration = Duration::from_millis(1050);
pub const LED_ERROR: &[LedState] = &[
    // S ...
    On(DIT),
    Off(DIT),
    On(DIT),
    Off(DIT),
    On(DIT),
    Off(DAH),
    // O ---
    On(DAH),
    Off(DIT),
    On(DAH),
    Off(DIT),
    On(DAH),
    Off(DAH),
    // S ...
    On(DIT),
    Off(DIT),
    On(DIT),
    Off(DIT),
    On(DIT),
    Off(GAP),
];

/// This runs in a thread that communicates with the RTL8370MB to control its LEDs. These LEDs are
/// useful in case the user doesn't have other indicators connected (like front panel LEDs).
///
/// It's a separate thread because these operations may block a bit more than typical GPIO, and we
/// don't want to interfere with the timings on the main LED blinking thread.
///
/// Send `[bool; 4]` arrays to control the 4 external port LEDs (left-to-right).
pub fn rtl8370mb_led_thread(rx: mpsc::Receiver<[bool; 4]>) {
    const I2C_PATH: &str = "/dev/i2c-0";
    const I2C_ADDR: u8 = 0x5c;
    const REG_GPIO_O: [u16; 2] = [0x1d1d, 0x1d1e];
    const REG_GPIO_OE: [u16; 2] = [0x1d21, 0x1d22];
    const REG_GPIO_MODE: [u16; 2] = [0x1d25, 0x1d26];
    const LED_GPIO_MAP: [u8; 4] = [15, 16, 1, 7];

    let mut i2c = match i2c_linux::I2c::from_path(I2C_PATH) {
        Ok(x) => x,
        Err(_) => return,
    };

    // Updates the specified register on the IC, ignoring errors
    let mut write_reg = |index: u16, value: u16| {
        let mut data = [0u8; 4];
        data[0..2].copy_from_slice(&index.to_le_bytes());
        data[2..4].copy_from_slice(&value.to_le_bytes());
        let _ = i2c.i2c_transfer(&mut [i2c_linux::Message::Write {
            address: I2C_ADDR.into(),
            data: &data,
            flags: Default::default(),
        }]);
    };

    // Converts a sequence of GPIO indexes to the u16s representing those pins
    let to_bitmaps = |indexes: &mut dyn Iterator<Item = u8>| {
        let map: u32 = indexes.map(|index| 1 << index).fold(0, |x, y| x | y);
        [map as u16, (map >> 16) as u16]
    };

    let bitmaps = to_bitmaps(&mut LED_GPIO_MAP.into_iter());
    for regs in [REG_GPIO_O, REG_GPIO_OE, REG_GPIO_MODE] {
        for (reg, value) in regs.into_iter().zip(bitmaps) {
            write_reg(reg, value);
        }
    }

    // All LEDs are off
    let mut current_bitmaps = bitmaps;

    while let Ok(states) = rx.recv() {
        // Compute the bitmaps for this new state
        let new_bitmaps = to_bitmaps(&mut LED_GPIO_MAP.into_iter().zip(states).filter_map(
            |(index, on)| match on {
                // off -> '1'
                false => Some(index),

                // on -> '0'
                true => None,
            },
        ));

        // Update only the registers that need it
        for ((reg, to), from) in REG_GPIO_O.into_iter().zip(new_bitmaps).zip(current_bitmaps) {
            if to != from {
                write_reg(reg, to);
            }
        }

        current_bitmaps = new_bitmaps;
    }
}

/// This runs in a thread and manages the LED blinking.
///
/// Send new blink patterns through the MPSC channel to change the active pattern.
pub fn led_blink_thread() -> mpsc::Sender<&'static [LedState]> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut pattern = &[LedState::Off(Duration::from_secs(3600))][..];
        let mut remaining = &[][..];

        let (rtl_tx, rtl_rx) = mpsc::channel();
        thread::spawn(move || rtl8370mb_led_thread(rtl_rx));

        let mut brightness_file = fs::File::options().write(true).open(STATUS_LED_PATH).ok();
        let mut set_leds = move |fp, swports| {
            use std::io::Write;
            let _ = rtl_tx.send(swports);

            let chr = match fp {
                true => b'1',
                false => b'0',
            };
            if let Some(ref mut f) = &mut brightness_file {
                let _ = f.write_all(&[chr, b'\n']);
            }
        };

        loop {
            let cmd = match remaining.split_first() {
                Some((first, elements)) => {
                    remaining = elements;
                    *first
                }
                None => {
                    remaining = pattern;
                    continue;
                }
            };

            let (fw, swports) = cmd.get_leds();
            set_leds(fw, swports);

            match rx.recv_timeout(cmd.get_duration()) {
                Err(err) => match err {
                    mpsc::RecvTimeoutError::Timeout => (),
                    mpsc::RecvTimeoutError::Disconnected => break,
                },
                Ok(new_pattern) => {
                    pattern = new_pattern;
                    remaining = &[];
                }
            }
        }

        set_leds(false, [false; 4]);
    });
    tx
}
