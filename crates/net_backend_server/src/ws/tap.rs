//! The bytes a client sends, between the socket and tungstenite. tungstenite logs every frame and
//! every message it receives at TRACE through the `log` crate, with the content, and has no setting
//! that turns this off. An `auth` message carries an access token, so the tap takes every `auth`
//! message out of the stream before tungstenite reads it: tungstenite gets [`MARKER`] in its place,
//! and the connection takes the real text from [`Diverted`] when the marker arrives (one entry per
//! marker, in order).
//!
//! Everything else reaches tungstenite unchanged, except a text message sent in several frames: it
//! arrives as one frame with the same content (it may be an `auth`). A frame the tap does not
//! expect (an unmasked frame, reserved bits or opcodes, a frame out of sequence, a fragmented or
//! oversized control frame) reaches tungstenite as its header alone with an empty payload, so
//! tungstenite refuses it on the header as before while the content (it may be an `auth`) never
//! reaches it; a text message started before is replaced by the start of an empty one. Nothing
//! after a refused frame is handed over, and the end of the stream inside a frame hands over
//! nothing of that frame. A frame or message over the size limit is refused by tungstenite before
//! it reads the content (close 1009): the tap hands over a header one byte over tungstenite's limit.
//!
//! The size limit is a [`TapLimit`]: [`PRE_AUTH_MAX_BYTES`] until the socket authenticates, then
//! `ws.max_message_bytes` (the connection raises it).

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// What tungstenite receives in place of an `auth` message. It is an `auth` message itself, so a
/// client that sends this text is diverted like any other.
pub(crate) const MARKER: &str = r#"{"type":"auth","data":"(read by the server, not by tungstenite)"}"#;

/// The largest frame or message a socket may send before it authenticated (at most
/// `ws.max_message_bytes`). The only message it may send then is an `auth` (an access token of
/// the `Auth` module is 69 characters; 16 KiB leaves room for a game's own authenticator's
/// tokens, e.g. JWTs), so a socket waiting for `auth` holds a few KiB, not several copies of
/// `ws.max_message_bytes`.
pub(crate) const PRE_AUTH_MAX_BYTES: usize = 16 * 1024;

/// Bytes read from the socket per call.
const READ_CHUNK: usize = 8 * 1024;

/// Buffers above this size are given back once empty (an idle socket keeps little memory).
const KEEP: usize = 1024;

/// The largest control frame payload (RFC 6455 section 5.5).
const MAX_CONTROL: u64 = 125;

const CONTINUATION: u8 = 0x0;
const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;

/// The `auth` messages the tap took out, oldest first.
#[derive(Clone, Default)]
pub(crate) struct Diverted(Arc<Mutex<VecDeque<String>>>);

impl Diverted {
    fn push(&self, text: String) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push_back(text);
    }

    /// The `auth` message a [`MARKER`] stands for.
    pub(crate) fn pop(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).pop_front()
    }
}

/// The tap's current size limit for one frame or message, shared with the connection (which
/// raises it once the socket authenticated).
#[derive(Clone)]
pub(crate) struct TapLimit(Arc<AtomicUsize>);

impl TapLimit {
    pub(crate) fn new(bytes: usize) -> Self {
        Self(Arc::new(AtomicUsize::new(bytes)))
    }

    pub(crate) fn set(&self, bytes: usize) {
        self.0.store(bytes, Ordering::Relaxed);
    }

    fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// A frame header (RFC 6455 section 5.2).
struct Head {
    /// The first byte (FIN, RSV1-3, opcode).
    first: u8,
    fin: bool,
    reserved: bool,
    opcode: u8,
    mask: Option<[u8; 4]>,
    len: u64,
    /// The header's own length in bytes.
    size: usize,
}

/// The header at the start of `raw`; `None` until all of it arrived.
fn parse_head(raw: &[u8]) -> Option<Head> {
    let (&first, &second) = (raw.first()?, raw.get(1)?);
    let (len, mut size) = match second & 0x7F {
        126 => (u64::from(u16::from_be_bytes(raw.get(2..4)?.try_into().ok()?)), 4),
        127 => (u64::from_be_bytes(raw.get(2..10)?.try_into().ok()?), 10),
        short => (u64::from(short), 2),
    };
    let mask = if second & 0x80 != 0 {
        let mask: [u8; 4] = raw.get(size..size + 4)?.try_into().ok()?;
        size += 4;
        Some(mask)
    } else {
        None
    };
    Some(Head { first, fin: first & 0x80 != 0, reserved: first & 0x70 != 0, opcode: first & 0x0F, mask, len, size })
}

/// The header of a text frame of `len` bytes, masked with a zero mask (the content unchanged).
fn text_head(fin: bool, len: u64) -> Vec<u8> {
    let mut head = Vec::with_capacity(14);
    head.push(if fin { 0x80 | TEXT } else { TEXT });
    match (u8::try_from(len), u16::try_from(len)) {
        (Ok(short), _) if short < 126 => head.push(0x80 | short),
        (_, Ok(medium)) => {
            head.push(0x80 | 126);
            head.extend_from_slice(&medium.to_be_bytes());
        }
        _ => {
            head.push(0x80 | 127);
            head.extend_from_slice(&len.to_be_bytes());
        }
    }
    head.extend_from_slice(&[0; 4]);
    head
}

/// A refused frame's header with an empty payload: the same FIN / RSV / opcode bits and the same
/// mask bit (a zero mask), so tungstenite refuses it for the same reason.
fn empty_head(head: &Head) -> Vec<u8> {
    match head.mask {
        Some(_) => vec![head.first, 0x80, 0, 0, 0, 0],
        None => vec![head.first, 0],
    }
}

fn unmask(payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    payload.iter().zip(mask.iter().cycle()).map(|(byte, m)| byte ^ m).collect()
}

fn is_auth(payload: &[u8]) -> bool {
    std::str::from_utf8(payload).is_ok_and(super::connection::is_auth_frame)
}

/// The socket under tungstenite on the server: reads go through the tap, writes go straight out.
pub(crate) struct AuthTap<S> {
    inner: S,
    /// Read from the socket, not looked at yet.
    raw: Vec<u8>,
    /// Ready for tungstenite; `sent` bytes of it were handed over already.
    out: Vec<u8>,
    sent: usize,
    /// A text message sent in several frames, unmasked, until its last frame.
    text: Option<Vec<u8>>,
    /// Inside a binary message sent in several frames (its frames pass unchanged): its size so far.
    binary: Option<usize>,
    /// A frame was refused: nothing more is handed over (tungstenite refuses the socket).
    refused: bool,
    eof: bool,
    /// The largest frame or message now (see [`TapLimit`]).
    limit: TapLimit,
    /// tungstenite's own limit (`ws.max_message_bytes`).
    hard: usize,
    diverted: Diverted,
}

impl<S> AuthTap<S> {
    /// A tap with the size limit `limit` (the connection raises it after `auth`), under
    /// tungstenite's own limit `hard`.
    pub(crate) fn new(inner: S, limit: TapLimit, hard: usize, diverted: Diverted) -> Self {
        Self { inner, raw: Vec::new(), out: Vec::new(), sent: 0, text: None, binary: None, refused: false, eof: false, limit, hard, diverted }
    }

    /// Refuse the frame `head`: tungstenite gets its header with an empty payload (after the start
    /// of an empty text message in place of a started one), then nothing more.
    fn refuse(&mut self, head: &Head) {
        if self.text.take().is_some() {
            self.out.extend_from_slice(&text_head(false, 0));
        }
        self.out.extend_from_slice(&empty_head(head));
        self.stop();
    }

    /// Refuse an oversized frame or message: a text header one byte over tungstenite's limit
    /// (tungstenite closes with 1009 on the header alone), then nothing more.
    fn too_big(&mut self) {
        self.text = None;
        self.out.extend_from_slice(&text_head(true, self.hard as u64 + 1));
        self.stop();
    }

    /// Hand nothing more over; forget what was read.
    fn stop(&mut self) {
        self.refused = true;
        self.raw = Vec::new();
    }

    /// A whole text message: diverted when it is an `auth`, else one frame with the same content.
    fn text_message(&mut self, payload: Vec<u8>) {
        if is_auth(&payload) {
            self.diverted.push(String::from_utf8(payload).unwrap_or_default());
            self.out.extend_from_slice(&text_head(true, MARKER.len() as u64));
            self.out.extend_from_slice(MARKER.as_bytes());
        } else {
            self.out.extend_from_slice(&text_head(true, payload.len() as u64));
            self.out.extend_from_slice(&payload);
        }
    }

    /// Look at the frame at the start of `raw`. False when more bytes are needed first.
    fn step(&mut self) -> bool {
        let Some(head) = parse_head(&self.raw) else { return false };
        let control = head.opcode & 0x8 != 0;
        let known = matches!(head.opcode, CONTINUATION | TEXT | BINARY | 0x8..=0xA);
        let in_sequence = match head.opcode {
            CONTINUATION => self.text.is_some() || self.binary.is_some(),
            TEXT | BINARY => self.text.is_none() && self.binary.is_none(),
            // Control frames: at most 125 bytes.
            _ => head.len <= MAX_CONTROL,
        };
        let expected = known && !head.reserved && in_sequence && (head.fin || !control);
        let Some(mask) = head.mask.filter(|_| expected) else {
            self.refuse(&head);
            return true;
        };
        let limit = self.limit.get().min(self.hard);
        let before = match head.opcode {
            CONTINUATION => self.text.as_ref().map_or(0, Vec::len) + self.binary.unwrap_or(0),
            _ => 0,
        };
        let len = match usize::try_from(head.len) {
            Ok(len) if before.saturating_add(len) <= limit => len,
            // Too big: refused on a header, before tungstenite reads any content.
            _ => {
                self.too_big();
                return true;
            }
        };
        let end = head.size + len;
        if self.raw.len() < end {
            return false;
        }
        let frame: Vec<u8> = self.raw.drain(..end).collect();
        if self.raw.is_empty() && self.raw.capacity() > KEEP {
            self.raw = Vec::new();
        }
        let payload = frame.get(head.size..).unwrap_or_default();
        match head.opcode {
            BINARY => {
                self.binary = (!head.fin).then_some(len);
                self.out.extend_from_slice(&frame);
            }
            CONTINUATION if self.binary.is_some() => {
                self.binary = (!head.fin).then_some(before + len);
                self.out.extend_from_slice(&frame);
            }
            CONTINUATION => {
                let mut text = self.text.take().unwrap_or_default();
                text.extend(payload.iter().zip(mask.iter().cycle()).map(|(byte, m)| byte ^ m));
                if head.fin {
                    self.text_message(text);
                } else {
                    self.text = Some(text);
                }
            }
            TEXT => {
                let text = unmask(payload, mask);
                if !head.fin {
                    self.text = Some(text);
                } else if is_auth(&text) {
                    self.text_message(text);
                } else {
                    self.out.extend_from_slice(&frame);
                }
            }
            // Control frames (ping, pong, close) pass unchanged, also between fragments.
            _ => self.out.extend_from_slice(&frame),
        }
        true
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for AuthTap<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.sent < this.out.len() {
                let n = (this.out.len() - this.sent).min(buf.remaining());
                buf.put_slice(&this.out[this.sent..this.sent + n]);
                this.sent += n;
                if this.sent == this.out.len() {
                    this.sent = 0;
                    if this.out.capacity() > KEEP {
                        this.out = Vec::new();
                    } else {
                        this.out.clear();
                    }
                }
                return Poll::Ready(Ok(()));
            }
            if this.refused || this.eof {
                // After a refusal tungstenite has what it refuses on; at the end of the stream a
                // cut frame is not handed over (it may be an `auth`): tungstenite sees the end.
                return Poll::Ready(Ok(()));
            }
            if this.step() {
                continue;
            }
            let mut chunk = [0u8; READ_CHUNK];
            let mut read = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut read))?;
            if read.filled().is_empty() {
                this.eof = true;
                this.raw = Vec::new();
                this.text = None;
                continue;
            }
            this.raw.extend_from_slice(read.filled());
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for AuthTap<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 1000;

    /// A client frame (masked with a non-zero mask).
    fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xFA, 0x21, 0x3D];
        let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
        match payload.len() {
            len @ 0..=125 => out.push(0x80 | len as u8),
            len @ 126..=0xFFFF => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(unmask(payload, mask));
        out
    }

    /// Feed `input` through a tap, `piece` bytes per read; what tungstenite would read, and the
    /// diverted texts.
    fn run(input: &[u8], piece: usize) -> (Vec<u8>, Vec<String>) {
        run_limited(input, piece, &TapLimit::new(LIMIT), LIMIT)
    }

    fn run_limited(input: &[u8], piece: usize, limit: &TapLimit, hard: usize) -> (Vec<u8>, Vec<String>) {
        let mut reader = Pieces { data: input.to_vec(), at: 0, piece: piece.max(1) };
        let diverted = Diverted::default();
        let mut tap = AuthTap::new(&mut reader, limit.clone(), hard, diverted.clone());
        let mut seen = Vec::new();
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            let mut chunk = [0u8; 300];
            let mut buf = ReadBuf::new(&mut chunk);
            match Pin::new(&mut tap).poll_read(&mut cx, &mut buf) {
                Poll::Ready(Ok(())) if buf.filled().is_empty() => break,
                Poll::Ready(Ok(())) => seen.extend_from_slice(buf.filled()),
                other => panic!("{other:?}"),
            }
        }
        let mut texts = Vec::new();
        while let Some(text) = diverted.pop() {
            texts.push(text);
        }
        (seen, texts)
    }

    /// A reader that gives at most `piece` bytes per read.
    struct Pieces {
        data: Vec<u8>,
        at: usize,
        piece: usize,
    }

    impl AsyncRead for Pieces {
        fn poll_read(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let n = this.piece.min(buf.remaining()).min(this.data.len() - this.at);
            buf.put_slice(&this.data[this.at..this.at + n]);
            this.at += n;
            Poll::Ready(Ok(()))
        }
    }

    /// The messages tungstenite reads from `bytes` (as a server, with the same limits).
    fn messages(bytes: Vec<u8>) -> (Vec<tungstenite::Message>, Option<tungstenite::Error>) {
        let config = tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(LIMIT)).max_frame_size(Some(LIMIT));
        let mut socket = tungstenite::WebSocket::from_raw_socket(Duplex(io::Cursor::new(bytes)), tungstenite::protocol::Role::Server, Some(config));
        let mut out = Vec::new();
        loop {
            match socket.read() {
                Ok(message) => out.push(message),
                Err(tungstenite::Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return (out, None),
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return (out, None),
                Err(error) => return (out, Some(error)),
            }
        }
    }

    struct Duplex(io::Cursor<Vec<u8>>);

    impl io::Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match io::Read::read(&mut self.0, buf)? {
                0 => Err(io::ErrorKind::UnexpectedEof.into()),
                n => Ok(n),
            }
        }
    }

    impl io::Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn texts(messages: &[tungstenite::Message]) -> Vec<String> {
        messages.iter().filter_map(|m| m.to_text().ok().filter(|_| m.is_text()).map(str::to_string)).collect()
    }

    /// Every payload byte tungstenite would receive in `bytes`, unmasked, frame by frame (a cut
    /// last frame with what arrived of it).
    fn payloads(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(head) = parse_head(rest) {
            let end = (head.size as u64).saturating_add(head.len).min(rest.len() as u64) as usize;
            let payload = rest.get(head.size..end).unwrap_or_default();
            match head.mask {
                Some(mask) => out.extend(unmask(payload, mask)),
                None => out.extend_from_slice(payload),
            }
            rest = &rest[end..];
        }
        // A cut header: its bytes as they are.
        out.extend_from_slice(rest);
        out
    }

    /// Whether an access token is in what tungstenite would receive.
    fn token_reaches_tungstenite(seen: &[u8]) -> bool {
        payloads(seen).windows(5).any(|w| w == b"nbsa_")
    }

    const AUTH: &str = r#"{"type":"auth","data":{"token":"nbsa_secret_token_value"}}"#;

    /// The inputs of the "no token reaches tungstenite" checks below.
    fn token_inputs() -> Vec<(&'static str, Vec<u8>)> {
        let (a, b) = AUTH.as_bytes().split_at(20);
        let mut fragmented = frame(false, TEXT, a);
        fragmented.extend(frame(true, 0x9, b"p2"));
        fragmented.extend(frame(true, CONTINUATION, b));
        let mut rsv1 = frame(true, TEXT, AUTH.as_bytes());
        rsv1[0] |= 0x40;
        let mut binary_open = frame(false, BINARY, &[1, 2, 3]);
        binary_open.extend(frame(true, TEXT, AUTH.as_bytes()));
        let mut text_open = frame(false, TEXT, b"{\"type\":");
        text_open.extend(frame(true, TEXT, AUTH.as_bytes()));
        let (long, _) = AUTH.as_bytes().split_at(40);
        let mut started_then_unexpected = frame(false, TEXT, long);
        started_then_unexpected.extend(frame(true, TEXT, b"x"));
        let mut cut = frame(false, TEXT, long);
        cut.extend_from_slice(&frame(true, CONTINUATION, b"rest")[..3]);
        let mut cut_single = frame(true, TEXT, AUTH.as_bytes());
        cut_single.truncate(cut_single.len() - 3);
        let mut unknown_opcode = frame(true, 0x3, AUTH.as_bytes());
        unknown_opcode[0] = 0x83;
        let mut fragmented_ping = frame(false, 0x9, AUTH.as_bytes());
        fragmented_ping[0] = 0x09;
        let big_ping = frame(true, 0x9, format!("{AUTH}{AUTH}{AUTH}").as_bytes());
        let mut unmasked = vec![0x81, AUTH.len() as u8];
        unmasked.extend_from_slice(AUTH.as_bytes());
        vec![
            ("one frame", frame(true, TEXT, AUTH.as_bytes())),
            ("fragmented with a ping between", fragmented),
            ("RSV1 set", rsv1),
            ("binary message open", binary_open),
            ("text message open", text_open),
            ("started, then an unexpected frame", started_then_unexpected),
            ("cut inside a fragment", cut),
            ("cut inside one frame", cut_single),
            ("unknown opcode", unknown_opcode),
            ("fragmented ping", fragmented_ping),
            ("ping over 125 bytes", big_ping),
            ("unmasked", unmasked),
        ]
    }

    #[test]
    fn no_token_reaches_tungstenite() {
        for (what, input) in token_inputs() {
            for piece in [1, 3, 64, 100_000] {
                let (seen, _) = run(&input, piece);
                assert!(!token_reaches_tungstenite(&seen), "{what}, piece {piece}: a token reached tungstenite");
            }
        }
    }

    /// The check above can fail: without the tap (the bytes as they are) every input hands a token
    /// to tungstenite.
    #[test]
    fn the_token_check_sees_tokens_without_the_tap() {
        for (what, input) in token_inputs() {
            assert!(token_reaches_tungstenite(&input), "{what}: the check missed a token");
        }
    }

    #[test]
    fn auth_messages_are_diverted_and_everything_else_arrives_unchanged() {
        let request = r#"{"id":1,"type":"x","data":{"token":"not an auth"}}"#;
        let mut input = Vec::new();
        input.extend(frame(true, TEXT, request.as_bytes()));
        input.extend(frame(true, TEXT, AUTH.as_bytes()));
        input.extend(frame(true, 0x9, b"ping"));
        input.extend(frame(true, BINARY, &[1, 2, 3]));
        // A fragmented auth with a ping between its frames.
        let (a, b) = AUTH.as_bytes().split_at(20);
        input.extend(frame(false, TEXT, a));
        input.extend(frame(true, 0x9, b"p2"));
        input.extend(frame(true, CONTINUATION, b));
        // A fragmented ordinary text, and a fragmented binary message.
        input.extend(frame(false, TEXT, b"hel"));
        input.extend(frame(false, CONTINUATION, b"lo "));
        input.extend(frame(true, CONTINUATION, "wörld".as_bytes()));
        input.extend(frame(false, BINARY, &[9]));
        input.extend(frame(true, CONTINUATION, &[8]));
        input.extend(frame(true, TEXT, MARKER.as_bytes()));
        assert!(token_reaches_tungstenite(&input));
        for piece in [1, 2, 3, 7, 64, 100_000] {
            let (seen, diverted) = run(&input, piece);
            assert!(!token_reaches_tungstenite(&seen), "piece {piece}: a token reached tungstenite");
            assert_eq!(diverted, vec![AUTH.to_string(), AUTH.to_string(), MARKER.to_string()], "piece {piece}");
            let (got, error) = messages(seen);
            assert!(error.is_none(), "piece {piece}: {error:?}");
            assert_eq!(texts(&got), vec![request, MARKER, MARKER, "hello wörld", MARKER], "piece {piece}");
            assert_eq!(got.iter().filter(|m| m.is_ping()).count(), 2);
            let binary: Vec<_> = got.iter().filter(|m| m.is_binary()).map(|m| m.clone().into_data().to_vec()).collect();
            assert_eq!(binary, vec![vec![1, 2, 3], vec![9, 8]]);
        }
    }

    #[test]
    fn sizes_on_the_length_boundaries_pass() {
        for len in [0, 125, 126, 127, 500, LIMIT] {
            let text = "x".repeat(len);
            let (seen, diverted) = run(&frame(true, TEXT, text.as_bytes()), 5);
            assert!(diverted.is_empty());
            let (got, error) = messages(seen);
            assert!(error.is_none(), "{len}: {error:?}");
            assert_eq!(texts(&got), vec![text.clone()]);
            // Fragmented up to the limit.
            let mut input = frame(false, TEXT, &text.as_bytes()[..len / 2]);
            input.extend(frame(true, CONTINUATION, &text.as_bytes()[len / 2..]));
            let (got, error) = messages(run(&input, 3).0);
            assert!(error.is_none(), "{len}: {error:?}");
            assert_eq!(texts(&got), vec!["x".repeat(len)]);
        }
    }

    #[test]
    fn too_big_is_refused_by_tungstenite_without_the_content() {
        let big = AUTH.repeat(LIMIT / AUTH.len() + 1);
        // One frame over the limit.
        let (seen, diverted) = run(&frame(true, TEXT, big.as_bytes()), 64);
        assert!(diverted.is_empty() && !token_reaches_tungstenite(&seen));
        let (_, error) = messages(seen);
        assert!(matches!(error, Some(tungstenite::Error::Capacity(_))), "{error:?}");
        // A fragmented auth over the limit: its first part is never handed over.
        let (a, b) = big.as_bytes().split_at(LIMIT / 2);
        let mut input = frame(false, TEXT, a);
        input.extend(frame(true, CONTINUATION, b));
        let (seen, diverted) = run(&input, 64);
        assert!(diverted.is_empty());
        assert_eq!(seen, text_head(true, LIMIT as u64 + 1), "only the oversized header");
        let (got, error) = messages(seen);
        assert!(got.is_empty() && matches!(error, Some(tungstenite::Error::Capacity(_))), "{error:?}");
        // A fragmented binary message over the limit (each frame under it).
        let mut input = frame(false, BINARY, &[0; 600]);
        input.extend(frame(true, CONTINUATION, &[0; 600]));
        let (_, error) = messages(run(&input, 64).0);
        assert!(matches!(error, Some(tungstenite::Error::Capacity(_))), "{error:?}");
    }

    /// SF4: before `auth` a socket may send only small messages; the limit rises once raised.
    #[test]
    fn the_pre_auth_limit_is_lower_and_can_be_raised() {
        let limit = TapLimit::new(100);
        let text = "y".repeat(200);
        let (seen, _) = run_limited(&frame(true, TEXT, text.as_bytes()), 16, &limit, LIMIT);
        assert_eq!(seen, text_head(true, LIMIT as u64 + 1), "refused on a header over tungstenite's limit");
        assert!(matches!(messages(seen).1, Some(tungstenite::Error::Capacity(_))));
        // A fragmented text message over the small limit.
        let mut input = frame(false, TEXT, &text.as_bytes()[..80]);
        input.extend(frame(true, CONTINUATION, &text.as_bytes()[80..]));
        assert!(matches!(messages(run_limited(&input, 16, &limit, LIMIT).0).1, Some(tungstenite::Error::Capacity(_))));
        // An auth under it passes; after raising, the large message passes too.
        let mut input = frame(true, TEXT, AUTH.as_bytes());
        input.extend(frame(true, TEXT, text.as_bytes()));
        let mut reader = Pieces { data: input, at: 0, piece: 100_000 };
        let diverted = Diverted::default();
        let mut tap = AuthTap::new(&mut reader, limit.clone(), LIMIT, diverted.clone());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut chunk = [0u8; 4096];
        let mut buf = ReadBuf::new(&mut chunk);
        assert!(matches!(Pin::new(&mut tap).poll_read(&mut cx, &mut buf), Poll::Ready(Ok(()))));
        let mut seen = buf.filled().to_vec();
        assert_eq!(diverted.pop().as_deref(), Some(AUTH));
        limit.set(LIMIT);
        loop {
            let mut chunk = [0u8; 4096];
            let mut buf = ReadBuf::new(&mut chunk);
            match Pin::new(&mut tap).poll_read(&mut cx, &mut buf) {
                Poll::Ready(Ok(())) if buf.filled().is_empty() => break,
                Poll::Ready(Ok(())) => seen.extend_from_slice(buf.filled()),
                other => panic!("{other:?}"),
            }
        }
        let (got, error) = messages(seen);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(texts(&got), vec![MARKER.to_string(), text]);
    }

    #[test]
    fn anything_unexpected_is_refused_by_tungstenite_on_the_header() {
        // An unmasked frame: its header alone.
        let mut unmasked = vec![0x81, 5];
        unmasked.extend_from_slice(b"hello");
        let (seen, _) = run(&unmasked, 2);
        assert_eq!(seen, vec![0x81, 0]);
        assert!(messages(seen).1.is_some());
        // A started auth, then an unexpected text frame: neither content is handed over.
        let (a, _) = AUTH.as_bytes().split_at(30);
        let mut input = frame(false, TEXT, a);
        input.extend(frame(true, TEXT, b"x"));
        input.extend(frame(true, TEXT, AUTH.as_bytes()));
        let (seen, diverted) = run(&input, 4);
        assert!(diverted.is_empty() && !token_reaches_tungstenite(&seen));
        let mut expected = text_head(false, 0);
        expected.extend_from_slice(&[0x81, 0x80, 0, 0, 0, 0]);
        assert_eq!(seen, expected, "the empty start, the refused header, nothing after it");
        assert!(matches!(messages(seen).1, Some(tungstenite::Error::Protocol(_))));
        // RSV1 on an auth, an auth while a binary message is open: refused, no content.
        let mut rsv1 = frame(true, TEXT, AUTH.as_bytes());
        rsv1[0] |= 0x40;
        let (seen, diverted) = run(&rsv1, 7);
        assert!(diverted.is_empty());
        assert_eq!(seen, vec![0xC1, 0x80, 0, 0, 0, 0]);
        assert!(matches!(messages(seen).1, Some(tungstenite::Error::Protocol(_))));
        let mut open = frame(false, BINARY, &[1, 2, 3]);
        open.extend(frame(true, TEXT, AUTH.as_bytes()));
        let (seen, diverted) = run(&open, 7);
        assert!(diverted.is_empty() && !token_reaches_tungstenite(&seen));
        let (got, error) = messages(seen);
        assert!(got.is_empty() && matches!(error, Some(tungstenite::Error::Protocol(_))), "{error:?}");
        // A continuation without a start, an unknown opcode, a fragmented ping.
        assert!(messages(run(&frame(true, CONTINUATION, b"x"), 1).0).1.is_some());
        assert!(messages(run(&frame(true, 0x3, b"x"), 1).0).1.is_some());
        assert!(messages(run(&frame(false, 0x9, b"x"), 1).0).1.is_some());
        // The stream ends inside an auth: nothing of it is handed over.
        let mut cut = frame(false, TEXT, a);
        cut.extend_from_slice(&frame(true, CONTINUATION, b"rest")[..3]);
        let (seen, diverted) = run(&cut, 5);
        assert!(diverted.is_empty() && seen.is_empty());
    }
}
