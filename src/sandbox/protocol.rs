// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(
    dead_code,
    reason = "D06a proves the protocol before production thumbnail routing uses it"
)]

use std::{
    io::{IoSlice, IoSliceMut},
    os::fd::{BorrowedFd, OwnedFd},
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{FileType, MemfdFlags, SealFlags, fcntl_get_seals, fstat, memfd_create},
    io::{Errno, retry_on_intr},
    net::{
        AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recvmsg,
        sendmsg, socketpair,
    },
};

const MAGIC: [u8; 4] = *b"STTP";
const VERSION: u16 = 1;
pub(crate) const CONTROL_PACKET_BYTES: usize = 48;
pub(crate) const STARTUP_DEADLINE: Duration = Duration::from_secs(2);
pub(crate) const REQUEST_DEADLINE: Duration = Duration::from_secs(12);
pub(crate) const THUMBNAIL_OUTPUT_CAP_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_EDGE: u16 = 256;
const MAX_FDS: usize = 1;
const REQUIRED_OUTPUT_SEALS: SealFlags = SealFlags::SEAL
    .union(SealFlags::SHRINK)
    .union(SealFlags::GROW)
    .union(SealFlags::WRITE);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum MessageType {
    Ready = 1,
    Request = 2,
    Reply = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum Operation {
    ThumbnailPng = 1,
}

impl Operation {
    fn from_wire(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::ThumbnailPng),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum Status {
    Ok = 0,
    DecodeFailed = 1,
    UnsupportedOperation = 2,
    ProtocolFailure = 0xffff,
}

impl Status {
    fn from_wire(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Ok),
            1 => Some(Self::DecodeFailed),
            2 => Some(Self::UnsupportedOperation),
            0xffff => Some(Self::ProtocolFailure),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub(crate) enum Representation {
    Png = 1,
}

impl Representation {
    fn from_wire(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Png),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WireEnvelope {
    pub(crate) message_type: MessageType,
    pub(crate) request_id: u64,
    pub(crate) operation: u16,
    pub(crate) requested_edge: u16,
    pub(crate) status: u16,
    pub(crate) representation: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) stride: u32,
    pub(crate) output_len: u64,
}

impl WireEnvelope {
    pub(crate) fn ready() -> Self {
        Self {
            message_type: MessageType::Ready,
            request_id: 0,
            operation: 0,
            requested_edge: 0,
            status: Status::Ok as u16,
            representation: 0,
            width: 0,
            height: 0,
            stride: 0,
            output_len: 0,
        }
    }

    pub(crate) fn request(request_id: u64, operation: u16, requested_edge: u16) -> Self {
        Self {
            message_type: MessageType::Request,
            request_id,
            operation,
            requested_edge,
            status: Status::Ok as u16,
            representation: 0,
            width: 0,
            height: 0,
            stride: 0,
            output_len: 0,
        }
    }

    pub(crate) fn reply(
        request_id: u64,
        status: Status,
        representation: Representation,
        width: u16,
        height: u16,
        stride: u32,
        output_len: u64,
    ) -> Self {
        Self {
            message_type: MessageType::Reply,
            request_id,
            operation: Operation::ThumbnailPng as u16,
            requested_edge: 0,
            status: status as u16,
            representation: representation as u16,
            width,
            height,
            stride,
            output_len,
        }
    }

    fn encode(self) -> [u8; CONTROL_PACKET_BYTES] {
        let mut out = [0u8; CONTROL_PACKET_BYTES];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6] = self.message_type as u8;
        out[8..16].copy_from_slice(&self.request_id.to_le_bytes());
        out[16..18].copy_from_slice(&self.operation.to_le_bytes());
        out[18..20].copy_from_slice(&self.requested_edge.to_le_bytes());
        out[20..22].copy_from_slice(&self.status.to_le_bytes());
        out[22..24].copy_from_slice(&self.representation.to_le_bytes());
        out[24..26].copy_from_slice(&self.width.to_le_bytes());
        out[26..28].copy_from_slice(&self.height.to_le_bytes());
        out[28..32].copy_from_slice(&self.stride.to_le_bytes());
        out[32..40].copy_from_slice(&self.output_len.to_le_bytes());
        out
    }

    fn decode(input: &[u8]) -> ProtocolResult<Self> {
        if input.len() != CONTROL_PACKET_BYTES {
            return Err(ProtocolError::MalformedPacket);
        }
        if input[0..4] != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let version = u16::from_le_bytes(input[4..6].try_into().expect("version bytes"));
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let message_type = match input[6] {
            1 => MessageType::Ready,
            2 => MessageType::Request,
            3 => MessageType::Reply,
            _ => return Err(ProtocolError::MalformedPacket),
        };
        if input[7] != 0 || input[40..].iter().any(|byte| *byte != 0) {
            return Err(ProtocolError::MalformedPacket);
        }
        Ok(Self {
            message_type,
            request_id: u64::from_le_bytes(input[8..16].try_into().expect("id bytes")),
            operation: u16::from_le_bytes(input[16..18].try_into().expect("op bytes")),
            requested_edge: u16::from_le_bytes(input[18..20].try_into().expect("edge bytes")),
            status: u16::from_le_bytes(input[20..22].try_into().expect("status bytes")),
            representation: u16::from_le_bytes(input[22..24].try_into().expect("repr bytes")),
            width: u16::from_le_bytes(input[24..26].try_into().expect("width bytes")),
            height: u16::from_le_bytes(input[26..28].try_into().expect("height bytes")),
            stride: u32::from_le_bytes(input[28..32].try_into().expect("stride bytes")),
            output_len: u64::from_le_bytes(input[32..40].try_into().expect("len bytes")),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Packet {
    pub(crate) envelope: WireEnvelope,
    pub(crate) fds: Vec<OwnedFd>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkerGeneration(u64);

impl WorkerGeneration {
    pub(crate) fn new(value: u64) -> ProtocolResult<Self> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(ProtocolError::WorkerGenerationMismatch)
    }
}

#[derive(Debug)]
pub(crate) struct ParentSession {
    generation: WorkerGeneration,
    next_request_id: u64,
    active_request: Option<u64>,
}

impl ParentSession {
    pub(crate) fn new(generation: WorkerGeneration) -> Self {
        Self {
            generation,
            next_request_id: 1,
            active_request: None,
        }
    }

    pub(crate) fn begin_request(
        &mut self,
        generation: WorkerGeneration,
        operation: Operation,
        requested_edge: u16,
    ) -> ProtocolResult<WireEnvelope> {
        if generation != self.generation {
            return Err(ProtocolError::WorkerGenerationMismatch);
        }
        if self.active_request.is_some() || requested_edge == 0 || requested_edge > MAX_EDGE {
            return Err(ProtocolError::InvalidState);
        }
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ProtocolError::InvalidState)?;
        self.active_request = Some(request_id);
        Ok(WireEnvelope::request(
            request_id,
            operation as u16,
            requested_edge,
        ))
    }

    pub(crate) fn accept_reply(&mut self, packet: Packet) -> ProtocolResult<JobReply> {
        let request_id = self
            .active_request
            .ok_or(ProtocolError::UnsolicitedPacket)?;
        if packet.envelope.message_type != MessageType::Reply
            || packet.envelope.request_id != request_id
        {
            drop(packet);
            return Err(ProtocolError::OrderingViolation);
        }
        let reply = validate_reply_packet(packet)?;
        self.active_request = None;
        Ok(reply)
    }
}

#[derive(Debug)]
pub(crate) enum JobReply {
    Success(ValidatedOutput),
    JobFailure(Status),
}

#[derive(Debug)]
pub(crate) struct ValidatedOutput {
    fd: OwnedFd,
    metadata: OutputMetadata,
}

impl ValidatedOutput {
    pub(crate) fn metadata(&self) -> OutputMetadata {
        self.metadata
    }

    pub(crate) fn read_all(self) -> ProtocolResult<Vec<u8>> {
        let mut output = Vec::with_capacity(
            usize::try_from(self.metadata.output_len)
                .map_err(|_| ProtocolError::AllocationLimit)?,
        );
        let mut offset = 0;
        while offset < self.metadata.output_len {
            let remaining = usize::try_from((self.metadata.output_len - offset).min(64 * 1024))
                .map_err(|_| ProtocolError::AllocationLimit)?;
            let mut chunk = vec![0u8; remaining];
            let read = retry_on_intr(|| rustix::io::pread(&self.fd, chunk.as_mut_slice(), offset))?;
            if read == 0 {
                return Err(ProtocolError::PeerClosed);
            }
            offset += u64::try_from(read).map_err(|_| ProtocolError::AllocationLimit)?;
            output.extend_from_slice(&chunk[..read]);
        }
        Ok(output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutputMetadata {
    pub(crate) representation: Representation,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) stride: u32,
    pub(crate) output_len: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolError {
    Io(Errno),
    Timeout,
    PeerClosed,
    BadMagic,
    UnsupportedVersion(u16),
    MalformedPacket,
    TruncatedPacket,
    AncillaryTruncated,
    UnexpectedDescriptors,
    MissingDescriptor,
    WrongDescriptorType,
    MissingSeals,
    BadOutputLength,
    OutputTooLarge,
    InvalidMetadata,
    AllocationLimit,
    InvalidState,
    WorkerGenerationMismatch,
    UnsolicitedPacket,
    OrderingViolation,
}

type ProtocolResult<T> = Result<T, ProtocolError>;

impl From<Errno> for ProtocolError {
    fn from(value: Errno) -> Self {
        Self::Io(value)
    }
}

pub(crate) fn control_socketpair() -> ProtocolResult<(OwnedFd, OwnedFd)> {
    socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC.union(SocketFlags::NONBLOCK),
        None,
    )
    .map_err(ProtocolError::Io)
}

pub(crate) fn send_packet(
    socket: BorrowedFd<'_>,
    envelope: WireEnvelope,
    fds: &[BorrowedFd<'_>],
    deadline: Duration,
) -> ProtocolResult<()> {
    if fds.len() > MAX_FDS {
        return Err(ProtocolError::UnexpectedDescriptors);
    }
    let encoded = envelope.encode();
    wait_for(socket, PollFlags::OUT, deadline)?;
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = if fds.is_empty() {
        SendAncillaryBuffer::default()
    } else {
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        if !ancillary.push(SendAncillaryMessage::ScmRights(fds)) {
            return Err(ProtocolError::UnexpectedDescriptors);
        }
        ancillary
    };
    let sent = send_with_retry(socket, &[IoSlice::new(&encoded)], &mut ancillary)?;
    if sent != CONTROL_PACKET_BYTES {
        return Err(ProtocolError::TruncatedPacket);
    }
    Ok(())
}

pub(crate) fn recv_packet(
    socket: BorrowedFd<'_>,
    deadline: Duration,
    expected_fds: usize,
) -> ProtocolResult<Packet> {
    if expected_fds > MAX_FDS {
        return Err(ProtocolError::UnexpectedDescriptors);
    }
    wait_for(socket, PollFlags::IN, deadline)?;
    let mut buffer = [0u8; CONTROL_PACKET_BYTES];
    let mut iov = [IoSliceMut::new(&mut buffer)];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let message = recv_with_retry(
        socket,
        &mut iov,
        &mut ancillary,
        RecvFlags::CMSG_CLOEXEC.union(RecvFlags::TRUNC),
    )?;
    if message.bytes == 0 {
        return Err(ProtocolError::PeerClosed);
    }
    if message.flags.contains(ReturnFlags::TRUNC) || message.bytes != CONTROL_PACKET_BYTES {
        drop(ancillary);
        return Err(ProtocolError::TruncatedPacket);
    }
    if message.flags.contains(ReturnFlags::CTRUNC) {
        drop(ancillary);
        return Err(ProtocolError::AncillaryTruncated);
    }
    let mut fds = Vec::new();
    for item in ancillary.drain() {
        match item {
            RecvAncillaryMessage::ScmRights(iter) => {
                fds.extend(iter);
            }
            _ => return Err(ProtocolError::UnexpectedDescriptors),
        }
    }
    if fds.len() != expected_fds {
        let received = fds.len();
        drop(fds);
        return Err(if received < expected_fds {
            ProtocolError::MissingDescriptor
        } else {
            ProtocolError::UnexpectedDescriptors
        });
    }
    let envelope = WireEnvelope::decode(&buffer)?;
    Ok(Packet { envelope, fds })
}

pub(crate) fn expect_ready(socket: BorrowedFd<'_>) -> ProtocolResult<()> {
    let packet = recv_packet(socket, STARTUP_DEADLINE, 0)?;
    if packet.envelope == WireEnvelope::ready() {
        Ok(())
    } else {
        Err(ProtocolError::OrderingViolation)
    }
}

pub(crate) fn validate_request(packet: Packet) -> ProtocolResult<WorkerRequest> {
    if !packet.fds.is_empty() || packet.envelope.message_type != MessageType::Request {
        return Err(ProtocolError::UnexpectedDescriptors);
    }
    let operation =
        Operation::from_wire(packet.envelope.operation).ok_or(ProtocolError::InvalidMetadata)?;
    if packet.envelope.request_id == 0
        || packet.envelope.requested_edge == 0
        || packet.envelope.requested_edge > MAX_EDGE
        || packet.envelope.status != Status::Ok as u16
        || packet.envelope.representation != 0
        || packet.envelope.width != 0
        || packet.envelope.height != 0
        || packet.envelope.stride != 0
        || packet.envelope.output_len != 0
    {
        return Err(ProtocolError::MalformedPacket);
    }
    Ok(WorkerRequest {
        request_id: packet.envelope.request_id,
        operation,
        requested_edge: packet.envelope.requested_edge,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkerRequest {
    pub(crate) request_id: u64,
    pub(crate) operation: Operation,
    pub(crate) requested_edge: u16,
}

pub(crate) fn unsupported_reply(request_id: u64) -> WireEnvelope {
    WireEnvelope::reply(
        request_id,
        Status::UnsupportedOperation,
        Representation::Png,
        0,
        0,
        0,
        0,
    )
}

pub(crate) fn validate_reply_packet(mut packet: Packet) -> ProtocolResult<JobReply> {
    if packet.envelope.message_type != MessageType::Reply || packet.envelope.request_id == 0 {
        return Err(ProtocolError::MalformedPacket);
    }
    let status = Status::from_wire(packet.envelope.status).ok_or(ProtocolError::MalformedPacket)?;
    if status != Status::Ok {
        if !packet.fds.is_empty() || packet.envelope.output_len != 0 {
            return Err(ProtocolError::UnexpectedDescriptors);
        }
        if status == Status::DecodeFailed || status == Status::UnsupportedOperation {
            return Ok(JobReply::JobFailure(status));
        }
        return Err(ProtocolError::MalformedPacket);
    }
    if packet.fds.len() != 1 {
        return Err(if packet.fds.is_empty() {
            ProtocolError::MissingDescriptor
        } else {
            ProtocolError::UnexpectedDescriptors
        });
    }
    let fd = packet.fds.pop().expect("one fd");
    let metadata = validate_output_metadata(&packet.envelope)?;
    validate_output_fd(fd, metadata).map(JobReply::Success)
}

fn validate_output_metadata(envelope: &WireEnvelope) -> ProtocolResult<OutputMetadata> {
    let representation =
        Representation::from_wire(envelope.representation).ok_or(ProtocolError::InvalidMetadata)?;
    if envelope.operation != Operation::ThumbnailPng as u16
        || representation != Representation::Png
        || envelope.width == 0
        || envelope.height == 0
        || envelope.width > MAX_EDGE
        || envelope.height > MAX_EDGE
    {
        return Err(ProtocolError::InvalidMetadata);
    }
    if envelope.stride != 0 {
        return Err(ProtocolError::InvalidMetadata);
    }
    if envelope.output_len == 0 {
        return Err(ProtocolError::BadOutputLength);
    }
    if envelope.output_len > THUMBNAIL_OUTPUT_CAP_BYTES {
        return Err(ProtocolError::OutputTooLarge);
    }
    usize::try_from(envelope.output_len).map_err(|_| ProtocolError::AllocationLimit)?;
    Ok(OutputMetadata {
        representation,
        width: envelope.width,
        height: envelope.height,
        stride: envelope.stride,
        output_len: envelope.output_len,
    })
}

fn validate_output_fd(fd: OwnedFd, metadata: OutputMetadata) -> ProtocolResult<ValidatedOutput> {
    let stat = fstat(&fd)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ProtocolError::WrongDescriptorType);
    }
    if u64::try_from(stat.st_size).map_err(|_| ProtocolError::BadOutputLength)?
        != metadata.output_len
    {
        return Err(ProtocolError::BadOutputLength);
    }
    let seals = fcntl_get_seals(&fd).map_err(|_| ProtocolError::MissingSeals)?;
    if !seals.contains(REQUIRED_OUTPUT_SEALS) {
        return Err(ProtocolError::MissingSeals);
    }
    Ok(ValidatedOutput { fd, metadata })
}

pub(crate) fn sealed_memfd(name: &str, bytes: &[u8]) -> ProtocolResult<OwnedFd> {
    let fd = memfd_create(name, MemfdFlags::CLOEXEC.union(MemfdFlags::ALLOW_SEALING))?;
    let mut written = 0;
    while written < bytes.len() {
        let count = retry_on_intr(|| rustix::io::write(&fd, &bytes[written..]))?;
        if count == 0 {
            return Err(ProtocolError::PeerClosed);
        }
        written += count;
    }
    rustix::fs::ftruncate(
        &fd,
        u64::try_from(bytes.len()).map_err(|_| ProtocolError::AllocationLimit)?,
    )?;
    rustix::fs::fcntl_add_seals(&fd, REQUIRED_OUTPUT_SEALS)?;
    Ok(fd)
}

fn send_with_retry(
    socket: BorrowedFd<'_>,
    iov: &[IoSlice<'_>],
    ancillary: &mut SendAncillaryBuffer<'_, '_, '_>,
) -> ProtocolResult<usize> {
    loop {
        match sendmsg(socket, iov, ancillary, SendFlags::NOSIGNAL) {
            Ok(sent) => return Ok(sent),
            Err(Errno::INTR) => continue,
            Err(Errno::AGAIN) => return Err(ProtocolError::Timeout),
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
}

fn recv_with_retry(
    socket: BorrowedFd<'_>,
    iov: &mut [IoSliceMut<'_>],
    ancillary: &mut RecvAncillaryBuffer<'_>,
    flags: RecvFlags,
) -> ProtocolResult<rustix::net::RecvMsg> {
    loop {
        match recvmsg(socket, iov, ancillary, flags) {
            Ok(message) => return Ok(message),
            Err(Errno::INTR) => continue,
            Err(Errno::AGAIN) => return Err(ProtocolError::Timeout),
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
}

fn wait_for(socket: BorrowedFd<'_>, events: PollFlags, timeout: Duration) -> ProtocolResult<()> {
    let started = Instant::now();
    loop {
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(ProtocolError::Timeout);
        }
        let remaining = timeout - elapsed;
        let timespec = Timespec {
            tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: remaining.subsec_nanos() as _,
        };
        let mut pollfd = [PollFd::from_borrowed_fd(socket, events)];
        match poll(&mut pollfd, Some(&timespec)) {
            Ok(0) => return Err(ProtocolError::Timeout),
            Ok(_) => {
                let revents = pollfd[0].revents();
                if revents.intersects(events) {
                    return Ok(());
                }
                if revents.intersects(PollFlags::HUP.union(PollFlags::ERR)) {
                    return Err(ProtocolError::PeerClosed);
                }
            }
            Err(Errno::INTR) => continue,
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
}

#[cfg(test)]
mod tests;
