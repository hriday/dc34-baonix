pub const SYNC: u16 = 0xB0C1;
pub const PAGE: usize = 4096;
const HEADER: usize = 5; // SYNC + TYPE + LEN
const TRAILER: usize = 4; // CRC32
/// Longest legal payload: a page plus its 4-byte page number.
const MAX_PAYLOAD: usize = PAGE + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    ReadReq { page: u32 },
    ReadResp { page: u32, data: Box<[u8; PAGE]> },
    WriteReq { page: u32, data: Box<[u8; PAGE]> },
    WriteAck { page: u32 },
    ConOut(Vec<u8>),
    ConIn(Vec<u8>),
    Err { code: u16, page: u32 },
}

impl Frame {
    pub(crate) fn type_byte(&self) -> u8 {
        match self {
            Frame::ReadReq { .. } => 0x01,
            Frame::ReadResp { .. } => 0x02,
            Frame::WriteReq { .. } => 0x03,
            Frame::WriteAck { .. } => 0x04,
            Frame::ConOut(_) => 0x05,
            Frame::ConIn(_) => 0x06,
            Frame::Err { .. } => 0x07,
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    // Bitwise CRC-32/ISO-HDLC. No table: this runs on a microcontroller and
    // 16k frames per boot does not justify 1 KiB of table.
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

pub fn encode(f: &Frame, out: &mut Vec<u8>) {
    match f {
        Frame::ConOut(b) => {
            // Console bytes are a stream: splitting across frames is free, and it
            // keeps every emitted frame inside MAX_PAYLOAD, which is what makes the
            // decoder's length guard a safety check rather than a silent drop.
            for chunk in b.chunks(MAX_PAYLOAD) {
                encode_frame_body(0x05, chunk, out);
            }
            // an empty payload is still a frame worth sending
            if b.is_empty() {
                encode_frame_body(0x05, &[], out);
            }
        }
        Frame::ConIn(b) => {
            // Console bytes are a stream: splitting across frames is free, and it
            // keeps every emitted frame inside MAX_PAYLOAD, which is what makes the
            // decoder's length guard a safety check rather than a silent drop.
            for chunk in b.chunks(MAX_PAYLOAD) {
                encode_frame_body(0x06, chunk, out);
            }
            // an empty payload is still a frame worth sending
            if b.is_empty() {
                encode_frame_body(0x06, &[], out);
            }
        }
        _ => {
            let mut payload = Vec::with_capacity(MAX_PAYLOAD);
            match f {
                Frame::ReadReq { page } | Frame::WriteAck { page } => {
                    payload.extend_from_slice(&page.to_le_bytes());
                }
                Frame::ReadResp { page, data } | Frame::WriteReq { page, data } => {
                    payload.extend_from_slice(&page.to_le_bytes());
                    payload.extend_from_slice(&data[..]);
                }
                Frame::ConOut(_) | Frame::ConIn(_) => unreachable!(),
                Frame::Err { code, page } => {
                    payload.extend_from_slice(&code.to_le_bytes());
                    payload.extend_from_slice(&page.to_le_bytes());
                }
            }
            encode_frame_body(f.type_byte(), &payload, out);
        }
    }
}

fn encode_frame_body(type_byte: u8, payload: &[u8], out: &mut Vec<u8>) {
    let len = payload.len() as u16;
    let mut body = Vec::with_capacity(3 + payload.len());
    body.push(type_byte);
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(payload);

    out.extend_from_slice(&SYNC.to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_le_bytes());
}

/// How many bytes of off-protocol text a noise-capturing [`Decoder`] will hold
/// before it starts dropping them.
///
/// The host drains after every `read()`, so this only fills if one read
/// produced more noise than this — 16x a 4 KiB read. The oldest bytes are the
/// ones kept, because the first line of a panic (`PANIC in PID n:`) is the one
/// worth having.
pub const MAX_NOISE: usize = 64 * 1024;

#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Bytes discarded as not-a-frame, kept only when `capture` is set.
    noise: Vec<u8>,
    dropped_noise: usize,
    capture: bool,
    /// How many discarded bytes to keep. The count of the rest is still exact.
    noise_cap: usize,
}

impl Decoder {
    pub fn new() -> Self { Self::default() }

    /// A decoder that keeps the bytes it discards, for [`Decoder::take_noise`].
    ///
    /// # Why this is not the default
    ///
    /// **The host wants it and the badge must not have it.** On the host,
    /// discarded bytes are the badge's USB panic mirror: `rv64-host serve` sat
    /// in front of it silently eating `PANIC in PID n:` and the text after it,
    /// so the first hardware failure produced no diagnosis at all until the
    /// server was stopped and a plain reader attached. Everything this project
    /// built to make a panic visible was defeated by a decoder in front of it.
    ///
    /// On the badge, the same buffer would be a `Vec` growing inside a process
    /// with a couple of hundred kilobytes to its name, holding bytes nobody
    /// will ever read. So capture is opt-in, and `Mux::new` — which is what the
    /// badge uses — leaves it off.
    pub fn capturing_noise() -> Self {
        Self::capturing_noise_capped(MAX_NOISE)
    }

    /// The same, keeping at most `cap` bytes.
    ///
    /// **The count is exact whatever the cap is** — only the sample is bounded.
    /// That is what makes a small cap worth having on the badge, where 64 KiB
    /// is out of the question but a 64-byte sample plus an exact total answers
    /// "what is the decoder throwing away, and how much of it".
    pub fn capturing_noise_capped(cap: usize) -> Self {
        Self { capture: true, noise_cap: cap, ..Self::default() }
    }

    pub fn push(&mut self, bytes: &[u8]) { self.buf.extend_from_slice(bytes); }

    /// Bytes discarded as not-a-frame since the last call, and how many more
    /// were dropped because [`MAX_NOISE`] was reached.
    ///
    /// Always empty unless the decoder was built by [`Decoder::capturing_noise`].
    pub fn take_noise(&mut self) -> (Vec<u8>, usize) {
        (core::mem::take(&mut self.noise), core::mem::take(&mut self.dropped_noise))
    }

    /// Drops the first `n` bytes of the buffer, keeping them if asked to.
    fn discard(&mut self, n: usize) {
        if self.capture {
            let room = self.noise_cap.saturating_sub(self.noise.len());
            let keep = room.min(n);
            self.noise.extend_from_slice(&self.buf[..keep]);
            self.dropped_noise += n - keep;
        }
        self.buf.drain(..n);
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let Some(start) = self.find_sync() else {
                // No SYNC anywhere. Everything except a possible first half of
                // one is noise, and draining it here is what bounds the buffer:
                // before this, a stream that never contained a frame — which is
                // exactly what the panic mirror is — accumulated forever.
                //
                // The held-back byte is only held back when it could actually
                // *be* the low half of a SYNC. Keeping the last byte
                // unconditionally would mean a transcript always trailing one
                // character behind the badge, which on a panic is the character
                // someone is waiting for.
                let keep = usize::from(self.buf.last() == Some(&SYNC.to_le_bytes()[0]));
                self.discard(self.buf.len() - keep);
                return None;
            };
            if start > 0 { self.discard(start); }
            if self.buf.len() < HEADER { return None; }

            let len = u16::from_le_bytes([self.buf[3], self.buf[4]]) as usize;
            if len > MAX_PAYLOAD {
                self.discard(2); // implausible length: this SYNC was noise
                continue;
            }
            let total = HEADER + len + TRAILER;
            if self.buf.len() < total { return None; }

            let body = &self.buf[2..HEADER + len];
            let want = u32::from_le_bytes([
                self.buf[HEADER + len],
                self.buf[HEADER + len + 1],
                self.buf[HEADER + len + 2],
                self.buf[HEADER + len + 3],
            ]);
            if crc32(body) != want {
                // Either a corrupted frame or two SYNC-shaped bytes inside
                // ordinary text. Nothing here can tell which, so it is captured
                // as noise: a mangled frame in a transcript is a clue, and a
                // silently eaten one is not.
                self.discard(2);
                continue;
            }
            let frame = decode_body(self.buf[2], &self.buf[HEADER..HEADER + len]);
            // A well-formed frame of an unknown type is *not* noise -- its CRC
            // matched, so it is protocol, just protocol this build does not
            // know. Dropping it into a text transcript would only confuse.
            self.buf.drain(..total);
            if frame.is_some() { return frame; }
        }
    }

    fn find_sync(&self) -> Option<usize> {
        let s = SYNC.to_le_bytes();
        self.buf.windows(2).position(|w| w == s)
    }
}

fn decode_body(ty: u8, p: &[u8]) -> Option<Frame> {
    let page = |p: &[u8]| u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let boxed = |p: &[u8]| -> Box<[u8; PAGE]> {
        let mut d = Box::new([0u8; PAGE]);
        d.copy_from_slice(&p[4..4 + PAGE]);
        d
    };
    match ty {
        0x01 if p.len() == 4 => Some(Frame::ReadReq { page: page(p) }),
        0x02 if p.len() == 4 + PAGE => Some(Frame::ReadResp { page: page(p), data: boxed(p) }),
        0x03 if p.len() == 4 + PAGE => Some(Frame::WriteReq { page: page(p), data: boxed(p) }),
        0x04 if p.len() == 4 => Some(Frame::WriteAck { page: page(p) }),
        0x05 => Some(Frame::ConOut(p.to_vec())),
        0x06 => Some(Frame::ConIn(p.to_vec())),
        0x07 if p.len() == 6 => Some(Frame::Err {
            code: u16::from_le_bytes([p[0], p[1]]),
            page: u32::from_le_bytes([p[2], p[3], p[4], p[5]]),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: Frame) {
        let mut buf = Vec::new();
        encode(&f, &mut buf);
        let mut d = Decoder::new();
        d.push(&buf);
        assert_eq!(d.next_frame(), Some(f));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn every_frame_kind_round_trips() {
        roundtrip(Frame::ReadReq { page: 0x1234 });
        roundtrip(Frame::WriteAck { page: 0 });
        roundtrip(Frame::ConOut(b"hello".to_vec()));
        roundtrip(Frame::ConIn(vec![0x0d]));
        roundtrip(Frame::Err { code: 7, page: 9 });
        roundtrip(Frame::ReadResp { page: 5, data: Box::new([0xab; 4096]) });
        roundtrip(Frame::WriteReq { page: 6, data: Box::new([0x17; 4096]) });
    }

    /// The USB layer truncates at 3840 bytes, so a 4 KiB frame always arrives
    /// in pieces. The decoder must accumulate.
    #[test]
    fn a_frame_delivered_in_fragments_smaller_than_the_usb_cap_still_decodes() {
        let f = Frame::ReadResp { page: 42, data: Box::new([0x5a; 4096]) };
        let mut buf = Vec::new();
        encode(&f, &mut buf);
        assert!(buf.len() > 3840, "fixture must exceed the USB send cap");

        let mut d = Decoder::new();
        for chunk in buf.chunks(1000) {
            assert_eq!(d.next_frame(), None, "no frame before the last chunk");
            d.push(chunk);
        }
        assert_eq!(d.next_frame(), Some(f));
    }

    /// **The regression for the blocker of the first hardware run.** The badge's
    /// panic mirror shares the CDC endpoint with the protocol, and a plain
    /// decoder scans past it looking for SYNC and drops it. A capturing one
    /// hands it back.
    #[test]
    fn text_interleaved_with_frames_is_kept_rather_than_eaten() {
        let mut wire = Vec::new();
        wire.extend_from_slice(b"PANIC in PID 4: no ticktimer\n");
        encode(&Frame::ReadReq { page: 7 }, &mut wire);
        wire.extend_from_slice(b"more text after the frame\n");

        let mut d = Decoder::capturing_noise();
        d.push(&wire);
        assert_eq!(d.next_frame(), Some(Frame::ReadReq { page: 7 }));
        // The trailing text has no SYNC, so it is recognised as noise on the
        // call that finds no frame — which `serve_once` always makes.
        assert_eq!(d.next_frame(), None);
        let (noise, dropped) = d.take_noise();
        assert_eq!(dropped, 0);
        assert_eq!(
            String::from_utf8_lossy(&noise),
            "PANIC in PID 4: no ticktimer\nmore text after the frame\n"
        );
    }

    /// A decoder that is not capturing keeps nothing, so the badge pays no
    /// memory for a buffer nobody there will read.
    #[test]
    fn a_plain_decoder_captures_nothing() {
        let mut d = Decoder::new();
        d.push(b"lots of text that is not a frame at all");
        assert_eq!(d.next_frame(), None);
        assert_eq!(d.take_noise(), (Vec::new(), 0));
    }

    /// Before this, a stream that never contained a frame grew the decoder's
    /// buffer without limit — and the panic mirror is exactly such a stream.
    #[test]
    fn pure_text_does_not_accumulate_in_the_buffer() {
        let mut d = Decoder::new();
        for _ in 0..1000 {
            d.push(b"no frames here, just log lines\n");
            assert_eq!(d.next_frame(), None);
        }
        assert!(d.buf.is_empty(), "the decoder is hoarding {} bytes of text", d.buf.len());
    }

    /// A frame split across pushes must still assemble, which is what the
    /// buffer-draining fix above could plausibly have broken: the last byte is
    /// held back precisely because it may be the first half of a SYNC.
    #[test]
    fn draining_text_does_not_eat_a_sync_that_arrives_split() {
        let f = Frame::ReadReq { page: 3 };
        let mut buf = Vec::new();
        encode(&f, &mut buf);

        let mut d = Decoder::capturing_noise();
        d.push(b"noise");
        d.push(&buf[..1]); // the low SYNC byte, alone
        assert_eq!(d.next_frame(), None);
        d.push(&buf[1..]);
        assert_eq!(d.next_frame(), Some(f));
        assert_eq!(String::from_utf8_lossy(&d.take_noise().0), "noise");
    }

    #[test]
    fn noise_capture_is_bounded_and_reports_what_it_dropped() {
        let mut d = Decoder::capturing_noise();
        d.push(&vec![b'x'; MAX_NOISE + 500]);
        assert_eq!(d.next_frame(), None);
        let (noise, dropped) = d.take_noise();
        assert_eq!(noise.len(), MAX_NOISE);
        assert_eq!(dropped, 500);
    }

    /// A small cap keeps a usable *sample* while the count stays exact. This is
    /// what lets the badge carry noise capture at all: 64 bytes of hex is what
    /// a human reads, and the total is what tells them whether 64 bytes is the
    /// whole story.
    #[test]
    fn a_capped_capture_keeps_a_sample_and_an_exact_count() {
        let mut d = Decoder::capturing_noise_capped(64);
        d.push(&vec![b'z'; 5000]);
        assert_eq!(d.next_frame(), None);
        let (noise, dropped) = d.take_noise();
        assert_eq!(noise.len(), 64, "the sample must be capped");
        assert_eq!(noise.len() + dropped, 5000, "the count must be exact regardless of the cap");
    }

    #[test]
    fn a_corrupt_frame_is_dropped_and_the_next_one_still_decodes() {
        let mut buf = Vec::new();
        encode(&Frame::ReadReq { page: 1 }, &mut buf);
        let corrupt_at = buf.len() - 1;
        buf[corrupt_at] ^= 0xff; // break the CRC
        encode(&Frame::ReadReq { page: 2 }, &mut buf);

        let mut d = Decoder::new();
        d.push(&buf);
        assert_eq!(d.next_frame(), Some(Frame::ReadReq { page: 2 }));
    }

    #[test]
    fn garbage_before_a_frame_is_skipped() {
        let mut buf = vec![0x00, 0xff, 0x13, 0x37];
        encode(&Frame::ReadReq { page: 3 }, &mut buf);
        let mut d = Decoder::new();
        d.push(&buf);
        assert_eq!(d.next_frame(), Some(Frame::ReadReq { page: 3 }));
    }

    #[test]
    fn large_console_payload_is_chunked_across_frames() {
        let original_payload = vec![0x42; MAX_PAYLOAD * 2 + 7];
        let f = Frame::ConOut(original_payload.clone());
        let mut buf = Vec::new();
        encode(&f, &mut buf);

        let mut d = Decoder::new();
        d.push(&buf);

        let mut collected = Vec::new();
        loop {
            match d.next_frame() {
                Some(Frame::ConOut(data)) => collected.extend(data),
                None => break,
                Some(frame) => panic!("Expected ConOut frame, got {:?}", frame),
            }
        }

        assert_eq!(collected, original_payload);
    }

    #[test]
    fn empty_console_payload_produces_one_frame() {
        let f = Frame::ConOut(vec![]);
        let mut buf = Vec::new();
        encode(&f, &mut buf);

        let mut d = Decoder::new();
        d.push(&buf);
        assert_eq!(d.next_frame(), Some(Frame::ConOut(vec![])));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn read_resp_encodes_as_exactly_one_frame() {
        let f = Frame::ReadResp { page: 5, data: Box::new([0xab; 4096]) };
        let mut buf = Vec::new();
        encode(&f, &mut buf);

        let mut d = Decoder::new();
        d.push(&buf);
        assert_eq!(d.next_frame(), Some(f.clone()));
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn a_console_frame_arriving_mid_request_does_not_masquerade_as_the_response() {
        let mut wire = Vec::new();
        encode(&Frame::ConIn(b"ls\r".to_vec()), &mut wire);
        encode(&Frame::ReadResp { page: 77, data: Box::new([0x11; PAGE]) }, &mut wire);

        let mut m = Mux::new();
        m.push(&wire);

        // The waiter asks for its response and gets it, not the keystrokes.
        assert_eq!(
            m.take_matching(0x02),
            Some(Frame::ReadResp { page: 77, data: Box::new([0x11; PAGE]) })
        );
        // The keystrokes were held aside, not dropped.
        assert_eq!(m.take_console(), b"ls\r".to_vec());
    }

    #[test]
    fn console_input_accumulates_across_several_frames() {
        let mut wire = Vec::new();
        encode(&Frame::ConIn(b"ab".to_vec()), &mut wire);
        encode(&Frame::ConIn(b"cd".to_vec()), &mut wire);
        let mut m = Mux::new();
        m.push(&wire);
        assert_eq!(m.take_console(), b"abcd".to_vec());
        assert_eq!(m.take_console(), Vec::<u8>::new());
    }
}

/// Demultiplexes a stream in which console frames may arrive at any time,
/// including while a page request is outstanding.
#[derive(Default)]
pub struct Mux {
    dec: Decoder,
    console: Vec<u8>,
    held: Vec<Frame>,
}

impl Mux {
    pub fn new() -> Self { Self::default() }

    /// A `Mux` whose decoder keeps the bytes it discards. See
    /// [`Decoder::capturing_noise`] for why this is opt-in: the host needs it
    /// so the badge's panic mirror is not eaten, and the badge must not have it.
    pub fn capturing_noise() -> Self {
        Self { dec: Decoder::capturing_noise(), ..Self::default() }
    }

    /// [`Mux::capturing_noise`] keeping at most `cap` bytes of sample. The
    /// discarded *count* stays exact. See [`Decoder::capturing_noise_capped`].
    pub fn capturing_noise_capped(cap: usize) -> Self {
        Self { dec: Decoder::capturing_noise_capped(cap), ..Self::default() }
    }

    /// Off-protocol bytes received since the last call, and how many more were
    /// dropped at [`MAX_NOISE`]. Always empty unless built by
    /// [`Mux::capturing_noise`].
    pub fn take_noise(&mut self) -> (Vec<u8>, usize) { self.dec.take_noise() }

    pub fn push(&mut self, bytes: &[u8]) {
        self.dec.push(bytes);
        while let Some(f) = self.dec.next_frame() {
            match f {
                Frame::ConIn(b) | Frame::ConOut(b) => self.console.extend_from_slice(&b),
                other => self.held.push(other),
            }
        }
    }

    /// Removes and returns the first held frame whose type byte is `want`.
    pub fn take_matching(&mut self, want: u8) -> Option<Frame> {
        let i = self.held.iter().position(|f| f.type_byte() == want)?;
        Some(self.held.remove(i))
    }

    /// Drains console bytes received so far.
    pub fn take_console(&mut self) -> Vec<u8> { core::mem::take(&mut self.console) }
}
