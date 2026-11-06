//! The bulk lane: transfers of any size, sliced into chunks and reassembled.
//!
//! A transfer is a sequence of bulk records with the same id and consecutive
//! sequence numbers: the first has `FIRST` (and `LEN_KNOWN` + the total if
//! the length was declared), the last has `LAST`, an abandoned one ends with
//! `ABORT`. The receiver checks all of it, so a lost, duplicated or foreign
//! chunk, a wrong total, or an abandoned transfer is reported instead of
//! producing wrong data.
//!
//! The transfer state lives in the ends (so the C API can drive it step by
//! step); [`BulkWriter`] and [`BulkReader`] are borrowing wrappers.

use std::io;

use super::lane::{BULK_HDR, Rec, flag};
use super::{BULK, GkReceiver, GkSender, KIND_UNSPECIFIED, broken};
use crate::error;

/// Sender side state of the bulk lane.
#[derive(Debug, Default)]
pub(super) struct TxState {
    next_id: u64,
    cur: Option<Tx>,
}

#[derive(Debug)]
struct Tx {
    id: u64,
    kind: u16,
    seq: u64,
    written: u64,
    total: Option<u64>,
    /// The `FIRST` record was published.
    started: bool,
    /// The `LAST` record was published.
    complete: bool,
}

/// Receiver side state of the bulk lane.
#[derive(Debug, Default)]
pub(super) struct RxState {
    cur: Option<Rx>,
}

#[derive(Debug)]
struct Rx {
    id: u64,
    kind: u16,
    next_seq: u64,
    received: u64,
    total: Option<u64>,
    /// Bytes of the current record's data already read.
    off: usize,
    done: bool,
    /// Abandoned by the reader: discard the rest before the next transfer.
    skipping: bool,
}

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

fn corrupt(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// The sub-header and data of a bulk record.
struct Chunk {
    id: u64,
    seq: u64,
    total: u64,
    data: *const u8,
    len: usize,
}

fn parse(rec: &Rec) -> io::Result<Chunk> {
    if (rec.len as u64) < BULK_HDR {
        return Err(corrupt("bulk record too short"));
    }
    // SAFETY: a published record of `rec.len >= 24` bytes, 8-aligned.
    unsafe {
        let p = rec.ptr.cast::<u64>();
        Ok(Chunk {
            id: p.read(),
            seq: p.add(1).read(),
            total: p.add(2).read(),
            data: rec.ptr.add(BULK_HDR as usize),
            len: rec.len - BULK_HDR as usize,
        })
    }
}

impl GkSender {
    /// Starts a bulk transfer written through the returned [`BulkWriter`]
    /// (`io::Write`), for data of any size. `len` declares the total length
    /// if known (the receiver sees it, and the transfer ends by itself once
    /// that many bytes are written); `None` streams data of unknown length,
    /// ended by [`BulkWriter::finish`]. Fails with
    /// [`io::ErrorKind::InvalidInput`] if a transfer is already in progress.
    pub fn bulk_writer(&mut self, len: Option<u64>) -> io::Result<BulkWriter<'_>> {
        self.bulk_writer_kind(KIND_UNSPECIFIED, len)
    }

    /// Like [`bulk_writer`](Self::bulk_writer), for a transfer of the given
    /// `kind` (the receiver gets it from [`BulkReader::kind`] before
    /// reading any data).
    pub fn bulk_writer_kind(&mut self, kind: u16, len: Option<u64>) -> io::Result<BulkWriter<'_>> {
        let id = self.bulk_begin(kind, len)?;
        Ok(BulkWriter {
            sender: self,
            id,
            paused: false,
        })
    }

    /// Resumes the transfer left open by [`BulkWriter::pause`] (for event
    /// loops that also use the message lane in between). Fails with
    /// [`io::ErrorKind::InvalidInput`] if no transfer is open.
    pub fn resume_bulk(&mut self) -> io::Result<BulkWriter<'_>> {
        let id = self
            .bulk
            .cur
            .as_ref()
            .map(|tx| tx.id)
            .ok_or_else(|| invalid("no bulk transfer in progress"))?;
        Ok(BulkWriter {
            sender: self,
            id,
            paused: false,
        })
    }

    /// Sends `data` (any size) as one bulk transfer and returns its id.
    /// Blocks until all of it is in the ring (even in non-blocking mode),
    /// not until it is received. Fails with [`io::ErrorKind::BrokenPipe`]
    /// once the receiver is gone.
    pub fn send_bulk(&mut self, data: &[u8]) -> io::Result<u64> {
        self.send_bulk_kind(KIND_UNSPECIFIED, data)
    }

    /// Like [`send_bulk`](Self::send_bulk), for a transfer of the given
    /// `kind`.
    pub fn send_bulk_kind(&mut self, kind: u16, data: &[u8]) -> io::Result<u64> {
        let nonblocking = std::mem::replace(&mut self.end.nonblocking, false);
        let r = self
            .bulk_begin(kind, Some(data.len() as u64))
            .and_then(|id| {
                let mut off = 0;
                while off < data.len() {
                    off += self.bulk_write(&data[off..])?;
                }
                self.bulk_finish()?;
                Ok(id)
            });
        if r.is_err() {
            let _ = self.bulk_abort();
        }
        self.end.nonblocking = nonblocking;
        r
    }

    /// Starts a transfer (see [`bulk_writer`](Self::bulk_writer)).
    pub(crate) fn bulk_begin(&mut self, kind: u16, len: Option<u64>) -> io::Result<u64> {
        self.end.check_owner()?;
        if self.bulk.cur.is_some() {
            return Err(invalid("a bulk transfer is already in progress"));
        }
        self.bulk.next_id += 1;
        let id = self.bulk.next_id;
        self.bulk.cur = Some(Tx {
            id,
            kind,
            seq: 0,
            written: 0,
            total: len,
            started: false,
            complete: false,
        });
        Ok(id)
    }

    /// Publishes one bulk record with `data`; `Ok(false)` if non-blocking
    /// and the ring is full.
    fn bulk_record(&mut self, data: &[u8], flags: u32, nonblocking: bool) -> io::Result<bool> {
        let tx = self.bulk.cur.as_ref().expect("transfer in progress");
        let (id, kind, seq, total) = (tx.id, tx.kind, tx.seq, tx.total);
        let len = BULK_HDR + data.len() as u64;
        loop {
            if self.end.peer_closed() {
                return Err(broken());
            }
            let lane = *self.end.lane(BULK);
            if let Some((pad, idx)) = lane.reserve(self.bulk_tail, len) {
                let flags = flags | if total.is_some() { flag::LEN_KNOWN } else { 0 };
                // SAFETY: `len` bytes reserved at `idx`; payload is 8-aligned.
                unsafe {
                    let p = lane.payload(idx);
                    let h = p.cast::<u64>();
                    h.write(id);
                    h.add(1).write(seq);
                    h.add(2).write(total.unwrap_or(0));
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        p.add(BULK_HDR as usize),
                        data.len(),
                    );
                    self.bulk_tail = lane.publish(self.bulk_tail, pad, idx, len, flags, kind);
                }
                if let Some(tx) = self.bulk.cur.as_mut() {
                    tx.seq += 1;
                }
                self.end.wake_peer(BULK);
                return Ok(true);
            }
            let tail = self.bulk_tail;
            match self
                .end
                .wait(BULK, nonblocking, || lane.reserve(tail, len).is_some())
            {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }

    /// Writes data of the current transfer (see [`BulkWriter`]).
    pub(crate) fn bulk_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.end.check_owner()?;
        let tx = self
            .bulk
            .cur
            .as_ref()
            .ok_or_else(|| invalid("no bulk transfer in progress"))?;
        if buf.is_empty() {
            return Ok(0);
        }
        if tx.complete {
            return Err(invalid("the bulk transfer is already complete"));
        }
        if let Some(total) = tx.total {
            if tx.written + buf.len() as u64 > total {
                return Err(invalid("more data than the declared bulk length"));
            }
        }
        let chunk = self.chunk();
        let mut off = 0;
        while off < buf.len() {
            let n = chunk.min(buf.len() - off);
            let tx = self.bulk.cur.as_ref().expect("transfer in progress");
            let mut flags = if tx.started { 0 } else { flag::FIRST };
            let last = tx.total == Some(tx.written + n as u64);
            if last {
                flags |= flag::LAST;
            }
            match self.bulk_record(&buf[off..off + n], flags, self.end.nonblocking) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) if off > 0 => {
                    let _ = e;
                    break;
                }
                Err(e) => return Err(e),
            }
            let tx = self.bulk.cur.as_mut().expect("transfer in progress");
            tx.started = true;
            tx.written += n as u64;
            tx.complete = last;
            off += n;
        }
        if off == 0 {
            return Err(error::would_block());
        }
        Ok(off)
    }

    /// Ends the current transfer (see [`BulkWriter::finish`]).
    pub(crate) fn bulk_finish(&mut self) -> io::Result<()> {
        self.end.check_owner()?;
        let tx = self
            .bulk
            .cur
            .as_ref()
            .ok_or_else(|| invalid("no bulk transfer in progress"))?;
        if !tx.complete {
            if let Some(total) = tx.total {
                if tx.written != total {
                    let _ = self.bulk_abort();
                    return Err(invalid("bulk transfer shorter than its declared length"));
                }
            }
            let flags = flag::LAST | if tx.started { 0 } else { flag::FIRST };
            if !self.bulk_record(&[], flags, self.end.nonblocking)? {
                return Err(error::would_block());
            }
        }
        self.bulk.cur = None;
        Ok(())
    }

    /// Abandons the current transfer: the receiver gets
    /// `ConnectionAborted`. Waits for ring space if needed.
    pub(crate) fn bulk_abort(&mut self) -> io::Result<()> {
        let Some(tx) = self.bulk.cur.as_ref() else {
            return Ok(());
        };
        let r = if tx.started && !tx.complete {
            self.bulk_record(&[], flag::ABORT, false).map(drop)
        } else {
            Ok(())
        };
        self.bulk.cur = None;
        r
    }
}

/// Writes one bulk transfer (see [`GkSender::bulk_writer`]). In blocking
/// mode `write` takes everything; in non-blocking mode it may take part of
/// the data, or fail with [`io::ErrorKind::WouldBlock`] when the ring is
/// full, like any `io::Write`. Call [`finish`](Self::finish) at the end;
/// dropping an unfinished writer abandons the transfer (the receiver gets
/// [`io::ErrorKind::ConnectionAborted`]).
#[derive(Debug)]
pub struct BulkWriter<'a> {
    sender: &'a mut GkSender,
    id: u64,
    paused: bool,
}

impl BulkWriter<'_> {
    /// The transfer's id (also seen by the receiver).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Ends the transfer. With a declared length, fails with
    /// [`io::ErrorKind::InvalidInput`] if fewer bytes were written (the
    /// transfer is then abandoned). In non-blocking mode it may fail with
    /// [`io::ErrorKind::WouldBlock`]; call it again later.
    pub fn finish(&mut self) -> io::Result<()> {
        self.sender.bulk_finish()
    }

    /// Releases the writer but keeps the transfer open, to continue later
    /// with [`GkSender::resume_bulk`] (e.g. to send messages in between).
    pub fn pause(mut self) {
        self.paused = true;
    }
}

impl io::Write for BulkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sender.bulk_write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BulkWriter<'_> {
    fn drop(&mut self) {
        if !self.paused {
            let _ = self.sender.bulk_abort();
        }
    }
}

impl GkReceiver {
    /// Waits for the next transfer and returns a [`BulkReader`]
    /// (`io::Read`) for it. Blocks, or fails with
    /// [`io::ErrorKind::WouldBlock`] in non-blocking mode; fails with
    /// [`io::ErrorKind::BrokenPipe`] once the sender is gone and every
    /// transfer was read. A transfer left open with [`BulkReader::pause`] is
    /// resumed; one whose reader was dropped early is discarded first.
    pub fn bulk_reader(&mut self) -> io::Result<BulkReader<'_>> {
        let resume = self
            .bulk
            .cur
            .as_ref()
            .is_some_and(|rx| !rx.done && !rx.skipping);
        if !resume {
            self.bulk_begin()?;
        }
        Ok(BulkReader {
            rx: self,
            paused: false,
        })
    }

    /// Receives the next transfer into `out` (replacing its contents) and
    /// returns its id. Fails with [`io::ErrorKind::InvalidInput`] if it is
    /// longer than `limit` bytes (the transfer is then discarded).
    pub fn recv_bulk_vec(&mut self, out: &mut Vec<u8>, limit: usize) -> io::Result<u64> {
        self.recv_bulk_vec_kind(out, limit).map(|(_, id)| id)
    }

    /// Like [`recv_bulk_vec`](Self::recv_bulk_vec), also returning the
    /// transfer's kind: `(kind, id)`.
    pub fn recv_bulk_vec_kind(
        &mut self,
        out: &mut Vec<u8>,
        limit: usize,
    ) -> io::Result<(u16, u64)> {
        let (id, total) = self.bulk_begin()?;
        let kind = self.bulk_info().map_or(KIND_UNSPECIFIED, |i| i.3);
        out.clear();
        if let Some(total) = total {
            if total > limit as u64 {
                self.bulk_end();
                return Err(invalid("bulk transfer larger than the limit"));
            }
            out.reserve_exact(total as usize);
        }
        let r = (|| {
            while let Some(len) = self.bulk_chunk(|data| {
                out.extend_from_slice(data);
                data.len()
            })? {
                let _ = len;
                if out.len() > limit {
                    return Err(invalid("bulk transfer larger than the limit"));
                }
            }
            Ok((kind, id))
        })();
        self.bulk_end();
        r
    }

    /// The next record of the bulk lane (waits like other calls).
    fn bulk_rec(&mut self) -> io::Result<Rec> {
        loop {
            if let Some(rec) = self.end.lane(BULK).peek(self.bulk_head)? {
                return Ok(rec);
            }
            let (lane, head, nb) = (*self.end.lane(BULK), self.bulk_head, self.end.nonblocking);
            self.end
                .wait(BULK, nb, || !matches!(lane.peek(head), Ok(None)))?;
        }
    }

    /// Frees the bulk record just consumed.
    fn bulk_release(&mut self, next: u64) {
        self.bulk_head = next;
        self.end.lane(BULK).release(next);
        self.end.wake_peer(BULK);
    }

    /// Starts reading the next transfer: `(id, declared length)`.
    pub(crate) fn bulk_begin(&mut self) -> io::Result<(u64, Option<u64>)> {
        self.end.check_owner()?;
        if let Some(rx) = &self.bulk.cur {
            if !rx.done && !rx.skipping {
                return Err(invalid("a bulk transfer is being read"));
            }
            if rx.skipping && !rx.done {
                // Discard the rest of the abandoned transfer.
                while self.bulk_chunk(|_| ())?.is_some() {}
            }
            self.bulk.cur = None;
        }
        let rec = self.bulk_rec()?;
        let c = parse(&rec)?;
        if rec.flags & flag::FIRST == 0 || c.seq != 0 {
            return Err(corrupt(
                "bulk lane out of sync (expected the start of a transfer)",
            ));
        }
        let total = (rec.flags & flag::LEN_KNOWN != 0).then_some(c.total);
        self.bulk.cur = Some(Rx {
            id: c.id,
            kind: rec.kind,
            next_seq: 0,
            received: 0,
            total,
            off: 0,
            done: false,
            skipping: false,
        });
        Ok((c.id, total))
    }

    /// The current transfer's record, validated; consumes it if it aborts.
    fn bulk_current(&mut self) -> io::Result<Option<(Rec, Chunk)>> {
        let Some(rx) = self.bulk.cur.as_ref() else {
            return Err(invalid("no bulk transfer is being read"));
        };
        if rx.done {
            return Ok(None);
        }
        let (id, kind, seq) = (rx.id, rx.kind, rx.next_seq);
        let rec = match self.bulk_rec() {
            Ok(rec) => rec,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(e),
            Err(e) => {
                self.bulk.cur = None;
                return Err(e);
            }
        };
        let c = parse(&rec)?;
        if c.id != id || c.seq != seq || rec.kind != kind {
            self.bulk.cur = None;
            return Err(corrupt("bulk chunk out of sequence"));
        }
        if rec.flags & flag::ABORT != 0 {
            self.bulk_release(rec.next);
            self.bulk.cur = None;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the sender abandoned the bulk transfer",
            ));
        }
        Ok(Some((rec, c)))
    }

    /// Account for `n` more bytes of the current record; frees it once read.
    fn bulk_advance(&mut self, rec: &Rec, c: &Chunk, n: usize) -> io::Result<()> {
        let rx = self.bulk.cur.as_mut().expect("transfer being read");
        rx.off += n;
        rx.received += n as u64;
        if rx.off < c.len {
            return Ok(());
        }
        rx.off = 0;
        rx.next_seq += 1;
        let last = rec.flags & flag::LAST != 0;
        if last {
            rx.done = true;
        }
        let mismatch = last && rx.total.is_some_and(|t| t != rx.received);
        self.bulk_release(rec.next);
        if mismatch {
            self.bulk.cur = None;
            return Err(corrupt(
                "bulk transfer length differs from its declared length",
            ));
        }
        Ok(())
    }

    /// Lends the unread rest of the current chunk to `f` (in place) and
    /// consumes it. `None` at the end of the transfer.
    pub(crate) fn bulk_chunk<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> io::Result<Option<R>> {
        let mut f = Some(f);
        loop {
            let Some((rec, c)) = self.bulk_current()? else {
                return Ok(None);
            };
            let off = self.bulk.cur.as_ref().map_or(0, |rx| rx.off);
            let n = c.len - off;
            if n == 0 {
                // An empty chunk (e.g. a separate LAST record): skip it.
                self.bulk_advance(&rec, &c, 0)?;
                continue;
            }
            // SAFETY: `n` unread bytes of a published record we own until
            // it is released in `bulk_advance`.
            let r = (f.take().expect("called once"))(unsafe {
                std::slice::from_raw_parts(c.data.add(off), n)
            });
            self.bulk_advance(&rec, &c, n)?;
            return Ok(Some(r));
        }
    }

    /// Reads data of the current transfer into `buf` (`Ok(0)` at its end).
    pub(crate) fn bulk_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some((rec, c)) = self.bulk_current()? else {
                return Ok(0);
            };
            let off = self.bulk.cur.as_ref().map_or(0, |rx| rx.off);
            let n = (c.len - off).min(buf.len());
            // SAFETY: as in `bulk_chunk`.
            buf[..n].copy_from_slice(unsafe { std::slice::from_raw_parts(c.data.add(off), n) });
            self.bulk_advance(&rec, &c, n)?;
            if n > 0 {
                return Ok(n);
            }
        }
    }

    /// Stops reading the current transfer; an unfinished one is discarded
    /// before the next transfer starts.
    pub(crate) fn bulk_end(&mut self) {
        match &mut self.bulk.cur {
            Some(rx) if !rx.done => rx.skipping = true,
            _ => self.bulk.cur = None,
        }
    }

    /// The current transfer: `(id, declared length, bytes read, kind)`.
    pub(crate) fn bulk_info(&self) -> Option<(u64, Option<u64>, u64, u16)> {
        self.bulk
            .cur
            .as_ref()
            .map(|rx| (rx.id, rx.total, rx.received, rx.kind))
    }
}

/// Reads one bulk transfer (see [`GkReceiver::bulk_reader`]): `io::Read`
/// returns `Ok(0)` at its end. Errors: [`io::ErrorKind::ConnectionAborted`]
/// if the sender abandoned it, [`io::ErrorKind::BrokenPipe`] if the sender
/// went away mid-transfer, [`io::ErrorKind::InvalidData`] if the received
/// length differs from the declared one, [`io::ErrorKind::WouldBlock`] in
/// non-blocking mode. Dropping it early discards the rest of the transfer.
#[derive(Debug)]
pub struct BulkReader<'a> {
    rx: &'a mut GkReceiver,
    paused: bool,
}

impl BulkReader<'_> {
    /// Releases the reader but keeps the transfer, to continue later with
    /// [`GkReceiver::bulk_reader`] (e.g. to receive messages in between).
    pub fn pause(mut self) {
        self.paused = true;
    }

    /// The transfer's id (as returned to the sender).
    pub fn id(&self) -> u64 {
        self.rx.bulk_info().map_or(0, |i| i.0)
    }

    /// The transfer's kind (`0` = unspecified), known before any data is
    /// read, e.g. to pick a handler.
    pub fn kind(&self) -> u16 {
        self.rx.bulk_info().map_or(KIND_UNSPECIFIED, |i| i.3)
    }

    /// The declared total length, if the sender declared one.
    pub fn len(&self) -> Option<u64> {
        self.rx.bulk_info().and_then(|i| i.1)
    }

    /// True if the sender declared a length of zero.
    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    /// Bytes read so far.
    pub fn received(&self) -> u64 {
        self.rx.bulk_info().map_or(0, |i| i.2)
    }

    /// Hands every remaining chunk to `f` in place (no copy), in order, and
    /// returns the number of bytes. Each chunk's space is freed when `f`
    /// returns.
    pub fn for_each_chunk(&mut self, mut f: impl FnMut(&[u8])) -> io::Result<u64> {
        let mut total = 0;
        while let Some(n) = self.rx.bulk_chunk(|data| {
            f(data);
            data.len()
        })? {
            total += n as u64;
        }
        Ok(total)
    }
}

impl io::Read for BulkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.rx.bulk_read(buf)
    }
}

impl Drop for BulkReader<'_> {
    fn drop(&mut self) {
        if !self.paused {
            self.rx.bulk_end();
        }
    }
}
