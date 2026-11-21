use super::{Error, Result, SEGMENT_LIMIT};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use flate2::{Decompress, FlushDecompress, Status};
use std::borrow::Cow;

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum Kind {
    TransportStream,
    Init,
    Fragment,
}

pub fn unwrap(input: &[u8], limit: usize) -> Result<Cow<'_, [u8]>> {
    if input.len() > limit {
        return Err(Error::Limit);
    }
    let Some(mut chunks) = input.strip_prefix(b"\x89PNG\r\n\x1a\n") else {
        return Ok(Cow::Borrowed(input));
    };
    while chunks.len() >= 12 {
        let size = u32::from_be_bytes(chunks[..4].try_into().unwrap()) as usize;
        if size > chunks.len() - 12 {
            return Err(Error::Unavailable);
        }
        if &chunks[4..8] == b"roUd" {
            let (flag, payload) = chunks[8..8 + size]
                .split_first()
                .ok_or(Error::Unavailable)?;
            return match flag {
                0 => Ok(Cow::Borrowed(payload)),
                1 => inflate(payload, limit).map(Cow::Owned),
                _ => Err(Error::Unavailable),
            };
        }
        chunks = &chunks[12 + size..];
    }
    Err(Error::Unavailable)
}

fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = Decompress::new(true);
    let mut out = Vec::new();
    let mut block = [0; 8192];
    loop {
        let consumed = decoder.total_in() as usize;
        let written = decoder.total_out();
        let status = decoder
            .decompress(&input[consumed..], &mut block, FlushDecompress::None)
            .map_err(|_| Error::Unavailable)?;
        let size = (decoder.total_out() - written) as usize;
        if size > limit - out.len() {
            return Err(Error::Limit);
        }
        out.extend_from_slice(&block[..size]);
        if status == Status::StreamEnd {
            return Ok(out);
        }
        if size == 0 && decoder.total_in() as usize == consumed {
            return Err(Error::Unavailable);
        }
    }
}

pub fn decode(input: &[u8], kind: Kind, aes: Option<(&[u8; 16], &[u8; 16])>) -> Result<Vec<u8>> {
    let mut plain = unwrap(input, SEGMENT_LIMIT)?.into_owned();
    if let Some((key, iv)) = aes {
        let size = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut plain)
            .map_err(|_| Error::Unavailable)?
            .len();
        plain.truncate(size);
    }
    match kind {
        Kind::TransportStream => {
            if plain.len() < 376
                || !plain.len().is_multiple_of(188)
                || plain.chunks_exact(188).any(|p| p[0] != 0x47)
            {
                return Err(Error::Unavailable);
            }
        }
        Kind::Init | Kind::Fragment => validate_mp4(&plain, kind)?,
    }
    Ok(plain)
}

fn validate_mp4(mut data: &[u8], kind: Kind) -> Result<()> {
    let (mut ftyp, mut moov, mut moof, mut mdat) = (false, false, false, false);
    while !data.is_empty() {
        let header = data.get(..8).ok_or(Error::Unavailable)?;
        let short = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, minimum) = match short {
            0 => (data.len(), 8),
            1 => {
                let large = data.get(8..16).ok_or(Error::Unavailable)?;
                (
                    usize::try_from(u64::from_be_bytes(large.try_into().unwrap()))
                        .map_err(|_| Error::Limit)?,
                    16,
                )
            }
            n => (n as usize, 8),
        };
        if size < minimum || size > data.len() {
            return Err(Error::Unavailable);
        }
        match &header[4..8] {
            b"ftyp" => ftyp = true,
            b"moov" => moov = true,
            b"moof" => moof = true,
            b"mdat" => mdat = true,
            _ => (),
        }
        data = &data[size..];
    }
    if (kind == Kind::Init && ftyp && moov && !moof && !mdat)
        || (kind == Kind::Fragment && moof && mdat && !moov)
    {
        Ok(())
    } else {
        Err(Error::Unavailable)
    }
}
