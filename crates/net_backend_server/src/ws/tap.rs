//! The bytes a client sends, between the socket and tungstenite. tungstenite logs every frame and
//! every message it receives at TRACE through the `log` crate, with the content, and has no setting
//! that turns this off. An `auth` message carries an access token, so the tap takes every `auth`
//! message out of the stream before tungstenite reads it: tungstenite gets [`MARKER`] in its place,
//! and the connection takes the real text from [`Diverted`] when the marker arrives (one entry per
//! marker, in order).
//!
//! Everything else reaches tungstenite unchanged, except a text message sent in several frames: it
//! arrives as one frame with the same content (it may be an `auth`). Anything the tap does not
//! expect (an unmasked frame, reserved bits or opcodes, a frame out of sequence, the end of the
//! stream) makes it hand the rest of the stream over untouched, so tungstenite answers it as
//! before; a text message started before that is replaced by an empty one (it may be an `auth`). A
//! frame or text message over the size limit is refused by tungstenite before it reads the content
//! (close 1009), as without the tap.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// What tungstenite receives in place of an `auth` message. It is an `auth` message itself, so a
/// client that sends this text is diverted like any other.
pub(crate) const MARKER: &str = r#"{"type":"auth","data":"(read by the server, not by tungstenite)"}"#;

/// Bytes read from the socket per call.
const READ_CHUNK: usize = 8 * 1024;

/// Buffers above this size are given back once empty (an idle socket keeps little memory).
const KEEP: usize = 1024;

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

/// A frame header (RFC 6455 section 5.2).
struct Head {
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
    Some(Head { fin: first & 0x80 != 0, reserved: first & 0x70 != 0, opcode: first & 0x0F, mask, len, size })
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
    /// Inside a binary message sent in several frames (its frames pass unchanged).
    binary: bool,
    /// Hand everything over untouched from now on.
    through: bool,
    eof: bool,
    /// The largest frame or message (`ws.max_message_bytes`, tungstenite's limits too).
    limit: usize,
    diverted: Diverted,
}

impl<S> AuthTap<S> {
    pub(crate) fn new(inner: S, limit: usize, diverted: Diverted) -> Self {
        Self { inner, raw: Vec::new(), out: Vec::new(), sent: 0, text: None, binary: false, through: false, eof: false, limit, diverted }
    }

    /// From now on, the stream reaches tungstenite untouched. A text message started before is
    /// replaced by the start of an empty one (its content may be an `auth`).
    fn hand_over(&mut self) {
        if self.text.take().is_some() {
            self.out.extend_from_slice(&text_head(false, 0));
        }
        self.through = true;
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
        let known = matches!(head.opcode, CONTINUATION | TEXT | BINARY | 0x8..=0xA);
        let Some(mask) = head.mask.filter(|_| known && !head.reserved) else {
            self.hand_over();
            return true;
        };
        let pending = self.text.as_ref().map_or(0, Vec::len);
        let len = match usize::try_from(head.len) {
            Ok(len) if len <= self.limit && !(head.opcode == CONTINUATION && self.text.is_some() && pending.saturating_add(len) > self.limit) => len,
            _ => {
                // Too big: tungstenite refuses a frame header over its limit before it reads the
                // content. A started text message is replaced by such a header.
                if self.text.take().is_some() {
                    self.out.extend_from_slice(&text_head(true, self.limit as u64 + 1));
                }
                self.through = true;
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
            0x8..=0xA => self.out.extend_from_slice(&frame),
            BINARY if self.text.is_none() && !self.binary => {
                self.binary = !head.fin;
                self.out.extend_from_slice(&frame);
            }
            CONTINUATION if self.binary => {
                self.binary = !head.fin;
                self.out.extend_from_slice(&frame);
            }
            CONTINUATION if self.text.is_some() => {
                let mut text = self.text.take().unwrap_or_default();
                text.extend(payload.iter().zip(mask.iter().cycle()).map(|(byte, m)| byte ^ m));
                if head.fin {
                    self.text_message(text);
                } else {
                    self.text = Some(text);
                }
            }
            TEXT if self.text.is_none() && !self.binary => {
                let text = unmask(payload, mask);
                if !head.fin {
                    self.text = Some(text);
                } else if is_auth(&text) {
                    self.text_message(text);
                } else {
                    self.out.extend_from_slice(&frame);
                }
            }
            _ => {
                // Out of sequence: tungstenite refuses it.
                self.hand_over();
                self.out.extend_from_slice(&frame);
            }
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
            if this.through {
                if !this.raw.is_empty() {
                    std::mem::swap(&mut this.out, &mut this.raw);
                    continue;
                }
            } else if this.step() {
                continue;
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            let mut chunk = [0u8; READ_CHUNK];
            let mut read = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut read))?;
            if read.filled().is_empty() {
                // The end: a cut frame goes over as it is (tungstenite reports the cut).
                this.eof = true;
                this.hand_over();
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
        let mut reader = Pieces { data: input.to_vec(), at: 0, piece: piece.max(1) };
        let diverted = Diverted::default();
        let mut tap = AuthTap::new(&mut reader, LIMIT, diverted.clone());
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

    const AUTH: &str = r#"{"type":"auth","data":{"token":"nbsa_secret_token_value"}}"#;

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
        for piece in [1, 2, 3, 7, 64, 100_000] {
            let (seen, diverted) = run(&input, piece);
            assert!(!seen.windows(5).any(|w| w == b"nbsa_"), "piece {piece}: a token reached tungstenite");
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
        assert!(diverted.is_empty());
        let (_, error) = messages(seen);
        assert!(matches!(error, Some(tungstenite::Error::Capacity(_))), "{error:?}");
        // A fragmented auth over the limit: its first part is never handed over.
        let (a, b) = big.as_bytes().split_at(LIMIT / 2);
        let mut input = frame(false, TEXT, a);
        input.extend(frame(true, CONTINUATION, b));
        let (seen, diverted) = run(&input, 64);
        assert!(diverted.is_empty());
        let header = text_head(true, LIMIT as u64 + 1);
        assert!(seen.starts_with(&header), "the oversized header comes first");
        assert!(!seen.windows(5).any(|w| w == b"nbsa_"));
        let (got, error) = messages(seen);
        assert!(got.is_empty() && matches!(error, Some(tungstenite::Error::Capacity(_))), "{error:?}");
    }

    #[test]
    fn anything_unexpected_is_handed_over_and_refused_by_tungstenite() {
        // An unmasked frame.
        let mut unmasked = vec![0x81, 5];
        unmasked.extend_from_slice(b"hello");
        let (seen, _) = run(&unmasked, 2);
        assert_eq!(seen, unmasked);
        assert!(messages(seen).1.is_some());
        // A started auth, then an unexpected text frame: the auth's start is not handed over.
        let (a, _) = AUTH.as_bytes().split_at(30);
        let mut input = frame(false, TEXT, a);
        input.extend(frame(true, TEXT, b"x"));
        let (seen, diverted) = run(&input, 4);
        assert!(diverted.is_empty() && !seen.windows(5).any(|w| w == b"nbsa_"));
        assert!(matches!(messages(seen).1, Some(tungstenite::Error::Protocol(_))));
        // A continuation without a start, reserved bits.
        assert!(messages(run(&frame(true, CONTINUATION, b"x"), 1).0).1.is_some());
        let mut reserved = frame(true, TEXT, b"x");
        reserved[0] |= 0x40;
        assert!(messages(run(&reserved, 1).0).1.is_some());
        // The stream ends inside an auth: nothing of it is handed over.
        let mut cut = frame(false, TEXT, a);
        cut.extend_from_slice(&frame(true, CONTINUATION, b"rest")[..3]);
        let (seen, diverted) = run(&cut, 5);
        assert!(diverted.is_empty() && !seen.windows(5).any(|w| w == b"nbsa_"));
    }
}
