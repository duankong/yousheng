use base64::{engine::general_purpose::STANDARD, Engine};
use jpeg_decoder::Decoder;

const MAX_AVATAR_BYTES: usize = 8192;
const MAX_AVATAR_SIDE: u16 = 96;
const AVATAR_FORMAT_ERROR: &str = "头像须为有效的 JPEG 图片";

pub fn nickname(value: &str) -> Result<&str, &'static str> {
    let name = value.trim();
    if name.is_empty() || name.chars().count() > 32 || name.chars().any(char::is_control) {
        return Err("昵称须为 1–32 个可显示字符");
    }
    Ok(name)
}

pub fn validate_avatar(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Ok(());
    }
    let encoded = value
        .strip_prefix("data:image/jpeg;base64,")
        .ok_or(AVATAR_FORMAT_ERROR)?;
    if encoded.len() > 4 * MAX_AVATAR_BYTES.div_ceil(3) {
        return Err("头像不能超过 8 KiB");
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| AVATAR_FORMAT_ERROR)?;
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err("头像不能超过 8 KiB");
    }
    let mut decoder = Decoder::new(bytes.as_slice());
    decoder.set_max_decoding_buffer_size(4 * usize::from(MAX_AVATAR_SIDE).pow(2));
    decoder.read_info().map_err(|_| AVATAR_FORMAT_ERROR)?;
    let info = decoder.info().ok_or(AVATAR_FORMAT_ERROR)?;
    if !(1..=MAX_AVATAR_SIDE).contains(&info.width) || !(1..=MAX_AVATAR_SIDE).contains(&info.height)
    {
        return Err("头像尺寸不能超过 96 × 96 像素");
    }
    decoder.decode().map_err(|_| AVATAR_FORMAT_ERROR)?;
    Ok(())
}
