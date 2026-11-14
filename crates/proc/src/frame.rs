//! Кадры обмена с дочерним процессом: длина (u32, LE) и содержимое. Сообщение — два кадра: заголовок и двоичная часть
//! (тела писем и файлы идут без перекодирования).

use std::io::{self, Read, Write};

/// Наибольший кадр заголовка и двоичной части по умолчанию.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

pub fn write_frame(w: &mut impl Write, payload: &[u8], max: usize) -> io::Result<()> {
    if payload.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "кадр слишком большой"));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "кадр слишком большой"));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Сообщение: заголовок (не больше [`MAX_FRAME`]) и двоичная часть (не больше `max_bin`).
pub fn write_msg(w: &mut impl Write, head: &[u8], bin: &[u8], max_bin: usize) -> io::Result<()> {
    write_frame(w, head, MAX_FRAME)?;
    write_frame(w, bin, max_bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_respect_the_limit() {
        let mut buf = Vec::new();
        write_msg(&mut buf, b"head", b"binary", 16).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r, 16).unwrap(), b"head");
        assert_eq!(read_frame(&mut r, 16).unwrap(), b"binary");
        assert!(write_frame(&mut Vec::new(), &[0; 17], 16).is_err());
        let mut long = Vec::new();
        write_frame(&mut long, &[0; 17], 32).unwrap();
        assert!(read_frame(&mut long.as_slice(), 16).is_err());
    }
}
