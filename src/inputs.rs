use mirajazz::{error::MirajazzError, types::DeviceInput};

use crate::mappings::{DEVICE_KEY_COUNT, KEY_COUNT};

pub fn process_input(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
    log::info!("Processing input: {}, {}", input, state);

    match input as usize {
        (0..=DEVICE_KEY_COUNT) => read_button_press(input, state),
        _ => Err(MirajazzError::BadData),
    }
}

/// Converts opendeck key index (3x5 grid, row-major) to device key index
pub fn opendeck_to_device(key: u8) -> u8 {
    if key < KEY_COUNT as u8 {
        [12, 9, 6, 3, 0, 13, 10, 7, 4, 1, 14, 11, 8, 5, 2][key as usize]
    } else {
        key
    }
}

/// Converts device key index to opendeck key index
///
/// Returns `None` for the right side display, which is not a key
pub fn device_to_opendeck(key: usize) -> Option<usize> {
    let key = key - 1; // We have to subtract 1 from key index reported by device, because list is shifted by 1

    if key < KEY_COUNT {
        Some([4, 9, 14, 3, 8, 13, 2, 7, 12, 1, 6, 11, 0, 5, 10][key])
    } else {
        None
    }
}

fn read_button_press(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
    let mut button_states = vec![false; KEY_COUNT];

    if input != 0 {
        // Touches on the right side display are ignored, it is not a key
        if let Some(pressed_index) = device_to_opendeck(input as usize) {
            button_states[pressed_index] = state != 0;
        }
    }

    Ok(DeviceInput::ButtonStateChange(button_states))
}
