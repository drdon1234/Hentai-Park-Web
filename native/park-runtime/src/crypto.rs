use crate::{Error, Invocation};
use aes::cipher::{BlockDecrypt, BlockSizeUser, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use aes_gcm::aead::{
    consts::{U12, U16},
    Aead, Payload,
};
use aes_gcm::AesGcm;
use base64::prelude::{Engine, BASE64_STANDARD};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::Sha256;

pub(crate) fn run(operation: &str, args: &Value, call: &Invocation) -> Result<Value, Error> {
    let text = |name: &str| args[name].as_str().ok_or(Error::Config);
    match operation {
        "md5Hex" => {
            let digest = |value: &Value| {
                call.check()?;
                Ok(json!(hex::encode(Md5::digest(
                    value.as_str().ok_or(Error::Config)?
                ))))
            };
            match &args["text"] {
                Value::Array(values) => values
                    .iter()
                    .map(digest)
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array),
                value => digest(value),
            }
        }
        "sha256Hex" => Ok(json!(hex::encode(Sha256::digest(text("text")?)))),
        "hmacSha256Hex" => {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(text("key")?.as_bytes())
                .map_err(|_| Error::Config)?;
            mac.update(text("text")?.as_bytes());
            Ok(json!(hex::encode(mac.finalize().into_bytes())))
        }
        "randomBytes" => {
            let n = args["length"]
                .as_u64()
                .filter(|n| (1..=65536).contains(n))
                .ok_or(Error::Config)?;
            let mut bytes = vec![0; n as usize];
            getrandom::fill(&mut bytes).map_err(|_| Error::Storage)?;
            Ok(json!(bytes))
        }
        "uuid" => {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).map_err(|_| Error::Storage)?;
            bytes[6] = (bytes[6] & 0x0f) | 0x40;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            let h = hex::encode(bytes);
            Ok(json!(format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )))
        }
        "digestBytes" => {
            let values = args["bytes"]
                .as_array()
                .filter(|v| v.len() <= 2_097_152)
                .ok_or(Error::Config)?;
            let bytes = values
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or(Error::Config)
                })
                .collect::<Result<Vec<_>, _>>()?;
            match text("algorithm")? {
                "sha1" => Ok(json!(Sha1::digest(&bytes).to_vec())),
                "sha256" => Ok(json!(Sha256::digest(&bytes).to_vec())),
                _ => Err(Error::Config),
            }
        }
        "aesEcbDecrypt" => {
            let mut bytes = BASE64_STANDARD
                .decode(text("base64")?.trim())
                .map_err(|_| Error::Config)?;
            let key = text("key")?.as_bytes();
            match key.len() {
                16 => decrypt_blocks::<Aes128>(&mut bytes, key)?,
                24 => decrypt_blocks::<Aes192>(&mut bytes, key)?,
                32 => decrypt_blocks::<Aes256>(&mut bytes, key)?,
                _ => return Err(Error::Config),
            }
            if let Some(&padding @ 1..=16) = bytes.last() {
                let end = bytes
                    .len()
                    .checked_sub(padding as usize)
                    .ok_or(Error::Config)?;
                bytes.truncate(end);
            }
            Ok(json!(String::from_utf8_lossy(&bytes)))
        }
        "aesGcmEncrypt" | "aesGcmDecrypt" => {
            let encrypt = operation == "aesGcmEncrypt";
            let field = |name: &str, limit: usize| {
                let raw = text(name)?;
                if raw.len() > limit * 2 {
                    return Err(Error::Config);
                }
                hex::decode(raw).map_err(|_| Error::Config)
            };
            let key = field("key", 32)?;
            let iv: [u8; 12] = field("iv", 12)?.try_into().map_err(|_| Error::Config)?;
            let aad = if args["aad"].is_null() {
                vec![]
            } else {
                field("aad", 4096)?
            };
            let data = if encrypt {
                field("plaintext", 4080)?
            } else {
                field("ciphertext", 4096)?
            };
            if !encrypt && data.len() < 16 {
                return Err(Error::Config);
            }
            let payload = Payload {
                msg: &data,
                aad: &aad,
            };
            let result = match key.len() {
                16 => crypt::<AesGcm<Aes128, U12>>(&key, &iv, payload, encrypt),
                24 => crypt::<AesGcm<Aes192, U12>>(&key, &iv, payload, encrypt),
                32 => crypt::<AesGcm<Aes256, U12>>(&key, &iv, payload, encrypt),
                _ => Err(Error::Config),
            }?;
            Ok(json!(result))
        }
        _ => Err(Error::Unsupported),
    }
}

fn decrypt_blocks<C: BlockDecrypt + BlockSizeUser<BlockSize = U16> + KeyInit>(
    bytes: &mut [u8],
    key: &[u8],
) -> Result<(), Error> {
    let cipher = C::new_from_slice(key).map_err(|_| Error::Config)?;
    let mut blocks = bytes.chunks_exact_mut(16);
    for block in &mut blocks {
        cipher.decrypt_block(block.into());
    }
    blocks.into_remainder().fill(0);
    Ok(())
}

fn crypt<C: Aead<NonceSize = U12> + KeyInit>(
    key: &[u8],
    iv: &[u8; 12],
    payload: Payload<'_, '_>,
    encrypt: bool,
) -> Result<Vec<u8>, Error> {
    let cipher = C::new_from_slice(key).map_err(|_| Error::Config)?;
    if encrypt {
        cipher.encrypt(iv.into(), payload)
    } else {
        cipher.decrypt(iv.into(), payload)
    }
    .map_err(|_| Error::Config)
}
