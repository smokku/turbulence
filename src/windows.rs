use std::{cmp::Ordering, collections::VecDeque, num::Wrapping, u32};

pub type StreamPos = Wrapping<u32>;

/// Compare the given wrapping stream positions.
///
/// A value `a` is considered less than `b` if it is faster to get to `a` from `b` by going left
/// than by going right, and `a` is considered greater than `b` if the opposite is true.
///
/// Cannot be used to implement `Ord` because it is not transitive.
pub fn stream_cmp(a: &StreamPos, b: &StreamPos) -> Ordering {
    (b - a).cmp(&(a - b))
}

pub fn stream_lt(a: &StreamPos, b: &StreamPos) -> bool {
    stream_cmp(a, b) == Ordering::Less
}

pub fn stream_gt(a: &StreamPos, b: &StreamPos) -> bool {
    stream_cmp(a, b) == Ordering::Greater
}

pub fn stream_ge(a: &StreamPos, b: &StreamPos) -> bool {
    stream_cmp(a, b) != Ordering::Less
}

#[derive(PartialEq, Eq, Debug)]
pub enum AckResult {
    /// This range was not found
    NotFound,
    /// This range acked more than was sent
    InvalidRange,
    /// This range was fully acked
    Ack,
    /// This range was a partial ack of a previously sent range, and the range from the end of the
    /// provided range to this stream position should be considered nacked.
    PartialAck(StreamPos),
}

/// Coaelesces and buffers outgoing stream data up to a configured window capacity and keeps it
/// available to resend until it is acknowledged from the remote.
pub struct SendWindow {
    // The capacity here is hard-coded for testability.  We could use `buffer.capacity()` here
    // instead, but tests assume that the capacity is the requested capacity, and
    // `VecDeque::with_capacity` only guarantees a minimum capacity.
    capacity: u32,
    buffer: VecDeque<u8>,
    // The stream position of the first byte of the outgoing buffer after the "sent" bytes.
    send_pos: StreamPos,
    // The number of bytes at the beginning of the outgoing buffer that have already been sent, but
    // are being kept in case they need to be retransmitted.
    sent: u32,
    // The set of sent but un-acked stream ranges.  All of these ranges should be non-empty and
    // non-overlapping, and the list should remain sorted in wrap-around stream ordering, and all of
    // the ranges should fall within the "sent" portion of the buffer.
    unacked_ranges: Vec<(StreamPos, StreamPos)>,
}

impl SendWindow {
    pub fn new(capacity: u32, stream_start: StreamPos) -> SendWindow {
        // Any more than this and the unacked list might not be totally ordered.
        assert!(capacity <= u32::MAX / 2);

        SendWindow {
            capacity,
            buffer: VecDeque::with_capacity(capacity as usize),
            send_pos: stream_start,
            sent: 0,
            unacked_ranges: Vec::new(),
        }
    }

    /// The amount of data available to be written
    pub fn write_available(&self) -> u32 {
        self.capacity - self.buffer.len() as u32
    }

    /// Write the given data to the end of the send buffer, up to the available amount to be written.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let amt = (self.capacity as usize - self.buffer.len()).min(data.len());
        self.buffer.extend(&data[0..amt]);
        amt
    }

    /// The stream position of the next byte of data that would be sent with a call to `SendWindow::send`.
    pub fn send_pos(&self) -> StreamPos {
        self.send_pos
    }

    pub fn send_available(&self) -> u32 {
        self.buffer.len() as u32 - self.sent
    }

    /// Send any pending written data up to the size of the provided buffer, and add this sent range
    /// as an unacked range.
    ///
    /// Returns the stream range of the sent data.  Not all of the provided buffer is necessarily
    /// written, only the data from the start of the buffer to the length of the returned stream
    /// range is actually written.  Will not return a zero sized range, if no data is available to
    /// be sent or the provided buffer is empty, will return None.
    pub fn send(&mut self, data: &mut [u8]) -> Option<(StreamPos, StreamPos)> {
        let send_amt = (self.buffer.len() - self.sent as usize).min(data.len()) as u32;
        if send_amt == 0 {
            None
        } else {
            for i in 0..send_amt as usize {
                data[i] = self.buffer[i + self.sent as usize];
            }
            let start = self.send_pos;
            let end = start + Wrapping(send_amt);

            self.sent += send_amt;
            self.send_pos = end;
            self.unacked_ranges.push((start, end));

            Some((start, end))
        }
    }

    /// Returns the stream position after the last contiguously acked sent data.  The stream data
    /// from `unacked_start` to `send_pos` is sent but not yet fully acked, and is retained in the
    /// send buffer.
    pub fn unacked_start(&self) -> StreamPos {
        self.send_pos - Wrapping(self.sent)
    }

    /// Fetches a portion of the unacked region of the send buffer.  Range must be within
    /// [unacked_start, send_pos].
    pub fn get_unacked(&self, start: StreamPos, data: &mut [u8]) {
        let unacked_start = self.unacked_start();
        let buf_start = (start - unacked_start).0 as usize;
        for i in 0..data.len() {
            data[i] = self.buffer[buf_start + i];
        }
    }

    /// Acknowledge the receipt of the given stream range from the remote, and thus potentially free
    /// up send buffer space.
    ///
    /// Acknowledged ranges are allowed to be equal to or shorter than the sent ranges, but they
    /// *must* start with the same stream position.  Acked ranges will be ignored if they are empty
    /// or do not start with the same position as a previously sent, unacked range.
    pub fn ack_range(&mut self, start: StreamPos, end: StreamPos) -> AckResult {
        match self
            .unacked_ranges
            .binary_search_by(|(range_start, _)| stream_cmp(range_start, &start))
        {
            Ok(i) => {
                if stream_gt(&end, &self.unacked_ranges[i].1) {
                    AckResult::InvalidRange
                } else {
                    let unacked_start = self.unacked_start();
                    if end == self.unacked_ranges[i].1 {
                        self.unacked_ranges.remove(i);

                        if start == unacked_start {
                            assert_eq!(i, 0);
                            if self.unacked_ranges.is_empty() {
                                self.buffer.drain(0..self.sent as usize);
                                self.sent = 0;
                            } else {
                                let acked_amt = (self.unacked_ranges[0].0 - start).0;
                                self.buffer.drain(0..acked_amt as usize);
                                self.sent -= acked_amt;
                            }
                        }
                        AckResult::Ack
                    } else {
                        if start == unacked_start {
                            assert_eq!(i, 0);
                            let acked_amt = (end - start).0;
                            self.buffer.drain(0..acked_amt as usize);
                            self.sent -= acked_amt;
                        }

                        self.unacked_ranges[i].0 = end;
                        AckResult::PartialAck(self.unacked_ranges[i].1)
                    }
                }
            }
            Err(_) => AckResult::NotFound,
        }
    }
}

/// Receives stream data up to a configured window capacity, in any order, and combines it into an
/// ordered stream.
pub struct RecvWindow {
    // The capacity here is hard-coded for testability.  We could use `buffer.capacity()` here
    // instead, but tests assume that the capacity is the requested capacity, and
    // `VecDeque::with_capacity` only guarantees a minimum capacity.
    capacity: u32,
    // The current stream position of the first byte of the incoming buffer after the "ready" bytes.
    recv_pos: StreamPos,
    // The number of bytes in the input buffer which are available for reading
    ready: u32,
    buffer: VecDeque<u8>,
    // An ordered list (in wrap-around stream positions) of non-contiguous received regions of data
    // in the buffer that do not connect with the "ready" data.  This is used to receive
    // out-of-ordered data and allow it to be recombined into an in-order stream.
    //
    // The invariants here are:
    // 1) The list must contain non-overlapping, non-"touching" regions.  In other words, the end of
    //    unready region i cannot be the equal to or greater than the start of unready region i + 1.
    // 2) The list must contain no empty regions, the end of any unready region must be strictly
    //    greater than the beginning.
    // 3) The list must not contain regions spanning such a large distance that the wrap-around
    //    ordering of the regions is no longer total.
    unready: Vec<(StreamPos, StreamPos)>,
}

impl RecvWindow {
    pub fn new(capacity: u32, stream_start: StreamPos) -> RecvWindow {
        // Any more than this and the unready list might not be totally ordered.
        assert!(capacity <= u32::MAX / 2);
        RecvWindow {
            capacity,
            recv_pos: stream_start,
            ready: 0,
            buffer: VecDeque::with_capacity(capacity as usize),
            unready: Vec::new(),
        }
    }

    /// The amount of contiguous data available to be read
    pub fn read_available(&self) -> u32 {
        self.ready
    }

    /// Read any ready data off of the beginning of the read buffer and return the number of bytes
    /// read.
    pub fn read(&mut self, data: &mut [u8]) -> usize {
        let read_amt = data.len().min(self.ready as usize);
        for i in 0..read_amt {
            data[i] = self.buffer.pop_front().unwrap();
        }
        self.ready -= read_amt as u32;
        read_amt
    }

    /// The stream position where no more data could be received.  This window will move forward as
    /// data is read.
    pub fn window_end(&self) -> StreamPos {
        self.recv_pos + Wrapping(self.capacity - self.ready)
    }

    /// Receive a new block of data and return the upper bound of the stream range that was
    /// successfully stored.
    ///
    /// If redundant data is received, all redundant data will be returned as successfully stored,
    /// even data that has already been read out.  It will *not* be checked for consistency with
    /// existing data, it will simply be ignored and assumed to be identical.
    ///
    /// The returned upper bound will never be beyond the current window end, any data that falls
    /// beyond the receive window cannot be stored.
    ///
    /// The range formed by the start position and the returned upper bound will never be empty, it
    /// will either be a non-empty range of successfully received data or this method will return
    /// None.  The range formed by the start position and the returned upper bound will also never
    /// be larger than the provided data, it will either be equal to or smaller.
    ///
    /// Received data may not be made immediately available for read if it is not contiguous with
    /// the existing ready data.
    pub fn recv(&mut self, start_pos: StreamPos, data: &[u8]) -> Option<StreamPos> {
        assert!(data.len() <= u32::MAX as usize / 2);

        // This is the stream position at the beginning of the read buffer
        let recv_start_pos = self.recv_pos - Wrapping(self.ready);

        // `recv_end_pos` is the stream position at the end of the maximum capacity of the receive
        // buffer.
        let recv_end_pos = self.recv_pos + Wrapping(self.capacity as u32 - self.ready);

        // `end_pos` is the stream position at the end of the input data
        let end_pos = start_pos + Wrapping(data.len() as u32);

        // If stream positions were strictly ordered this would not be necessary, but this check
        // combined with the assertions that `data.len() <= u32::MAX / 2` and `self.capacity <=
        // u32::MAX / 2` should prevent wrapping issues.
        if stream_gt(&start_pos, &recv_end_pos) {
            return None;
        }

        // `copy_start_pos` is the stream position at either the given `start_pos`, or the current
        // receive position, whichever is greater.  We do not copy data that has already been
        // received, so this is where we will begin copying.
        let copy_start_pos = if stream_gt(&self.recv_pos, &start_pos) {
            self.recv_pos
        } else {
            start_pos
        };

        // We calculate the `end_pos` as being either the previous `end_pos` or the stream position
        // at the maximum capacity of the receive buffer.  We should not read more data than the
        // requested buffer capacity can hold.
        let end_pos = if stream_lt(&end_pos, &recv_end_pos) {
            end_pos
        } else {
            recv_end_pos
        };

        // If we are not copying any new data (the range from `copy_start_pos` to `end_pos` is
        // empty), then we are done.
        if stream_ge(&copy_start_pos, &end_pos) {
            // We should only return and end position if there is actually acknowledged data (it
            // doesn't matter if the data has already been read and we skip copying it).
            if stream_lt(&start_pos, &end_pos) {
                return Some(end_pos);
            } else {
                return None;
            }
        }

        // The index in the destination buffer where we start copying from
        let data_start = (copy_start_pos - start_pos).0 as usize;
        // The index in the receive buffer where we start copying to
        let buf_start = (copy_start_pos - recv_start_pos).0 as usize;
        // The index in the receive buffer where we stop copying
        let buf_end = (end_pos - recv_start_pos).0 as usize;

        assert!(buf_end <= self.capacity as usize);
        self.buffer.resize(self.buffer.len().max(buf_end), 0);
        for i in buf_start..buf_end {
            self.buffer[i] = data[i - buf_start + data_start];
        }

        // Very, very carefully, combine this newly received region with the existing unready
        // regions and maintain all the invariants of the unready list.

        if stream_ge(&self.recv_pos, &start_pos) {
            // If this received region touches the end of the ready block, we need to combine this
            // region with the ready block, and any unready regions that it overlaps with also need
            // to be combined into the ready block.

            let pos = match self
                .unready
                .binary_search_by(|(_, end)| stream_cmp(end, &end_pos))
            {
                Ok(i) => i,
                Err(i) => i,
            };

            let end = if pos == self.unready.len() {
                self.unready.clear();
                end_pos
            } else if end_pos >= self.unready[pos].0 {
                let end = self.unready[pos].1;
                self.unready.drain(0..=pos);
                end
            } else {
                end_pos
            };

            self.ready += (end - self.recv_pos).0;
            self.recv_pos = end;
        } else {
            // If this received region does not touch the end of the ready block, we just need to
            // combine this with the other unready regions to maintain the invariants.  It must be
            // combined with any overlapping unready regions or any unready regions that are exactly
            // next to each other.

            let insert_pos = match self
                .unready
                .binary_search_by(|(_, end)| stream_cmp(end, &start_pos))
            {
                Ok(i) => i,
                Err(i) => i,
            };

            if insert_pos == self.unready.len() {
                self.unready.push((start_pos, end_pos));
            } else if stream_lt(&end_pos, &self.unready[insert_pos].0) {
                self.unready.insert(insert_pos, (start_pos, end_pos));
            } else {
                let start = self.unready[insert_pos].0;
                for i in insert_pos..self.unready.len() {
                    if stream_lt(&end_pos, &self.unready[i].0) {
                        self.unready.drain(insert_pos + 1..i);
                        self.unready[insert_pos].0 = start.min(start_pos);
                        self.unready[insert_pos].1 = end_pos;
                        break;
                    } else if stream_lt(&end_pos, &self.unready[i].1) || i == self.unready.len() - 1
                    {
                        self.unready.drain(insert_pos..i);
                        self.unready[insert_pos].0 = start.min(start_pos);
                        self.unready[insert_pos].1 = self.unready[insert_pos].1.max(end_pos);
                        break;
                    }
                }
            }
        }

        Some(end_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::u32;

    #[test]
    fn test_send_window() {
        let stream_start = Wrapping(u32::MAX - 11);
        let write_data = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut send_data = [0; 16];
        let mut send_window = SendWindow::new(8, stream_start);

        assert_eq!(send_window.write_available(), 8);
        assert_eq!(send_window.send_pos(), stream_start);

        assert_eq!(send_window.write(&write_data[0..4]), 4);
        assert_eq!(send_window.write(&write_data[4..6]), 2);
        assert_eq!(send_window.write(&write_data[6..10]), 2);

        assert_eq!(send_window.send_pos(), stream_start);

        assert_eq!(send_window.send_available(), 8);
        assert_eq!(
            send_window.send(&mut send_data[0..6]),
            Some((stream_start, stream_start + Wrapping(6)))
        );
        for i in 0..6 {
            assert_eq!(send_data[i], i as u8);
        }
        assert_eq!(send_window.send_pos(), stream_start + Wrapping(6));

        assert_eq!(send_window.write_available(), 0);

        assert_eq!(
            send_window.ack_range(stream_start, stream_start + Wrapping(4)),
            AckResult::PartialAck(stream_start + Wrapping(6))
        );

        assert_eq!(send_window.write_available(), 4);
        assert_eq!(send_window.write(&write_data[8..16]), 4);

        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(4), stream_start + Wrapping(6)),
            AckResult::Ack
        );

        assert_eq!(send_window.write_available(), 2);
        assert_eq!(send_window.write(&write_data[12..16]), 2);

        assert_eq!(send_window.send_available(), 8);
        assert_eq!(
            send_window.send(&mut send_data[6..9]),
            Some((stream_start + Wrapping(6), stream_start + Wrapping(9)))
        );
        for i in 6..9 {
            assert_eq!(send_data[i], i as u8);
        }
        assert_eq!(send_window.send_pos(), stream_start + Wrapping(9));

        assert_eq!(send_window.send_available(), 5);
        assert_eq!(
            send_window.send(&mut send_data[9..11]),
            Some((stream_start + Wrapping(9), stream_start + Wrapping(11)))
        );
        for i in 9..11 {
            assert_eq!(send_data[i], i as u8);
        }
        assert_eq!(send_window.send_pos(), stream_start + Wrapping(11));

        assert_eq!(send_window.send_available(), 3);
        assert_eq!(
            send_window.send(&mut send_data[11..16]),
            Some((stream_start + Wrapping(11), stream_start + Wrapping(14)))
        );
        for i in 11..14 {
            assert_eq!(send_data[i], i as u8);
        }
        assert_eq!(send_window.send_pos(), stream_start + Wrapping(14));

        // Ack ranges that error should not affect anything
        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(10), stream_start + Wrapping(11)),
            AckResult::NotFound
        );
        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(11), stream_start + Wrapping(15)),
            AckResult::InvalidRange
        );

        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(11), stream_start + Wrapping(12)),
            AckResult::PartialAck(stream_start + Wrapping(14))
        );
        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(6), stream_start + Wrapping(9)),
            AckResult::Ack
        );

        assert_eq!(send_window.write_available(), 3);
        assert_eq!(send_window.send_pos(), stream_start + Wrapping(14));
        assert_eq!(send_window.write(&write_data[14..16]), 2);

        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(12), stream_start + Wrapping(14)),
            AckResult::Ack
        );
        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(9), stream_start + Wrapping(11)),
            AckResult::Ack
        );

        assert_eq!(send_window.write_available(), 6);

        assert_eq!(send_window.send_available(), 2);
        assert_eq!(
            send_window.send(&mut send_data[14..16]),
            Some((stream_start + Wrapping(14), stream_start + Wrapping(16)))
        );
        for i in 14..16 {
            assert_eq!(send_data[i], i as u8);
        }

        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(14), stream_start + Wrapping(15)),
            AckResult::PartialAck(stream_start + Wrapping(16)),
        );
        assert_eq!(
            send_window.ack_range(stream_start + Wrapping(15), stream_start + Wrapping(16)),
            AckResult::Ack,
        );

        assert_eq!(send_window.write_available(), 8);
    }

    #[test]
    fn test_recv_window() {
        let stream_start = Wrapping(u32::MAX - 29);
        let recv_data = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ];
        let mut read_data = [0; 32];
        let mut recv_window = RecvWindow::new(8, stream_start);

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(8));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(0), &recv_data[0..4]),
            Some(stream_start + Wrapping(4))
        );
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(8));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(2), &recv_data[2..6]),
            Some(stream_start + Wrapping(6))
        );
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(8));

        assert_eq!(recv_window.read(&mut read_data[0..3]), 3);
        assert_eq!(recv_window.read(&mut read_data[3..5]), 2);
        for i in 0..5 {
            assert_eq!(read_data[i], i as u8);
        }

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(13));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(4), &recv_data[4..10]),
            Some(stream_start + Wrapping(10))
        );
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(9), &recv_data[9..15]),
            Some(stream_start + Wrapping(13))
        );
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(13));

        assert_eq!(recv_window.read(&mut read_data[5..10]), 5);
        for i in 5..10 {
            assert_eq!(read_data[i], i as u8);
        }

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(18));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(25), &recv_data[25..30]),
            None
        );
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(15), &recv_data[15..25]),
            Some(stream_start + Wrapping(18)),
        );
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(18));

        assert_eq!(recv_window.read(&mut read_data[10..20]), 3);
        for i in 10..13 {
            assert_eq!(read_data[i], i as u8);
        }

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(21));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(10), &recv_data[10..25]),
            Some(stream_start + Wrapping(21))
        );

        // Redundant receives
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(2), &recv_data[2..10]),
            Some(stream_start + Wrapping(10)),
        );
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(14), &recv_data[14..21]),
            Some(stream_start + Wrapping(21)),
        );
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(20), &recv_data[20..21]),
            Some(stream_start + Wrapping(21)),
        );

        // receives off of end
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(21), &recv_data[21..25]),
            None,
        );
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(22), &recv_data[22..25]),
            None,
        );
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(21), &recv_data[21..21]),
            None,
        );

        assert_eq!(recv_window.read(&mut read_data[13..25]), 8);
        for i in 13..21 {
            assert_eq!(read_data[i], i as u8);
        }

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(29));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(25), &recv_data[25..27]),
            Some(stream_start + Wrapping(27))
        );
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(29));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(21), &recv_data[21..26]),
            Some(stream_start + Wrapping(26))
        );

        assert_eq!(recv_window.read(&mut read_data[21..27]), 6);
        for i in 21..27 {
            assert_eq!(read_data[i], i as u8);
        }

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(35));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(31), &recv_data[31..32]),
            Some(stream_start + Wrapping(32))
        );
        assert_eq!(recv_window.read(&mut read_data[27..32]), 0);
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(35));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(28), &recv_data[28..29]),
            Some(stream_start + Wrapping(29))
        );
        assert_eq!(recv_window.read(&mut read_data[27..32]), 0);
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(35));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(30), &recv_data[30..31]),
            Some(stream_start + Wrapping(31))
        );
        assert_eq!(recv_window.read(&mut read_data[27..32]), 0);
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(35));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(29), &recv_data[29..30]),
            Some(stream_start + Wrapping(30))
        );
        assert_eq!(recv_window.read(&mut read_data[27..32]), 0);
        assert_eq!(recv_window.window_end(), stream_start + Wrapping(35));
        assert_eq!(
            recv_window.recv(stream_start + Wrapping(27), &recv_data[27..28]),
            Some(stream_start + Wrapping(28))
        );

        assert_eq!(recv_window.read(&mut read_data[27..32]), 5);
        for i in 27..32 {
            assert_eq!(read_data[i], i as u8);
        }

        assert_eq!(recv_window.window_end(), stream_start + Wrapping(40));
    }
}
