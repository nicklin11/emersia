//! Input events carried in `control::INPUT` records (device → host).
//!
//! Compact and versioned: `version (1) || event*`, each event `tag || fields`,
//! big-endian. Pointer coordinates are normalised to the *streamed output*
//! (0 = left/top edge, 65535 = right/bottom), so the device never needs to
//! know the host's layout; the host maps them onto the compositor's global
//! space. Keys are Linux evdev codes, which is what Android reports as the
//! scan code of a hardware keyboard.

/// Wire format version.
pub const VERSION: u8 = 1;
/// Most events one record may carry (well under the record MTU).
pub const MAX_EVENTS: usize = 128;

const TAG_POINTER: u8 = 1;
const TAG_BUTTON: u8 = 2;
const TAG_SCROLL: u8 = 3;
const TAG_KEY: u8 = 4;

/// Highest evdev key code accepted (`KEY_MAX`).
pub const KEY_MAX: u16 = 0x2ff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn to_u8(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            2 => Some(Self::Middle),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Absolute position on the streamed output, 0..=65535 on each axis.
    Pointer {
        x: u16,
        y: u16,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// Scroll in 1/120ths of a wheel detent (the evdev hi-res unit); positive
    /// `dy` scrolls down, positive `dx` right.
    Scroll {
        dx: i16,
        dy: i16,
    },
    /// Linux evdev key code.
    Key {
        code: u16,
        down: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    Empty,
    Version(u8),
    Truncated,
    UnknownTag(u8),
    BadButton(u8),
    BadKey(u16),
    TooMany,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty input record"),
            Self::Version(v) => write!(f, "unknown input format version {v}"),
            Self::Truncated => write!(f, "truncated input event"),
            Self::UnknownTag(t) => write!(f, "unknown input event tag {t}"),
            Self::BadButton(b) => write!(f, "unknown button {b}"),
            Self::BadKey(k) => write!(f, "key code {k} out of range"),
            Self::TooMany => write!(f, "more than {MAX_EVENTS} events in one record"),
        }
    }
}

impl std::error::Error for InputError {}

/// Serialise events into one record payload. Callers batch at most
/// [`MAX_EVENTS`]; extra events are an error rather than silently dropped.
pub fn encode(events: &[InputEvent]) -> Result<Vec<u8>, InputError> {
    if events.len() > MAX_EVENTS {
        return Err(InputError::TooMany);
    }
    let mut out = Vec::with_capacity(1 + events.len() * 5);
    out.push(VERSION);
    for event in events {
        match *event {
            InputEvent::Pointer { x, y } => {
                out.push(TAG_POINTER);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
            InputEvent::Button { button, down } => {
                out.extend_from_slice(&[TAG_BUTTON, button.to_u8(), u8::from(down)]);
            }
            InputEvent::Scroll { dx, dy } => {
                out.push(TAG_SCROLL);
                out.extend_from_slice(&dx.to_be_bytes());
                out.extend_from_slice(&dy.to_be_bytes());
            }
            InputEvent::Key { code, down } => {
                if code == 0 || code > KEY_MAX {
                    return Err(InputError::BadKey(code));
                }
                out.push(TAG_KEY);
                out.extend_from_slice(&code.to_be_bytes());
                out.push(u8::from(down));
            }
        }
    }
    Ok(out)
}

/// Parse a record payload. Strict: any malformed byte rejects the whole
/// record, so a half-understood batch is never injected.
pub fn decode(bytes: &[u8]) -> Result<Vec<InputEvent>, InputError> {
    let (&version, mut rest) = bytes.split_first().ok_or(InputError::Empty)?;
    if version != VERSION {
        return Err(InputError::Version(version));
    }
    let mut events = Vec::new();
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], InputError> {
        if rest.len() < n {
            return Err(InputError::Truncated);
        }
        let (head, tail) = rest.split_at(n);
        *rest = tail;
        Ok(head)
    }
    while let Some((&tag, tail)) = rest.split_first() {
        rest = tail;
        if events.len() == MAX_EVENTS {
            return Err(InputError::TooMany);
        }
        let event = match tag {
            TAG_POINTER => {
                let b = take(&mut rest, 4)?;
                InputEvent::Pointer {
                    x: u16::from_be_bytes([b[0], b[1]]),
                    y: u16::from_be_bytes([b[2], b[3]]),
                }
            }
            TAG_BUTTON => {
                let b = take(&mut rest, 2)?;
                InputEvent::Button {
                    button: Button::from_u8(b[0]).ok_or(InputError::BadButton(b[0]))?,
                    down: b[1] != 0,
                }
            }
            TAG_SCROLL => {
                let b = take(&mut rest, 4)?;
                InputEvent::Scroll {
                    dx: i16::from_be_bytes([b[0], b[1]]),
                    dy: i16::from_be_bytes([b[2], b[3]]),
                }
            }
            TAG_KEY => {
                let b = take(&mut rest, 3)?;
                let code = u16::from_be_bytes([b[0], b[1]]);
                if code == 0 || code > KEY_MAX {
                    return Err(InputError::BadKey(code));
                }
                InputEvent::Key {
                    code,
                    down: b[2] != 0,
                }
            }
            other => return Err(InputError::UnknownTag(other)),
        };
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<InputEvent> {
        vec![
            InputEvent::Pointer { x: 0, y: 65535 },
            InputEvent::Button {
                button: Button::Right,
                down: true,
            },
            InputEvent::Scroll { dx: -120, dy: 240 },
            InputEvent::Key {
                code: 30,
                down: false,
            },
        ]
    }

    #[test]
    fn roundtrips() {
        let bytes = encode(&sample()).unwrap();
        assert_eq!(bytes.len(), 1 + 5 + 3 + 5 + 4);
        assert_eq!(decode(&bytes).unwrap(), sample());
    }

    #[test]
    fn rejects_malformed_records_whole() {
        let bytes = encode(&sample()).unwrap();
        assert_eq!(decode(&[]), Err(InputError::Empty));
        assert_eq!(decode(&[9]), Err(InputError::Version(9)));
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]),
            Err(InputError::Truncated)
        );
        assert_eq!(decode(&[VERSION, 77]), Err(InputError::UnknownTag(77)));
        assert_eq!(
            decode(&[VERSION, TAG_BUTTON, 9, 1]),
            Err(InputError::BadButton(9))
        );
        assert_eq!(
            decode(&[VERSION, TAG_KEY, 0xff, 0xff, 1]),
            Err(InputError::BadKey(0xffff))
        );
        assert_eq!(
            decode(&[VERSION, TAG_KEY, 0, 0, 1]),
            Err(InputError::BadKey(0))
        );
    }

    #[test]
    fn bounds_event_count() {
        let many = vec![InputEvent::Pointer { x: 1, y: 1 }; MAX_EVENTS + 1];
        assert_eq!(encode(&many), Err(InputError::TooMany));
        let mut bytes = encode(&many[..MAX_EVENTS]).unwrap();
        bytes.extend_from_slice(&[TAG_POINTER, 0, 1, 0, 1]);
        assert_eq!(decode(&bytes), Err(InputError::TooMany));
    }

    #[test]
    fn empty_batch_is_valid() {
        assert_eq!(decode(&encode(&[]).unwrap()).unwrap(), vec![]);
    }
}
