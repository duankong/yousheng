use std::collections::HashMap;

use cpal::{
    traits::{DeviceTrait, HostTrait},
    Device,
};
use serde::Serialize;

#[derive(Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct AudioDevices {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

fn endpoints(input: bool) -> Result<Vec<(DeviceInfo, Device)>, String> {
    let host = cpal::default_host();
    let devices = if input {
        host.input_devices()
    } else {
        host.output_devices()
    }
    .map_err(|error| error.to_string())?;
    let mut seen = HashMap::<String, usize>::new();
    Ok(devices
        .filter_map(|device| {
            let name = device.name().ok()?;
            let ordinal = seen.entry(name.clone()).or_default();
            *ordinal += 1;
            let id = format!("{ordinal}:{name}");
            Some((DeviceInfo { id, name }, device))
        })
        .collect())
}

pub(super) fn enumerate() -> Result<AudioDevices, String> {
    Ok(AudioDevices {
        inputs: endpoints(true)?.into_iter().map(|(info, _)| info).collect(),
        outputs: endpoints(false)?
            .into_iter()
            .map(|(info, _)| info)
            .collect(),
    })
}

pub(super) fn select(id: &str, input: bool) -> Result<Device, String> {
    let kind = if input { "麦克风" } else { "播放设备" };
    if id.is_empty() {
        let host = cpal::default_host();
        let device = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
        return device.ok_or_else(|| format!("系统没有可用的{kind}"));
    }
    endpoints(input)?
        .into_iter()
        .find(|(info, _)| info.id == id)
        .map(|(_, device)| device)
        .ok_or_else(|| format!("选定的{kind}已断开，请重新选择"))
}
