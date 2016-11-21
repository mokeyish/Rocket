use std::io::{self, BufRead, Read, Write, Cursor, BufReader};
use std::path::Path;
use std::fs::File;
use std::time::Duration;
use std::mem::transmute;

use super::data_stream::{DataStream, StreamReader, kill_stream};
use http::hyper::{HyperBodyReader, HyperHttpStream};
use http::hyper::HyperNetworkStream;
use http::hyper::HyperHttpReader::*;

/// Type representing the data in the body of an incoming request.
///
/// This type is the only means by which the body of a request can be retrieved.
/// This type is not usually used directly. Instead, types that implement
/// [FromData](trait.FromData.html) are used via code generation by specifying
/// the `data = "<param>"` route parameter as follows:
///
/// ```rust,ignore
/// #[post("/submit", data = "<var>")]
/// fn submit(var: T) -> ... { ... }
/// ```
///
/// Above, `T` can be any type that implements `FromData`. Note that `Data`
/// itself implements `FromData`.
///
/// # Reading Data
///
/// Data may be read from a `Data` object by calling either the
/// [open](#method.open) or [peek](#method.peek) methods.
///
/// The `open` method consumes the `Data` object and returns the raw data
/// stream. The `Data` object is consumed for safety reasons: consuming the
/// object ensures that holding a `Data` object means that all of the data is
/// available for reading.
///
/// The `peek` method returns a slice containing at most 4096 bytes of buffered
/// body data. This enables partially or fully reading from a `Data` object
/// without consuming the `Data` object.
pub struct Data {
    buffer: Vec<u8>,
    is_done: bool,
    // TODO: This sucks as it depends on a TCPStream. Oh, hyper.
    stream: StreamReader,
    // Ideally we wouldn't have these, but Hyper forces us to.
    position: usize,
    capacity: usize,
}

impl Data {
    /// Returns the raw data stream.
    ///
    /// The stream contains all of the data in the body of the request,
    /// including that in the `peek` buffer. The method consumes the `Data`
    /// instance. This ensures that a `Data` type _always_ represents _all_ of
    /// the data in a request.
    pub fn open(mut self) -> impl BufRead {
        // Swap out the buffer and stream for empty ones so we can move.
        let mut buffer = vec![];
        let mut stream = EmptyReader(self.stream.get_ref().clone());
        ::std::mem::swap(&mut buffer, &mut self.buffer);
        ::std::mem::swap(&mut stream, &mut self.stream);

        // Setup the underlying reader at the correct pointers.
        let mut cursor = Cursor::new(buffer);
        cursor.set_position(self.position as u64);
        let buffered = cursor.take((self.capacity - self.position) as u64);

        // Create the actual DataSteam.
        DataStream {
            network: stream.get_ref().clone(),
            stream: buffered.chain(BufReader::new(stream)),
        }
    }

    #[doc(hidden)]
    pub fn from_hyp(mut h_body: HyperBodyReader) -> Result<Data, &'static str> {
        // FIXME: This is asolutely terrible, thanks to Hyper.

        // Retrieve the underlying HTTPStream from Hyper.
        let mut stream = match h_body.get_ref().get_ref()
                                     .downcast_ref::<HyperHttpStream>() {
            Some(s) => {
                let owned_stream = s.clone();
                let buf_len = h_body.get_ref().get_buf().len() as u64;
                match h_body {
                    SizedReader(_, n) => SizedReader(owned_stream, n - buf_len),
                    EofReader(_) => EofReader(owned_stream),
                    EmptyReader(_) => EmptyReader(owned_stream),
                    ChunkedReader(_, n) =>
                        ChunkedReader(owned_stream, n.map(|k| k - buf_len)),
                }
            },
            None => return Err("Stream is not an HTTP stream!"),
        };

        // Set the read timeout to 5 seconds.
        stream.get_mut().set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // Create the Data object from hyper's buffer.
        let (vec, pos, cap) = h_body.get_mut().take_buf();
        Ok(Data::new(vec, pos, cap, stream))
    }

    /// Retrieve the `peek` buffer.
    ///
    /// The peek buffer contains at most 4096 bytes of the body of the request.
    /// The actual size of the returned buffer varies by web request. The
    /// [peek_complete](#method.peek_complete) can be used to determine if this
    /// buffer contains _all_ of the data in the body of the request.
    #[inline(always)]
    pub fn peek(&self) -> &[u8] {
        &self.buffer[self.position..self.capacity]
    }

    /// Returns true if the `peek` buffer contains all of the data in the body
    /// of the request.
    #[inline(always)]
    pub fn peek_complete(&self) -> bool {
        self.is_done
    }

    /// A helper method to write the body of the request to any `Write` type.
    ///
    /// This method is identical to `io::copy(&mut data.open(), writer)`.
    #[inline(always)]
    pub fn stream_to<W: Write>(self, writer: &mut W) -> io::Result<u64> {
        io::copy(&mut self.open(), writer)
    }

    /// A helper method to write the body of the request to a file at the path
    /// determined by `path`.
    ///
    /// This method is identical to
    /// `io::copy(&mut self.open(), &mut File::create(path)?)`.
    #[inline(always)]
    pub fn stream_to_file<P: AsRef<Path>>(self, path: P) -> io::Result<u64> {
        io::copy(&mut self.open(), &mut File::create(path)?)
    }

    // Creates a new data object with an internal buffer `buf`, where the cursor
    // in the buffer is at `pos` and the buffer has `cap` valid bytes. The
    // remainder of the data bytes can be read from `stream`.
    #[doc(hidden)]
    pub fn new(mut buf: Vec<u8>,
               pos: usize,
               mut cap: usize,
               mut stream: StreamReader)
               -> Data {
        // Make sure the buffer is large enough for the bytes we want to peek.
        const PEEK_BYTES: usize = 4096;
        if buf.len() < PEEK_BYTES {
            trace!("Resizing peek buffer from {} to {}.", buf.len(), PEEK_BYTES);
            buf.resize(PEEK_BYTES, 0);
        }

        // We want to fill the buffer with as many bytes as possible. We also
        // want to record if we reach the EOF while filling the buffer. The
        // buffer already has `cap` bytes. We read up to buf.len() - 1 bytes,
        // and then we try reading 1 more to see if we've reached the EOF.
        trace!("Init buffer cap: {}", cap);
        let buf_len = buf.len();
        let eof = if cap < buf_len {
            // We have room to read into the buffer. Let's do it.
            match stream.read(&mut buf[cap..(buf_len - 1)]) {
                Ok(0) => true,
                Ok(n) => {
                    trace!("Filled peek buf with {} bytes.", n);
                    cap += n;
                    match stream.read(&mut buf[cap..(cap + 1)]) {
                        Ok(n) => {
                            cap += n;
                            n == 0
                        }
                        Err(e) => {
                            error_!("Failed to check stream EOF status: {:?}", e);
                            false
                        }
                    }
                }
                Err(e) => {
                    error_!("Failed to read into peek buffer: {:?}", e);
                    false
                }
            }
        } else {
            // There's no more room in the buffer. Assume there are still bytes.
            false
        };

        trace!("Peek buffer size: {}, remaining: {}", buf_len, buf_len - cap);
        Data {
            buffer: buf,
            stream: stream,
            is_done: eof,
            position: pos,
            capacity: cap,
        }
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        // This is okay since the network stream expects to be shared mutably.
        unsafe {
            let stream: &mut StreamReader = transmute(self.stream.by_ref());
            kill_stream(stream, self.stream.get_mut());
        }
    }
}

