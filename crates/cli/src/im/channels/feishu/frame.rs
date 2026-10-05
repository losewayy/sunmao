//! Feishu's WS frame codec. The long connection is not plain JSON: every
//! binary message is a `pbbp2.Frame` protobuf (the SDK's schema, nine
//! fields + a repeated header pair). Hand-rolled here because the shape
//! is small and a protobuf runtime would be a whole dependency for one
//! message type; field numbers and wire types are taken from the
//! reference SDK's generated codec.

/// Control frame (`method` 0) — ping/pong/handshake headers ride here.
pub const METHOD_CONTROL: i32 = 0;
/// Data frame (`method` 1) — the event envelope rides in `payload`.
pub const METHOD_DATA: i32 = 1;

/// One decoded frame. Optional scalars stay optional so the ack can echo
/// the frame it answers byte-for-byte in shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<(String, String)>,
    pub payload_encoding: Option<String>,
    pub payload_type: Option<String>,
    pub payload: Vec<u8>,
    pub log_id_new: Option<String>,
}

impl Frame {
    /// The value of header `key`, if the frame carries one.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Encode for the wire. Fields 1-4 are `required` in the schema, so
    /// they are written even when zero (the server's decoder enforces
    /// their presence).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.payload.len());
        put_varint(&mut out, 8);
        put_varint(&mut out, self.seq_id);
        put_varint(&mut out, 16);
        put_varint(&mut out, self.log_id);
        put_varint(&mut out, 24);
        put_varint(&mut out, self.service as u32 as u64);
        put_varint(&mut out, 32);
        put_varint(&mut out, self.method as u32 as u64);
        for (k, v) in &self.headers {
            let mut h = Vec::with_capacity(k.len() + v.len() + 4);
            put_str(&mut h, 1, k);
            put_str(&mut h, 2, v);
            put_bytes(&mut out, 5, &h);
        }
        if let Some(v) = &self.payload_encoding {
            put_str(&mut out, 6, v);
        }
        if let Some(v) = &self.payload_type {
            put_str(&mut out, 7, v);
        }
        if !self.payload.is_empty() {
            put_bytes(&mut out, 8, &self.payload);
        }
        if let Some(v) = &self.log_id_new {
            put_str(&mut out, 9, v);
        }
        out
    }

    /// Decode a binary WS message. `None` on any malformed input — a bad
    /// frame is a dropped frame, never a panic.
    pub fn decode(buf: &[u8]) -> Option<Frame> {
        let mut r = Reader { buf, pos: 0 };
        let mut f = Frame::default();
        let (mut seq, mut log, mut service, mut method) = (false, false, false, false);
        while let Some(tag) = r.varint() {
            let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
            match (field, wire) {
                (1, 0) => {
                    f.seq_id = r.varint()?;
                    seq = true;
                }
                (2, 0) => {
                    f.log_id = r.varint()?;
                    log = true;
                }
                (3, 0) => {
                    f.service = r.varint()? as u32 as i32;
                    service = true;
                }
                (4, 0) => {
                    f.method = r.varint()? as u32 as i32;
                    method = true;
                }
                (5, 2) => f.headers.push(decode_header(r.bytes()?)?),
                (6, 2) => f.payload_encoding = Some(r.string()?),
                (7, 2) => f.payload_type = Some(r.string()?),
                (8, 2) => f.payload = r.bytes()?.to_vec(),
                (9, 2) => f.log_id_new = Some(r.string()?),
                _ => {
                    if !r.skip(wire) {
                        return None;
                    }
                }
            }
        }
        (seq && log && service && method).then_some(f)
    }
}

fn decode_header(buf: &[u8]) -> Option<(String, String)> {
    let mut r = Reader { buf, pos: 0 };
    let (mut key, mut value) = (None, None);
    while let Some(tag) = r.varint() {
        let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
        match (field, wire) {
            (1, 2) => key = Some(r.string()?),
            (2, 2) => value = Some(r.string()?),
            _ => {
                if !r.skip(wire) {
                    return None;
                }
            }
        }
    }
    Some((key?, value?))
}

/// A control ping for `service` — what keeps the long connection alive and
/// makes the server send its pong (carrying refreshed intervals).
pub fn ping(service: i32) -> Vec<u8> {
    Frame {
        service,
        method: METHOD_CONTROL,
        headers: vec![("type".to_string(), "ping".to_string())],
        ..Frame::default()
    }
    .encode()
}

/// The ack for one data frame: the frame it answers, plus the `biz_rt`
/// header and the `{"code":200}` payload the sender waits for.
pub fn ack(frame: &Frame, biz_rt_ms: u64) -> Vec<u8> {
    let mut answered = frame.clone();
    answered
        .headers
        .push(("biz_rt".to_string(), biz_rt_ms.to_string()));
    answered.payload = br#"{"code":200}"#.to_vec();
    answered.encode()
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    put_varint(out, ((field << 3) | 2) as u64);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_str(out: &mut Vec<u8>, field: u32, s: &str) {
    put_bytes(out, field, s.as_bytes());
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len)?;
        let out = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn string(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }

    /// Skip one unknown field — wire types 0/1/2/5; anything else is a
    /// corrupt frame.
    fn skip(&mut self, wire: u32) -> bool {
        match wire {
            0 => self.varint().is_some(),
            1 => self.advance(8),
            2 => self.bytes().is_some(),
            5 => self.advance(4),
            _ => false,
        }
    }

    fn advance(&mut self, n: usize) -> bool {
        match self.pos.checked_add(n) {
            Some(end) if end <= self.buf.len() => {
                self.pos = end;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Frame {
        Frame {
            seq_id: 7,
            log_id: 99,
            service: 33_000_001,
            method: METHOD_DATA,
            headers: vec![
                ("type".into(), "event".into()),
                ("message_id".into(), "om_1".into()),
                ("sum".into(), "1".into()),
                ("seq".into(), "0".into()),
            ],
            payload_encoding: Some("json".into()),
            payload_type: Some("event".into()),
            payload: br#"{"hello":"world"}"#.to_vec(),
            log_id_new: Some("log-new".into()),
        }
    }

    #[test]
    fn round_trips_every_field() {
        let f = sample();
        assert_eq!(Frame::decode(&f.encode()).unwrap(), f);
    }

    #[test]
    fn ping_is_a_control_frame() {
        let f = Frame::decode(&ping(42)).unwrap();
        assert_eq!(f.method, METHOD_CONTROL);
        assert_eq!(f.service, 42);
        assert_eq!(f.header("type"), Some("ping"));
        assert!(f.payload.is_empty());
    }

    #[test]
    fn ack_echoes_headers_and_answers_ok() {
        let f = sample();
        let acked = Frame::decode(&ack(&f, 12)).unwrap();
        assert_eq!(acked.seq_id, f.seq_id);
        assert_eq!(acked.header("type"), Some("event"));
        assert_eq!(acked.header("biz_rt"), Some("12"));
        assert_eq!(acked.payload, br#"{"code":200}"#);
    }

    #[test]
    fn malformed_frames_are_dropped_not_panicking() {
        assert!(Frame::decode(&[]).is_none());
        let f = sample().encode();
        assert!(Frame::decode(&f[..f.len() - 3]).is_none());
        // a frame missing a required scalar is not a frame
        let mut out = Vec::new();
        put_varint(&mut out, 24);
        put_varint(&mut out, 1);
        assert!(Frame::decode(&out).is_none());
    }
}
