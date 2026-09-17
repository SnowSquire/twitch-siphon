use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NanoId(u128);

impl NanoId {
    const LEN: usize = 21;
    const ALPHABET: &[u8; 64] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_-";

    pub const fn new(raw: u128) -> Self {
        Self(raw & (u128::MAX >> 2))
    }

    pub fn encode(self) -> [u8; Self::LEN] {
        let mut acc = self.0;
        let mut out = [0_u8; Self::LEN];
        for slot in &mut out {
            *slot = Self::ALPHABET[(acc & 63) as usize];
            acc >>= 6;
        }
        out
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != Self::LEN {
            return None;
        }
        let mut acc: u128 = 0;
        for (shift, byte) in value.bytes().enumerate() {
            let digit = Self::ALPHABET
                .iter()
                .position(|candidate| *candidate == byte)? as u128;
            acc |= digit << (6 * shift);
        }
        Some(Self(acc))
    }
}

impl fmt::Display for NanoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.encode();
        // Alphabet is ASCII, so the encoded bytes are valid UTF-8.
        let text = core::str::from_utf8(&bytes).unwrap_or_default();
        write!(f, "{text}")
    }
}

impl serde::Serialize for NanoId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = self.encode();
        let text = core::str::from_utf8(&bytes).unwrap_or_default();
        serializer.serialize_str(text)
    }
}

pub struct Rng {
    state: u64,
}

impl Rng {
    pub const fn new(state: u64) -> Self {
        Self { state }
    }

    pub fn nano_id(&mut self) -> NanoId {
        let raw = (u128::from(self.next_u64()) << 64) | u128::from(self.next_u64());
        NanoId::new(raw)
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Topic {
    BroadcastSettingsUpdate(u64),
    VideoPlaybackById(u64),
}

impl Topic {
    pub const fn for_channel(id: u64) -> [Self; 2] {
        [
            Self::BroadcastSettingsUpdate(id),
            Self::VideoPlaybackById(id),
        ]
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BroadcastSettingsUpdate(id) => write!(f, "broadcast-settings-update.{id}"),
            Self::VideoPlaybackById(id) => write!(f, "video-playback-by-id.{id}"),
        }
    }
}
