use std::collections::HashMap;

use cpal::{
    traits::{DeviceTrait, HostTrait},
    BuildStreamError, Device, SampleFormat, Stream, SupportedStreamConfig,
    SupportedStreamConfigRange,
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

// Some WASAPI drivers report a float mix format but only accept integer PCM
// when a client opens the stream. Probe alternatives only after that rejection.
pub(super) fn open_stream(
    device: &Device,
    input: bool,
    mut build: impl FnMut(&SupportedStreamConfig) -> Result<Stream, BuildStreamError>,
) -> Result<Stream, String> {
    let kind = if input { "麦克风" } else { "扬声器" };
    let name = device.name().unwrap_or_else(|_| "未知设备".into());
    let default = if input {
        device.default_input_config()
    } else {
        device.default_output_config()
    }
    .map_err(|e| format!("无法读取{kind}「{name}」的默认音频格式：{e}"))?;
    let describe = |error| format!("无法打开{kind}「{name}」：{error}");
    match build(&default) {
        Ok(stream) => return Ok(stream),
        Err(BuildStreamError::StreamConfigNotSupported) => (),
        Err(other) => return Err(describe(other)),
    }
    let supported = if input {
        device.supported_input_configs().map(Iterator::collect)
    } else {
        device.supported_output_configs().map(Iterator::collect)
    }
    .map_err(|e| format!("无法查询{kind}「{name}」兼容的音频格式：{e}"))?;
    for config in fallback_configs(&default, supported) {
        match build(&config) {
            Ok(stream) => return Ok(stream),
            Err(BuildStreamError::StreamConfigNotSupported) => continue,
            Err(other) => return Err(describe(other)),
        }
    }
    Err(format!(
        "{kind}「{name}」拒绝了可用的音频格式，请在软件设置的「声音」中选择其他设备。"
    ))
}

fn fallback_configs(
    default: &SupportedStreamConfig,
    supported: Vec<SupportedStreamConfigRange>,
) -> Vec<SupportedStreamConfig> {
    let mut configs: Vec<_> = supported
        .into_iter()
        .map(|range| {
            let rate = default
                .sample_rate()
                .clamp(range.min_sample_rate(), range.max_sample_rate());
            range.with_sample_rate(rate)
        })
        .filter(|c| c != default)
        .collect();
    configs.sort_by_key(|c| {
        (
            c.channels().abs_diff(default.channels()),
            c.sample_rate().0.abs_diff(default.sample_rate().0),
            match c.sample_format() {
                SampleFormat::F32 => 0,
                SampleFormat::I32 => 1,
                SampleFormat::I16 => 2,
                SampleFormat::I64 => 3,
                _ => 4,
            },
        )
    });
    configs.dedup();
    configs
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{SampleRate, SupportedBufferSize};

    #[test]
    fn mono_usb_speaker_falls_back_to_i16_at_its_native_rate_before_u8() {
        let native = SampleRate(48_000);
        let range = |rate, format| {
            SupportedStreamConfigRange::new(1, rate, rate, SupportedBufferSize::Unknown, format)
        };
        let float = range(native, SampleFormat::F32);
        let fallback = fallback_configs(
            &float.with_sample_rate(native),
            vec![
                range(native, SampleFormat::U8),
                float,
                range(SampleRate(44_100), SampleFormat::I16),
                range(native, SampleFormat::I16),
            ],
        );
        assert_eq!(
            fallback
                .iter()
                .map(|c| (c.channels(), c.sample_rate().0, c.sample_format()))
                .collect::<Vec<_>>(),
            [
                (1, 48_000, SampleFormat::I16),
                (1, 48_000, SampleFormat::U8),
                (1, 44_100, SampleFormat::I16)
            ]
        );
    }
}
