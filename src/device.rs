use data_url::DataUrl;
use image::{DynamicImage, imageops::FilterType, load_from_memory_with_format};
use mirajazz::{device::Device, error::MirajazzError, state::DeviceStateUpdate};
use openaction::{OUTBOUND_EVENT_MANAGER, SetImageEvent};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{
    DEVICES, FLUSH_NOTIFY, TOKENS,
    inputs::opendeck_to_device,
    mappings::{
        COL_COUNT, CandidateDevice, ENCODER_COUNT, KEY_COUNT, Kind, ROW_COUNT,
        SIDE_PANEL_DEVICE_KEYS, get_image_format_for_key, get_image_format_for_side_tile,
    },
};

/// File names (next to the plugin executable) that are looked up for the right side display image
const SIDE_PANEL_FILES: [&str; 3] = ["side-panel.png", "side-panel.jpg", "side-panel.jpeg"];

fn load_side_panel_image() -> Option<DynamicImage> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();

    for name in SIDE_PANEL_FILES {
        let path = dir.join(name);

        if !path.exists() {
            continue;
        }

        match image::open(&path) {
            Ok(image) => return Some(image),
            Err(err) => log::error!("Could not load side panel image {:?}: {}", path, err),
        }
    }

    None
}

/// Draws the right side display as one tall panel, split across its 3 hardware tiles.
/// The panel is not a key and is not exposed to OpenDeck. Changes must be flushed.
pub async fn draw_side_panel(device: &Device, kind: &Kind) -> Result<(), MirajazzError> {
    let format = get_image_format_for_side_tile(kind);
    let (width, height) = (format.size.0 as u32, format.size.1 as u32);
    let tiles = SIDE_PANEL_DEVICE_KEYS.len() as u32;

    match load_side_panel_image() {
        Some(image) => {
            let panel = image.resize_to_fill(width, height * tiles, FilterType::Lanczos3);

            for (i, key) in SIDE_PANEL_DEVICE_KEYS.iter().enumerate() {
                let tile = panel.crop_imm(0, i as u32 * height, width, height);
                device.set_button_image(*key, format, tile).await?;
            }
        }
        None => {
            for key in SIDE_PANEL_DEVICE_KEYS {
                device.clear_button_image(key).await?;
            }
        }
    }

    Ok(())
}

/// Initializes a device and listens for events
pub async fn device_task(candidate: CandidateDevice, token: CancellationToken) {
    log::info!("Running device task for {:?}", candidate);

    // Wrap in a closure so we can use `?` operator
    let device = async || -> Result<Device, MirajazzError> {
        let device = connect(&candidate).await?;

        device.set_brightness(50).await?;
        device.clear_all_button_images().await?;
        draw_side_panel(&device, &candidate.kind).await?;
        device.flush().await?;

        Ok(device)
    }()
    .await;

    let device: Device = match device {
        Ok(device) => device,
        Err(err) => {
            handle_error(&candidate.id, err).await;

            log::error!(
                "Had error during device init, finishing device task: {:?}",
                candidate
            );

            return;
        }
    };

    log::info!("Registering device {}", candidate.id);
    if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
        outbound
            .register_device(
                candidate.id.clone(),
                candidate.kind.human_name(),
                ROW_COUNT as u8,
                COL_COUNT as u8,
                ENCODER_COUNT as u8,
                0,
            )
            .await
            .unwrap();
    }

    DEVICES.write().await.insert(candidate.id.clone(), device);

    let flush_notify = Arc::new(Notify::new());
    FLUSH_NOTIFY
        .write()
        .await
        .insert(candidate.id.clone(), flush_notify.clone());

    tokio::select! {
        _ = device_events_task(&candidate) => {},
        _ = device_flush_task(&candidate.id, flush_notify, token.clone()) => {},
        _ = token.cancelled() => {}
    };

    FLUSH_NOTIFY.write().await.remove(&candidate.id);

    log::info!("Shutting down device {:?}", candidate);

    if let Some(device) = DEVICES.read().await.get(&candidate.id) {
        device.shutdown().await.ok();
    }

    log::info!("Device task finished for {:?}", candidate);
}

/// Handles errors, returning true if should continue, returning false if an error is fatal
pub async fn handle_error(id: &String, err: MirajazzError) -> bool {
    log::error!("Device {} error: {}", id, err);

    // Some errors are not critical and can be ignored without sending disconnected event
    if matches!(err, MirajazzError::ImageError(_) | MirajazzError::BadData) {
        return true;
    }

    log::info!("Deregistering device {}", id);
    if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
        outbound.deregister_device(id.clone()).await.unwrap();
    }

    log::info!("Cancelling tasks for device {}", id);
    if let Some(token) = TOKENS.read().await.get(id) {
        token.cancel();
    }

    log::info!("Removing device {} from the list", id);
    DEVICES.write().await.remove(id);

    log::info!("Finished clean-up for {}", id);

    false
}

pub async fn connect(candidate: &CandidateDevice) -> Result<Device, MirajazzError> {
    let result = Device::connect(
        &candidate.dev,
        candidate.kind.protocol_version(),
        KEY_COUNT,
        ENCODER_COUNT,
    )
    .await;

    match result {
        Ok(device) => Ok(device),
        Err(e) => {
            log::error!("Error while connecting to device: {e}");

            Err(e)
        }
    }
}

/// Flushes queued images after a 50ms quiet window following the last set_button_image call.
/// Sleeps entirely when the device is idle — no polling.
async fn device_flush_task(id: &String, notify: Arc<Notify>, token: CancellationToken) {
    loop {
        tokio::select! {
            _ = notify.notified() => {}
            _ = token.cancelled() => break,
        }

        // Keep resetting the window while more images arrive; fire when quiet for 16ms.
        loop {
            tokio::select! {
                _ = notify.notified() => {}
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(50)) => break,
            }
        }

        let flush_result = {
            let guard = DEVICES.read().await;
            if let Some(device) = guard.get(id) {
                log::info!("Flushing pending updates");
                device.flush().await
            } else {
                Ok(())
            }
        };
        if let Err(err) = flush_result {
            handle_error(id, err).await;
            break;
        }
    }
}

/// Handles events from device to OpenDeck
async fn device_events_task(candidate: &CandidateDevice) -> Result<(), MirajazzError> {
    log::info!("Connecting to {} for incoming events", candidate.id);

    let devices_lock = DEVICES.read().await;
    let reader = match devices_lock.get(&candidate.id) {
        Some(device) => device.get_reader(crate::inputs::process_input),
        None => return Ok(()),
    };
    drop(devices_lock);

    log::info!("Connected to {} for incoming events", candidate.id);

    log::info!("Reader is ready for {}", candidate.id);

    loop {
        log::info!("Reading updates...");

        let updates = match reader.read(None).await {
            Ok(updates) => updates,
            Err(e) => {
                if !handle_error(&candidate.id, e).await {
                    break;
                }

                continue;
            }
        };

        for update in updates {
            log::info!("New update: {:#?}", update);

            let id = candidate.id.clone();

            if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
                match update {
                    DeviceStateUpdate::ButtonDown(key) => outbound.key_down(id, key).await.unwrap(),
                    DeviceStateUpdate::ButtonUp(key) => outbound.key_up(id, key).await.unwrap(),
                    DeviceStateUpdate::EncoderDown(encoder) => {
                        outbound.encoder_down(id, encoder).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderUp(encoder) => {
                        outbound.encoder_up(id, encoder).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderTwist(encoder, val) => {
                        outbound
                            .encoder_change(id, encoder, val as i16)
                            .await
                            .unwrap();
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handles different combinations of "set image" event, including clearing the specific buttons and whole device
pub async fn handle_set_image(
    device: &Device,
    id: &str,
    evt: SetImageEvent,
) -> Result<(), MirajazzError> {
    let kind = Kind::from_vid_pid(device.vid, device.pid).unwrap(); // Safe to unwrap here, because device is already filtered

    // Positions outside of the 3x5 grid (e.g. from profiles made with the old 3x6 layout)
    // must never reach the right side display
    if evt.position.is_some_and(|position| position as usize >= KEY_COUNT) {
        log::warn!("Ignoring image for position outside of the key grid: {:?}", evt.position);

        return Ok(());
    }

    match (evt.position, evt.image) {
        (Some(position), Some(image)) => {
            log::info!("Setting image for button {}", position);

            // OpenDeck sends image as a data url, so parse it using a library
            let url = DataUrl::process(image.as_str()).unwrap(); // Isn't expected to fail, so unwrap it is
            let (body, _fragment) = url.decode_to_vec().unwrap(); // Same here

            // Allow only image/jpeg mime for now
            if url.mime_type().subtype != "jpeg" {
                log::error!("Incorrect mime type: {}", url.mime_type());

                return Ok(()); // Not a fatal error, enough to just log it
            }

            let image = load_from_memory_with_format(body.as_slice(), image::ImageFormat::Jpeg)?;

            device
                .set_button_image(
                    opendeck_to_device(position),
                    get_image_format_for_key(&kind),
                    image,
                )
                .await?;

            if let Some(notify) = FLUSH_NOTIFY.read().await.get(id) {
                notify.notify_one();
            }
        }
        (Some(position), None) => {
            device
                .clear_button_image(opendeck_to_device(position))
                .await?;

            if let Some(notify) = FLUSH_NOTIFY.read().await.get(id) {
                notify.notify_one();
            }
        }
        (None, None) => {
            device.clear_all_button_images().await?;
            draw_side_panel(device, &kind).await?;
            device.flush().await?;
        }
        _ => {}
    }

    Ok(())
}
