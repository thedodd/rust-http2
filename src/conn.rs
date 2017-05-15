use std::collections::HashMap;
use std::collections::VecDeque;
use std::cmp;

use futures::Future;
use futures::future;
use futures;

use bytes::Bytes;

use tokio_io::io::ReadHalf;
use tokio_io::io::WriteHalf;
use tokio_io::AsyncRead;
use tokio_io::AsyncWrite;
use tokio_io::io as tokio_io;

use error::Error;
use error::ErrorCode;

use solicit::session::StreamState;
use solicit::frame::*;
use solicit::header::*;
use solicit::StreamId;
use solicit::WindowSize;
use solicit::HttpScheme;
use solicit::DEFAULT_SETTINGS;
use solicit::connection::EndStream;
use solicit::connection::HttpConnection;
use solicit::connection::SendFrame;
use solicit::connection::HttpFrame;

use stream_part::*;

use futures_misc::*;

use solicit_misc::*;
use solicit_async::*;


pub enum HttpStreamCommand {
    Headers(Headers, EndStream),
    Data(Bytes, EndStream),
    Rst(ErrorCode),
}

impl HttpStreamCommand {
    fn from(part: HttpStreamPart) -> HttpStreamCommand {
        let end_stream = match part.last {
            true => EndStream::Yes,
            false => EndStream::No,
        };
        match part.content {
            HttpStreamPartContent::Data(data) => {
                HttpStreamCommand::Data(data, end_stream)
            },
            HttpStreamPartContent::Headers(headers) => {
                HttpStreamCommand::Headers(headers, end_stream)
            },
        }
    }
}

pub use resp::Response;


pub enum CommonToWriteMessage {
    TryFlushStream(Option<StreamId>), // flush stream when window increased or new data added
    Write(Vec<u8>),
}


pub struct HttpStreamCommon {
    pub state: StreamState,
    pub out_window_size: WindowSize,
    pub in_window_size: WindowSize,
    pub outgoing: VecDeque<HttpStreamPartContent>,
    // Means nothing will be added to `outgoing`
    pub outgoing_end: Option<ErrorCode>,
}

impl HttpStreamCommon {
    pub fn new(out_window_size: u32) -> HttpStreamCommon {
        HttpStreamCommon {
            state: StreamState::Open,
            in_window_size: WindowSize::new(DEFAULT_SETTINGS.initial_window_size as i32),
            out_window_size: WindowSize::new(out_window_size as i32),
            outgoing: VecDeque::new(),
            outgoing_end: None,
        }
    }

    pub fn close_local(&mut self) {
        trace!("close local");
        self.state = match self.state {
            StreamState::Closed | StreamState::HalfClosedRemote => StreamState::Closed,
            _ => StreamState::HalfClosedLocal,
        };
    }

    pub fn close_remote(&mut self) {
        self.state = match self.state {
            StreamState::Closed | StreamState::HalfClosedLocal => StreamState::Closed,
            _ => StreamState::HalfClosedRemote,
        };
    }

    pub fn pop_outg(&mut self, conn_out_window_size: &mut WindowSize) -> Option<HttpStreamCommand> {
        if self.outgoing.is_empty() {
            return
                if let Some(error_code) = self.outgoing_end {
                    if self.state.is_closed_local() {
                        None
                    } else {
                        self.close_local();
                        Some(match error_code {
                            ErrorCode::NoError => HttpStreamCommand::Data(Bytes::new(), EndStream::Yes),
                            error_code => HttpStreamCommand::Rst(error_code),
                        })
                    }
                } else {
                    None
                };
        }

        let pop_headers =
            if let &HttpStreamPartContent::Headers(..) = self.outgoing.front().unwrap() {
                true
            } else {
                false
            };
        if pop_headers {
            let r = self.outgoing.pop_front().unwrap();
            let last = self.outgoing_end == Some(ErrorCode::NoError) && self.outgoing.is_empty();
            if last {
                self.close_local();
            }
            return Some(HttpStreamCommand::from(HttpStreamPart {
                content: r,
                last: last,
            }))
        }

        if self.out_window_size.size() <= 0 {
            return None
        }

        let mut data =
            if let Some(HttpStreamPartContent::Data(data)) = self.outgoing.pop_front() {
                data
            } else {
                unreachable!()
            };

        // Max of connection and stream window size
        let max_window = cmp::min(self.out_window_size.size(), conn_out_window_size.size());

        if data.len() as usize > max_window as usize {
            trace!("truncating data of len {} to {}", data.len(), max_window);
            let size = max_window as usize;
            let rem = data.split_off(size);
            self.outgoing.push_front(HttpStreamPartContent::Data(rem));
        };

        self.out_window_size.try_decrease(data.len() as i32).unwrap();
        conn_out_window_size.try_decrease(data.len() as i32).unwrap();

        let last = self.outgoing_end == Some(ErrorCode::NoError) && self.outgoing.is_empty();
        if last {
            self.close_local();
        }

        Some(HttpStreamCommand::from(HttpStreamPart {
            content: HttpStreamPartContent::Data(data),
            last: last,
        }))
    }

    pub fn pop_outg_all(&mut self, conn_out_window_size: &mut WindowSize) -> Vec<HttpStreamCommand> {
        let mut r = Vec::new();
        while let Some(p) = self.pop_outg(conn_out_window_size) {
            r.push(p);
        }
        r
    }
}


pub trait HttpStream {
    fn common(&self) -> &HttpStreamCommon;
    fn common_mut(&mut self) -> &mut HttpStreamCommon;
    fn new_data_chunk(&mut self, data: &[u8], last: bool);
    fn rst(&mut self, error_code: ErrorCode);
    fn closed_remote(&mut self);
}


pub struct LoopInnerCommon<S>
    where S : HttpStream,
{
    pub conn: HttpConnection,
    pub streams: HashMap<StreamId, S>,
}


#[derive(Debug)]
pub struct ConnectionStateSnapshot {
    pub streams: HashMap<StreamId, StreamState>,
}




impl<S> LoopInnerCommon<S>
    where S : HttpStream,
{
    pub fn new(scheme: HttpScheme) -> LoopInnerCommon<S> {
        LoopInnerCommon {
            conn: HttpConnection::new(scheme),
            streams: HashMap::new(),
        }
    }

    pub fn get_stream_mut(&mut self, stream_id: StreamId) -> Option<&mut S> {
        self.streams.get_mut(&stream_id)
    }

    pub fn remove_stream(&mut self, stream_id: StreamId) {
        match self.streams.remove(&stream_id) {
            Some(_) => debug!("removed stream: {}", stream_id),
            None => debug!("incorrect request to remove stream: {}", stream_id),
        }
    }

    pub fn remove_stream_if_closed(&mut self, stream_id: StreamId) {
        if self.get_stream_mut(stream_id).expect("unknown stream").common().state == StreamState::Closed {
            self.remove_stream(stream_id);
        }
    }


    pub fn pop_outg_for_stream(&mut self, stream_id: StreamId) -> Option<HttpStreamCommand> {
        let r = {
            if let Some(stream) = self.streams.get_mut(&stream_id) {
                stream.common_mut().pop_outg(&mut self.conn.out_window_size)
            } else {
                None
            }
        };
        if let Some(..) = r {
            self.remove_stream_if_closed(stream_id);
        }
        r
    }

    pub fn pop_outg_for_conn(&mut self) -> Option<(StreamId, HttpStreamCommand)> {
        // TODO: lame
        let stream_ids: Vec<StreamId> = self.streams.keys().cloned().collect();
        for stream_id in stream_ids {
            let r = self.pop_outg_for_stream(stream_id);
            if let Some(r) = r {
                return Some((stream_id, r));
            }
        }
        None
    }

    pub fn pop_outg_all_for_stream(&mut self, stream_id: StreamId) -> Vec<HttpStreamCommand> {
        let mut r = Vec::new();
        while let Some(p) = self.pop_outg_for_stream(stream_id) {
            r.push(p);
        }
        r
    }

    pub fn pop_outg_all_for_conn(&mut self) -> Vec<(StreamId, HttpStreamCommand)> {
        let mut r = Vec::new();
        while let Some(p) = self.pop_outg_for_conn() {
            r.push(p);
        }
        r
    }

    fn write_part(&mut self, target: &mut VecSendFrame, stream_id: StreamId, part: HttpStreamCommand) {
        match part {
            HttpStreamCommand::Data(data, end_stream) => {
                // if client requested end of stream,
                // we must send at least one frame with end stream flag
                if end_stream == EndStream::Yes && data.len() == 0 {
                    // probably should send RST_STREAM
                    let mut frame = DataFrame::with_data(stream_id, Vec::new());
                    frame.set_flag(DataFlag::EndStream);

                    debug!("sending frame {:?}", frame);

                    return target.send_frame(frame).unwrap();
                }

                let mut pos = 0;
                const MAX_CHUNK_SIZE: usize = 8 * 1024;
                while pos < data.len() {
                    let end = cmp::min(data.len(), pos + MAX_CHUNK_SIZE);

                    let end_stream_in_frame =
                        if end == data.len() && end_stream == EndStream::Yes {
                            EndStream::Yes
                        } else {
                            EndStream::No
                        };

                    let mut frame = DataFrame::with_data(stream_id, &data[pos..end]);
                    if end_stream_in_frame == EndStream::Yes {
                        frame.set_flag(DataFlag::EndStream);
                    }

                    debug!("sending frame {:?}", frame);

                    target.send_frame(frame).unwrap();

                    pos = end;
                }
            }
            HttpStreamCommand::Headers(headers, end_stream) => {
                let headers_fragment = self
                    .conn.encoder.encode(headers.0.iter().map(|h| (h.name(), h.value())));

                // For now, sending header fragments larger than 16kB is not supported
                // (i.e. the encoded representation cannot be split into CONTINUATION
                // frames).
                let mut frame = HeadersFrame::new(headers_fragment, stream_id);
                frame.set_flag(HeadersFlag::EndHeaders);

                if end_stream == EndStream::Yes {
                    frame.set_flag(HeadersFlag::EndStream);
                }

                debug!("sending frame {:?}", frame);

                target.send_frame(frame).unwrap();
            }
            HttpStreamCommand::Rst(error_code) => {
                let frame = RstStreamFrame::new(stream_id, error_code);

                debug!("sending frame {:?}", frame);

                target.send_frame(frame).unwrap();
            }
        }
    }

    pub fn pop_outg_all_for_stream_bytes(&mut self, stream_id: StreamId) -> Vec<u8> {
        let mut send = VecSendFrame(Vec::new());
        for part in self.pop_outg_all_for_stream(stream_id) {
            self.write_part(&mut send, stream_id, part);
        }
        send.0
    }

    pub fn pop_outg_all_for_conn_bytes(&mut self) -> Vec<u8> {
        let mut send = VecSendFrame(Vec::new());
        for (stream_id, part) in self.pop_outg_all_for_conn() {
            self.write_part(&mut send, stream_id, part);
        }
        send.0
    }

    pub fn dump_state(&self) -> ConnectionStateSnapshot {
        ConnectionStateSnapshot {
            streams: self.streams.iter().map(|(&k, s)| (k, s.common().state)).collect(),
        }
    }
}


pub trait LoopInner: 'static {
    type LoopHttpStream : HttpStream;

    fn common(&mut self) -> &mut LoopInnerCommon<Self::LoopHttpStream>;

    /// Send a frame back to the network
    /// Must not be data frame
    fn send_frame<R : FrameIR>(&mut self, frame: R) {
        let mut send_buf = VecSendFrame(Vec::new());
        send_buf.send_frame(frame).unwrap();
        self.send_common(CommonToWriteMessage::Write(send_buf.0))
    }

    fn send_common(&mut self, message: CommonToWriteMessage);

    fn out_window_increased(&mut self, stream_id: Option<StreamId>) {
        self.send_common(CommonToWriteMessage::TryFlushStream(stream_id))
    }

    /// Sends an SETTINGS Frame with ack set to acknowledge seeing a SETTINGS frame from the peer.
    fn ack_settings(&mut self) {
        self.send_frame(SettingsFrame::new_ack());
    }

    fn process_headers_frame(&mut self, frame: HeadersFrame) {
        let headers = self.common().conn.decoder
            .decode(&frame.header_fragment())
            .map_err(Error::CompressionError).unwrap(); // TODO: do not panic
        let headers = Headers(headers.into_iter().map(|h| Header::new(h.0, h.1)).collect());

        let end_stream = if frame.is_end_of_stream() { EndStream::Yes } else { EndStream::No };

        self.process_headers(frame.stream_id, end_stream, headers);
    }

    fn process_headers(&mut self, stream_id: StreamId, end_stream: EndStream, headers: Headers);

    fn process_settings_global(&mut self, frame: SettingsFrame) {
        if frame.is_ack() {
            return;
        }

        let mut out_window_increased = false;

        for setting in frame.settings {
            if let HttpSetting::InitialWindowSize(new_size) = setting {
                let old_size = self.common().conn.peer_settings.initial_window_size;
                let delta = (new_size as i32) - (old_size as i32);

                if delta != 0 {
                    for (_, s) in &mut self.common().streams {
                        // In addition to changing the flow-control window for streams
                        // that are not yet active, a SETTINGS frame can alter the initial
                        // flow-control window size for streams with active flow-control windows
                        // (that is, streams in the "open" or "half-closed (remote)" state).
                        // When the value of SETTINGS_INITIAL_WINDOW_SIZE changes,
                        // a receiver MUST adjust the size of all stream flow-control windows
                        // that it maintains by the difference between the new value
                        // and the old value.
                        s.common_mut().out_window_size.0 += delta;
                    }

                    if !self.common().streams.is_empty() && delta > 0 {
                        out_window_increased = true;
                    }
                }
            }

            self.common().conn.peer_settings.apply(setting);
        }

        self.ack_settings();

        if out_window_increased {
            self.out_window_increased(None);
        }
    }

    fn process_stream_window_update_frame(&mut self, frame: WindowUpdateFrame) {
        {
            match self.common().get_stream_mut(frame.get_stream_id()) {
                Some(stream) => {
                    stream.common_mut().out_window_size.try_increase(frame.increment())
                        .expect("failed to increment stream window");
                }
                None => {
                    // 6.9
                    // WINDOW_UPDATE can be sent by a peer that has sent a frame bearing the
                    // END_STREAM flag.  This means that a receiver could receive a
                    // WINDOW_UPDATE frame on a "half-closed (remote)" or "closed" stream.
                    // A receiver MUST NOT treat this as an error (see Section 5.1).
                    debug!("WINDOW_UPDATE of unknown stream: {}", frame.get_stream_id());
                }
            }
        }
        self.out_window_increased(Some(frame.get_stream_id()));
    }

    fn process_conn_window_update(&mut self, frame: WindowUpdateFrame) {
        self.common().conn.out_window_size.try_increase(frame.increment())
            .expect("failed to increment conn window");
        self.out_window_increased(None);
    }

    fn process_rst_stream_frame(&mut self, frame: RstStreamFrame) {
        let stream = self.common().get_stream_mut(frame.get_stream_id())
            .expect(&format!("stream not found: {}", frame.get_stream_id()));
        stream.rst(frame.error_code());
    }

    fn process_data_frame(&mut self, frame: DataFrame) {
        let stream_id = frame.get_stream_id();

        self.common().conn.decrease_in_window(frame.payload_len())
            .expect("failed to decrease conn win");

        let increment_conn =
            // TODO: need something better
            if self.common().conn.in_window_size() < (DEFAULT_SETTINGS.initial_window_size / 2) as i32 {
                let increment = DEFAULT_SETTINGS.initial_window_size;
                self.common().conn.in_window_size.try_increase(increment).expect("failed to increase");

                Some(increment)
            } else {
                None
            };

        let increment_stream = {
            let stream = self.common().get_stream_mut(frame.get_stream_id())
                .expect(&format!("stream not found: {}", frame.get_stream_id()));

            stream.common_mut().in_window_size.try_decrease(frame.payload_len() as i32)
                .expect("failed to decrease stream win");

            let increment_stream =
                if stream.common_mut().in_window_size.size() < (DEFAULT_SETTINGS.initial_window_size / 2) as i32 {
                    let increment = DEFAULT_SETTINGS.initial_window_size;
                    stream.common_mut().in_window_size.try_increase(increment).expect("failed to increase");

                    Some(increment)
                } else {
                    None
                };

            stream.new_data_chunk(&frame.data.as_ref(), frame.is_end_of_stream());

            increment_stream
        };

        if let Some(increment_conn) = increment_conn {
            self.send_frame(WindowUpdateFrame::for_connection(increment_conn));
        }

        if let Some(increment_stream) = increment_stream {
            self.send_frame(WindowUpdateFrame::for_stream(stream_id, increment_stream));
        }
    }

    fn process_ping(&mut self, frame: PingFrame) {
        if frame.is_ack() {

        } else {
            self.send_frame(PingFrame::new_ack(frame.opaque_data()));
        }
    }

    fn process_goaway(&mut self, _frame: GoawayFrame) {
        // TODO: After all streams end, close the connection.
    }

    fn process_conn_frame(&mut self, frame: HttpFrameConn) {
        match frame {
            HttpFrameConn::Settings(f) => self.process_settings_global(f),
            HttpFrameConn::Ping(f) => self.process_ping(f),
            HttpFrameConn::Goaway(f) => self.process_goaway(f),
            HttpFrameConn::WindowUpdate(f) => self.process_conn_window_update(f),
        }
    }

    fn process_stream_frame(&mut self, frame: HttpFrameStream) {
        let stream_id = frame.get_stream_id();
        let end_of_stream = frame.is_end_of_stream();

        let close_local = match frame {
            HttpFrameStream::RstStream(..) => true,
            _ => false,
        };

        match frame {
            HttpFrameStream::Data(data) => self.process_data_frame(data),
            HttpFrameStream::Headers(headers) => self.process_headers_frame(headers),
            HttpFrameStream::RstStream(rst) => self.process_rst_stream_frame(rst),
            HttpFrameStream::WindowUpdate(window_update) => self.process_stream_window_update_frame(window_update),
            HttpFrameStream::Continuation(_continuation) => unreachable!("must be joined with HEADERS before that"),
        };
        if end_of_stream {
            self.close_remote(stream_id);
        }
        if close_local {
            self.close_local(stream_id);
        }
    }

    fn process_http_frame(&mut self, frame: HttpFrame) {
        // TODO: decode headers
        debug!("received frame: {:?}", frame);
        match HttpFrameClassified::from(frame) {
            HttpFrameClassified::Conn(f) => self.process_conn_frame(f),
            HttpFrameClassified::Stream(f) => self.process_stream_frame(f),
            HttpFrameClassified::Unknown(_f) => {},
        }
    }

    fn close_remote(&mut self, stream_id: StreamId) {
        debug!("close remote: {}", stream_id);

        // RST_STREAM and WINDOW_UPDATE can be received when we have no stream
        {
            if let Some(stream) = self.common().get_stream_mut(stream_id) {
                stream.common_mut().close_remote();
                stream.closed_remote();
            } else {
                return;
            }
        }

        self.common().remove_stream_if_closed(stream_id);
    }

    fn close_local(&mut self, stream_id: StreamId) {
        debug!("close local: {}", stream_id);

        {
            if let Some(stream) = self.common().get_stream_mut(stream_id) {
                stream.common_mut().close_local();
                //stream.closed_local();
            } else {
                return;
            }
        }

        self.common().remove_stream_if_closed(stream_id);
    }
}


pub struct ReadLoopData<I, N>
    where
        I : AsyncRead + 'static,
        N : LoopInner,
{
    pub read: ReadHalf<I>,
    pub inner: TaskRcMut<N>,
}

pub struct WriteLoopData<I, N>
    where
        I : AsyncWrite + 'static,
        N : LoopInner,
{
    pub write: WriteHalf<I>,
    pub inner: TaskRcMut<N>,
}

pub struct CommandLoopData<N>
    where
        N : LoopInner,
{
    pub inner: TaskRcMut<N>,
}


impl<I, N> ReadLoopData<I, N>
    where
        I : AsyncRead + AsyncWrite + Send + 'static,
        N : LoopInner,
{
    /// Recv a frame from the network
    fn recv_http_frame(self) -> HttpFuture<(Self, HttpFrame)> {
        let ReadLoopData { read, inner } = self;
        Box::new(recv_http_frame_join_cont(read)
            .map(|(read, frame)| (ReadLoopData { read: read, inner: inner }, frame)))
    }

    fn read_process_frame(self) -> HttpFuture<Self> {
        Box::new(self.recv_http_frame()
            .and_then(move |(lp, frame)| lp.process_http_frame(frame)))
    }

    pub fn run(self) -> HttpFuture<()> {
        let future = future::loop_fn(self, |lp| {
            lp.read_process_frame().map(future::Loop::Continue::<(), _>)
        });

        Box::new(future)
    }

    fn process_http_frame(self, frame: HttpFrame) -> HttpFuture<Self> {
        self.inner.with(move |inner| {
            inner.process_http_frame(frame);
        });
        Box::new(futures::finished(self))
    }

}

impl<I, N> WriteLoopData<I, N>
    where
        I : AsyncWrite + Send + 'static,
        N : LoopInner,
{
    pub fn write_all(self, buf: Vec<u8>) -> HttpFuture<Self> {
        let WriteLoopData { write, inner } = self;

        Box::new(tokio_io::write_all(write, buf)
            .map(move |(write, _)| WriteLoopData { write: write, inner: inner })
            .map_err(Error::from))
    }

    fn with_inner<G, R>(&self, f: G) -> R
        where G: FnOnce(&mut N) -> R
    {
        self.inner.with(f)
    }

    pub fn send_outg_stream(self, stream_id: StreamId) -> HttpFuture<Self> {
        let bytes = self.with_inner(|inner| {
            inner.common().pop_outg_all_for_stream_bytes(stream_id)
        });

        self.write_all(bytes)
    }

    fn send_outg_conn(self) -> HttpFuture<Self> {
        let bytes = self.with_inner(|inner| {
            inner.common().pop_outg_all_for_conn_bytes()
        });

        self.write_all(bytes)
    }

    pub fn process_common(self, common: CommonToWriteMessage) -> HttpFuture<Self> {
        match common {
            CommonToWriteMessage::TryFlushStream(None) => self.send_outg_conn(),
            CommonToWriteMessage::TryFlushStream(Some(stream_id)) => self.send_outg_stream(stream_id),
            CommonToWriteMessage::Write(buf) => self.write_all(buf),
        }
    }
}

impl<N> CommandLoopData<N>
    where
        N : LoopInner,
{
}