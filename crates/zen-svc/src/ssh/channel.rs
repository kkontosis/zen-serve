//! The connection protocol (RFC 4254): session channels with `exec`,
//! `shell` and `subsystem`, their windows and their queued output.

use super::wire::{Reader, Writer};
use super::{Conn, Error, Event, QUEUE_MAX, QUEUE_PIECES, Stop, fail, reason};

const MSG_CHANNEL_OPEN: u8 = 90;
const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const MSG_CHANNEL_DATA: u8 = 94;
const MSG_CHANNEL_EXTENDED_DATA: u8 = 95;
const MSG_CHANNEL_EOF: u8 = 96;
const MSG_CHANNEL_CLOSE: u8 = 97;
const MSG_CHANNEL_REQUEST: u8 = 98;
const MSG_CHANNEL_SUCCESS: u8 = 99;
const MSG_CHANNEL_FAILURE: u8 = 100;

const OPEN_ADMINISTRATIVELY_PROHIBITED: u32 = 1;
const OPEN_UNKNOWN_CHANNEL_TYPE: u32 = 3;
const OPEN_RESOURCE_SHORTAGE: u32 = 4;

/// Channels open at once.
pub(crate) const MAX_CHANNELS: usize = 10;
/// The largest data packet the client may send, and the engine's own.
pub(crate) const MAX_DATA: u32 = 32_768;

pub(crate) const EOF_IN: u8 = 1;
pub(crate) const CLOSE_IN: u8 = 2;
pub(crate) const EOF_WANTED: u8 = 4;
pub(crate) const EOF_SENT: u8 = 8;
pub(crate) const CLOSE_WANTED: u8 = 16;
pub(crate) const CLOSE_SENT: u8 = 32;
pub(crate) const FLAGS: u8 = 63;

/// What a channel was asked to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Exec,
    Shell,
    Subsystem,
}

impl Request {
    pub(crate) fn code(self) -> u64 {
        match self {
            Request::Exec => 1,
            Request::Shell => 2,
            Request::Subsystem => 3,
        }
    }

    pub(crate) fn from_code(c: u64) -> Option<Request> {
        match c {
            1 => Some(Request::Exec),
            2 => Some(Request::Shell),
            3 => Some(Request::Subsystem),
            _ => None,
        }
    }
}

/// A channel as [`Conn::channels`] shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelInfo {
    pub ch: u32,
    pub request: Option<Request>,
    /// The client sent EOF.
    pub eof: bool,
    /// The client closed the channel.
    pub closed: bool,
    /// Data not consumed yet.
    pub unread: usize,
    /// Output queued.
    pub queued: usize,
    /// What the client's window still takes.
    pub peer_window: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct Channel {
    pub(crate) id: u32,
    pub(crate) peer: u32,
    pub(crate) peer_window: u32,
    pub(crate) peer_max: u32,
    /// What the client may still send.
    pub(crate) window: u32,
    pub(crate) unread: Vec<u8>,
    /// Pieces of output: (0 data or 1 standard error, bytes).
    pub(crate) queue: Vec<(u8, Vec<u8>)>,
    pub(crate) req: Option<Request>,
    pub(crate) exit: Option<u32>,
    pub(crate) flags: u8,
}

impl Channel {
    /// Closed both ways: dropped at the next input.
    pub(crate) fn gone(&self) -> bool {
        self.flags & (CLOSE_IN | CLOSE_SENT) == CLOSE_IN | CLOSE_SENT
    }

    pub(crate) fn queued(&self) -> usize {
        self.queue.iter().map(|(_, b)| b.len()).sum()
    }
}

impl Conn {
    fn chan(&self, id: u32) -> Option<usize> {
        self.chans.iter().position(|c| c.id == id)
    }

    /// The channel a client message names; one it closed is unknown.
    fn chan_in(&self, id: u32) -> Result<usize, Stop> {
        match self.chan(id) {
            Some(i) if self.chans[i].flags & CLOSE_IN == 0 => Ok(i),
            _ => fail(reason::PROTOCOL_ERROR, "a message for an unknown channel"),
        }
    }

    pub(super) fn on_channel(&mut self, p: &[u8]) -> Result<(), Stop> {
        let mut r = Reader::new(p);
        let typ = r.u8()?;
        if typ == MSG_CHANNEL_OPEN {
            return self.on_open(r);
        }
        let i = self.chan_in(r.u32()?)?;
        match typ {
            MSG_CHANNEL_WINDOW_ADJUST => {
                let n = r.u32()?;
                r.done()?;
                let c = &mut self.chans[i];
                let Some(w) = c.peer_window.checked_add(n) else {
                    return fail(reason::PROTOCOL_ERROR, "a channel window over 2^32 - 1");
                };
                c.peer_window = w;
                self.flush(i);
            }
            MSG_CHANNEL_DATA | MSG_CHANNEL_EXTENDED_DATA => {
                let ext = typ == MSG_CHANNEL_EXTENDED_DATA;
                if ext {
                    r.u32()?;
                }
                let data = r.string()?;
                r.done()?;
                let c = &mut self.chans[i];
                if c.flags & EOF_IN != 0 {
                    return fail(reason::PROTOCOL_ERROR, "channel data after EOF");
                }
                let n = data.len() as u32;
                if n > MAX_DATA || n > c.window {
                    return fail(
                        reason::PROTOCOL_ERROR,
                        "channel data over the window or packet size",
                    );
                }
                c.window -= n;
                if ext || c.flags & CLOSE_SENT != 0 {
                    // Discarded: give the window back.
                    self.adjust(i);
                } else {
                    c.unread.extend_from_slice(data);
                    let ch = c.id;
                    if !self.events.contains(&Event::Data { ch }) {
                        self.events.push(Event::Data { ch });
                    }
                }
            }
            MSG_CHANNEL_EOF => {
                r.done()?;
                let c = &mut self.chans[i];
                c.flags |= EOF_IN;
                self.events.push(Event::Eof { ch: c.id });
            }
            MSG_CHANNEL_CLOSE => {
                r.done()?;
                let c = &mut self.chans[i];
                c.flags = (c.flags | CLOSE_IN | CLOSE_WANTED) & !EOF_WANTED;
                c.queue.clear();
                c.exit = None;
                let ch = c.id;
                self.flush(i);
                self.events.push(Event::Close { ch });
            }
            MSG_CHANNEL_REQUEST => self.on_request(i, r)?,
            MSG_CHANNEL_SUCCESS | MSG_CHANNEL_FAILURE => {}
            _ => return fail(reason::PROTOCOL_ERROR, "an unexpected channel message"),
        }
        Ok(())
    }

    fn on_open(&mut self, mut r: Reader) -> Result<(), Stop> {
        let typ = r.string()?;
        let sender = r.u32()?;
        let window = r.u32()?;
        let max = r.u32()?;
        let refuse = if typ != b"session" {
            Some(OPEN_UNKNOWN_CHANNEL_TYPE)
        } else if self.chans.len() >= MAX_CHANNELS {
            Some(OPEN_RESOURCE_SHORTAGE)
        } else if max == 0 {
            Some(OPEN_ADMINISTRATIVELY_PROHIBITED)
        } else {
            r.done()?;
            None
        };
        if let Some(code) = refuse {
            let m = Writer::new(MSG_CHANNEL_OPEN_FAILURE)
                .u32(sender)
                .u32(code)
                .string(b"")
                .string(b"")
                .finish();
            self.send(m);
            return Ok(());
        }
        let id = (0..).find(|n| self.chan(*n).is_none()).expect("a free id");
        let ours = self.cfg.opts().window;
        self.chans.push(Channel {
            id,
            peer: sender,
            peer_window: window,
            peer_max: max,
            window: ours,
            unread: Vec::new(),
            queue: Vec::new(),
            req: None,
            exit: None,
            flags: 0,
        });
        let m = Writer::new(MSG_CHANNEL_OPEN_CONFIRMATION)
            .u32(sender)
            .u32(id)
            .u32(ours)
            .u32(MAX_DATA)
            .finish();
        self.send(m);
        self.events.push(Event::ChannelOpen { ch: id });
        Ok(())
    }

    fn on_request(&mut self, i: usize, mut r: Reader) -> Result<(), Stop> {
        let typ = r.string()?;
        let want = r.bool()?;
        let c = &self.chans[i];
        let (ch, peer) = (c.id, c.peer);
        let free = c.req.is_none() && c.flags & (EOF_IN | CLOSE_WANTED) == 0;
        let ok = match typ {
            b"exec" if free => {
                let command = r.string()?.to_vec();
                r.done()?;
                self.chans[i].req = Some(Request::Exec);
                self.events.push(Event::Exec { ch, command });
                true
            }
            b"shell" if free => {
                r.done()?;
                self.chans[i].req = Some(Request::Shell);
                self.events.push(Event::Shell { ch });
                true
            }
            b"subsystem" if free => match std::str::from_utf8(r.string()?) {
                Ok(name) => {
                    let name = name.to_owned();
                    r.done()?;
                    self.chans[i].req = Some(Request::Subsystem);
                    self.events.push(Event::Subsystem { ch, name });
                    true
                }
                Err(_) => false,
            },
            _ => false,
        };
        if want {
            let t = if ok {
                MSG_CHANNEL_SUCCESS
            } else {
                MSG_CHANNEL_FAILURE
            };
            self.send(Writer::new(t).u32(peer).finish());
        }
        Ok(())
    }

    /// Gives the client back the window the application has consumed, once
    /// that is half of it.
    pub(super) fn adjust(&mut self, i: usize) {
        if self.out_blocked() {
            return;
        }
        let full = self.cfg.opts().window as usize;
        let c = &self.chans[i];
        if c.flags & (CLOSE_IN | CLOSE_SENT) != 0 {
            return;
        }
        let credit = full.saturating_sub(c.window as usize + c.unread.len());
        if credit >= full / 2 && credit > 0 {
            let peer = c.peer;
            self.chans[i].window += credit as u32;
            let m = Writer::new(MSG_CHANNEL_WINDOW_ADJUST)
                .u32(peer)
                .u32(credit as u32)
                .finish();
            self.send(m);
        }
    }

    /// Sends what the window allows of the queue, then EOF, `exit-status`
    /// and CLOSE when they are due.
    pub(super) fn flush(&mut self, i: usize) {
        if self.out_blocked() {
            return;
        }
        loop {
            let c = &mut self.chans[i];
            let Some((kind, data)) = c.queue.first_mut() else {
                break;
            };
            let n = data
                .len()
                .min(c.peer_window as usize)
                .min(c.peer_max.min(MAX_DATA) as usize);
            if n == 0 {
                return;
            }
            let kind = *kind;
            let chunk: Vec<u8> = data.drain(..n).collect();
            if data.is_empty() {
                c.queue.remove(0);
            }
            self.send_data(i, kind, &chunk);
        }
        let c = &mut self.chans[i];
        let peer = c.peer;
        let mut msgs = Vec::new();
        if c.flags & (EOF_WANTED | EOF_SENT | CLOSE_IN) == EOF_WANTED {
            c.flags |= EOF_SENT;
            msgs.push(Writer::new(MSG_CHANNEL_EOF).u32(peer).finish());
        }
        if let Some(code) = c.exit.take() {
            msgs.push(
                Writer::new(MSG_CHANNEL_REQUEST)
                    .u32(peer)
                    .string(b"exit-status")
                    .bool(false)
                    .u32(code)
                    .finish(),
            );
        }
        if c.flags & (CLOSE_WANTED | CLOSE_SENT) == CLOSE_WANTED {
            c.flags |= CLOSE_SENT;
            msgs.push(Writer::new(MSG_CHANNEL_CLOSE).u32(peer).finish());
        }
        for m in msgs {
            self.send_now(&m);
        }
    }

    /// After our NEWKEYS: what waited for it.
    pub(super) fn flush_all(&mut self) {
        for i in 0..self.chans.len() {
            self.adjust(i);
            self.flush(i);
        }
    }

    /// The channel the application names, if it may still send on it.
    fn chan_out(&self, ch: u32) -> Result<usize, Error> {
        if self.ended() {
            return Err(Error::Closed);
        }
        let i = self.chan(ch).ok_or(Error::NoChannel)?;
        if self.chans[i].flags & (EOF_WANTED | CLOSE_WANTED | CLOSE_IN) != 0 {
            return Err(Error::ChannelEnded);
        }
        Ok(i)
    }

    /// What the client sent on `ch` and the application hasn't consumed.
    pub fn channel_data(&self, ch: u32) -> &[u8] {
        self.chan(ch).map_or(&[], |i| &self.chans[i].unread)
    }

    /// Consumes `n` bytes of [`Conn::channel_data`]; the output is a
    /// WINDOW_ADJUST when one is due.
    pub fn consume(&mut self, ch: u32, n: usize) -> Result<Vec<u8>, Error> {
        let i = self.chan(ch).ok_or(Error::NoChannel)?;
        let c = &mut self.chans[i];
        if n > c.unread.len() {
            return Err(Error::Config(format!(
                "consume({n}) with {} bytes unread",
                c.unread.len()
            )));
        }
        c.unread.drain(..n);
        if !self.ended() {
            self.adjust(i);
        }
        Ok(std::mem::take(&mut self.out))
    }

    /// Writes standard output: how much it took (what the client's window
    /// takes now plus what fits the queue), and the bytes to send.
    pub fn write(&mut self, ch: u32, data: &[u8]) -> Result<(usize, Vec<u8>), Error> {
        self.write_kind(ch, 0, data)
    }

    /// Writes standard error (extended data of type 1).
    pub fn write_stderr(&mut self, ch: u32, data: &[u8]) -> Result<(usize, Vec<u8>), Error> {
        self.write_kind(ch, 1, data)
    }

    fn write_kind(&mut self, ch: u32, kind: u8, data: &[u8]) -> Result<(usize, Vec<u8>), Error> {
        let i = self.chan_out(ch)?;
        let queued: usize = self.chans.iter().map(Channel::queued).sum();
        let pieces: usize = self.chans.iter().map(|c| c.queue.len()).sum();
        // What the window takes now goes out at once (unless output
        // already waits); the rest goes into the queue, as far as it fits.
        let mut sent = 0;
        if !self.out_blocked() && self.chans[i].queue.is_empty() {
            let c = &self.chans[i];
            let now = data.len().min(c.peer_window as usize);
            let max = c.peer_max.min(MAX_DATA) as usize;
            while sent < now {
                let n = (now - sent).min(max);
                self.send_data(i, kind, &data[sent..sent + n]);
                sent += n;
            }
        }
        let c = &mut self.chans[i];
        let merges = c.queue.last().is_some_and(|(k, _)| *k == kind);
        let room = if merges || pieces < QUEUE_PIECES {
            QUEUE_MAX.saturating_sub(queued)
        } else {
            0
        };
        let q = (data.len() - sent).min(room);
        if q > 0 {
            let more = &data[sent..sent + q];
            match c.queue.last_mut() {
                Some((k, b)) if *k == kind => b.extend_from_slice(more),
                _ => c.queue.push((kind, more.to_vec())),
            }
        }
        Ok((sent + q, std::mem::take(&mut self.out)))
    }

    fn send_data(&mut self, i: usize, kind: u8, chunk: &[u8]) {
        let c = &mut self.chans[i];
        c.peer_window -= chunk.len() as u32;
        let peer = c.peer;
        let m = if kind == 0 {
            Writer::new(MSG_CHANNEL_DATA)
                .u32(peer)
                .string(chunk)
                .finish()
        } else {
            Writer::new(MSG_CHANNEL_EXTENDED_DATA)
                .u32(peer)
                .u32(1)
                .string(chunk)
                .finish()
        };
        self.send_now(&m);
    }

    /// Sends `exit-status` after what is queued.
    pub fn exit_status(&mut self, ch: u32, code: u32) -> Result<Vec<u8>, Error> {
        let i = self.chan_out_or_eof(ch)?;
        self.chans[i].exit = Some(code);
        self.flush(i);
        Ok(std::mem::take(&mut self.out))
    }

    /// Sends EOF after what is queued.
    pub fn eof(&mut self, ch: u32) -> Result<Vec<u8>, Error> {
        let i = self.chan_out(ch)?;
        self.chans[i].flags |= EOF_WANTED;
        self.flush(i);
        Ok(std::mem::take(&mut self.out))
    }

    /// Closes the channel after what is queued (and EOF and `exit-status`
    /// if asked for).
    pub fn close(&mut self, ch: u32) -> Result<Vec<u8>, Error> {
        let i = self.chan_out_or_eof(ch)?;
        self.chans[i].flags |= CLOSE_WANTED;
        self.flush(i);
        Ok(std::mem::take(&mut self.out))
    }

    /// Like `chan_out`, but after EOF too.
    fn chan_out_or_eof(&self, ch: u32) -> Result<usize, Error> {
        if self.ended() {
            return Err(Error::Closed);
        }
        let i = self.chan(ch).ok_or(Error::NoChannel)?;
        if self.chans[i].flags & (CLOSE_WANTED | CLOSE_IN) != 0 {
            return Err(Error::ChannelEnded);
        }
        Ok(i)
    }

    /// The open channels.
    pub fn channels(&self) -> Vec<ChannelInfo> {
        self.chans
            .iter()
            .map(|c| ChannelInfo {
                ch: c.id,
                request: c.req,
                eof: c.flags & EOF_IN != 0,
                closed: c.flags & CLOSE_IN != 0,
                unread: c.unread.len(),
                queued: c.queued(),
                peer_window: c.peer_window,
            })
            .collect()
    }
}
