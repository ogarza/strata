// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    io::{IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::AsFd,
    thread,
    time::Duration,
};

use rustix::{
    fs::{MemfdFlags, SealFlags, memfd_create},
    net::{
        RecvAncillaryBuffer, RecvFlags, ReturnFlags, SendAncillaryBuffer, SendAncillaryMessage,
        SendFlags, recvmsg, sendmsg,
    },
};

use super::*;

const TEST_DEADLINE: Duration = Duration::from_secs(1);

#[test]
fn round_trips_and_multiple_sequential_requests() {
    let (parent, worker) = control_socketpair().expect("socketpair");
    let worker_thread = thread::spawn(move || {
        send_packet(worker.as_fd(), WireEnvelope::ready(), &[], TEST_DEADLINE).expect("ready");
        for expected_id in 1..=3 {
            let request = validate_request(
                recv_packet(worker.as_fd(), TEST_DEADLINE, 0).expect("request packet"),
            )
            .expect("request");
            assert_eq!(request.request_id, expected_id);
            assert_eq!(request.operation, Operation::ThumbnailPng);
            assert_eq!(request.requested_edge, 64);
            let bytes = [expected_id as u8; 16];
            let fd = sealed_memfd("reply", &bytes).expect("sealed output");
            send_packet(
                worker.as_fd(),
                WireEnvelope::reply(
                    request.request_id,
                    Status::Ok,
                    Representation::Png,
                    32,
                    16,
                    0,
                    bytes.len() as u64,
                ),
                &[fd.as_fd()],
                TEST_DEADLINE,
            )
            .expect("reply");
        }
    });

    expect_ready(parent.as_fd()).expect("ready packet");
    let generation = WorkerGeneration::new(1).expect("generation");
    let mut session = ParentSession::new(generation);
    for value in 1..=3 {
        let request = session
            .begin_request(generation, Operation::ThumbnailPng, 64)
            .expect("begin");
        send_packet(parent.as_fd(), request, &[], TEST_DEADLINE).expect("send request");
        let reply = session
            .accept_reply(recv_packet(parent.as_fd(), TEST_DEADLINE, 1).expect("reply"))
            .expect("accept reply");
        let JobReply::Success(output) = reply else {
            panic!("expected success");
        };
        assert_eq!(output.metadata().width, 32);
        assert_eq!(output.metadata().height, 16);
        assert_eq!(
            output.read_all().expect("read output"),
            vec![value as u8; 16]
        );
    }
    worker_thread.join().expect("worker thread");
}

#[test]
fn pdf_thumbnail_operation_is_a_valid_bounded_request() {
    let (parent, worker) = control_socketpair().expect("socketpair");
    let generation = WorkerGeneration::new(3).expect("generation");
    let mut session = ParentSession::new(generation);
    let request = session
        .begin_request(generation, Operation::ThumbnailPdf, 128)
        .expect("begin pdf request");
    send_packet(parent.as_fd(), request, &[], TEST_DEADLINE).expect("send pdf request");

    let request = validate_request(
        recv_packet(worker.as_fd(), TEST_DEADLINE, 0).expect("pdf request packet"),
    )
    .expect("pdf request");

    assert_eq!(request.operation, Operation::ThumbnailPdf);
    assert_eq!(request.requested_edge, 128);
}

#[test]
fn unknown_opcode_is_job_failure_and_loop_accepts_next_request() {
    let (parent, worker) = control_socketpair().expect("socketpair");
    let worker_thread = thread::spawn(move || {
        send_packet(worker.as_fd(), WireEnvelope::ready(), &[], TEST_DEADLINE).expect("ready");
        let bad = recv_packet(worker.as_fd(), TEST_DEADLINE, 0).expect("bad request");
        assert_eq!(
            validate_request(bad).expect_err("unknown opcode"),
            ProtocolError::InvalidMetadata
        );
        send_packet(worker.as_fd(), unsupported_reply(1), &[], TEST_DEADLINE)
            .expect("unsupported reply");
        let request =
            validate_request(recv_packet(worker.as_fd(), TEST_DEADLINE, 0).expect("valid request"))
                .expect("valid request");
        let fd = sealed_memfd("reply", b"png").expect("sealed output");
        send_packet(
            worker.as_fd(),
            WireEnvelope::reply(
                request.request_id,
                Status::Ok,
                Representation::Png,
                1,
                1,
                0,
                3,
            ),
            &[fd.as_fd()],
            TEST_DEADLINE,
        )
        .expect("success reply");
    });

    expect_ready(parent.as_fd()).expect("ready");
    let generation = WorkerGeneration::new(7).expect("generation");
    let mut session = ParentSession::new(generation);
    let unknown = session
        .begin_request(generation, Operation::ThumbnailPng, 64)
        .expect("begin unknown");
    send_packet(
        parent.as_fd(),
        WireEnvelope {
            operation: 999,
            ..unknown
        },
        &[],
        TEST_DEADLINE,
    )
    .expect("send unknown");
    assert_eq!(
        match session
            .accept_reply(recv_packet(parent.as_fd(), TEST_DEADLINE, 0).expect("failure reply"))
            .expect("job failure")
        {
            JobReply::JobFailure(status) => status,
            JobReply::Success(_) => panic!("unexpected success"),
        },
        Status::UnsupportedOperation
    );
    let valid = session
        .begin_request(generation, Operation::ThumbnailPng, 64)
        .expect("begin valid");
    send_packet(parent.as_fd(), valid, &[], TEST_DEADLINE).expect("send valid");
    assert!(matches!(
        session.accept_reply(recv_packet(parent.as_fd(), TEST_DEADLINE, 1).expect("success")),
        Ok(JobReply::Success(_))
    ));
    worker_thread.join().expect("worker thread");
}

#[test]
fn malformed_truncated_version_id_status_and_ordering_are_rejected() {
    assert_eq!(
        WireEnvelope::decode(&[0; 4]),
        Err(ProtocolError::MalformedPacket)
    );

    let mut bad_magic = WireEnvelope::ready().encode();
    bad_magic[0] = b'X';
    assert_eq!(
        WireEnvelope::decode(&bad_magic),
        Err(ProtocolError::BadMagic)
    );

    let mut bad_version = WireEnvelope::ready().encode();
    bad_version[4..6].copy_from_slice(&99u16.to_le_bytes());
    assert_eq!(
        WireEnvelope::decode(&bad_version),
        Err(ProtocolError::UnsupportedVersion(99))
    );

    let (a, b) = control_socketpair().expect("socketpair");
    rustix::net::send(b.as_fd(), &[1, 2, 3], SendFlags::NOSIGNAL).expect("send truncated");
    assert_eq!(
        recv_packet(a.as_fd(), TEST_DEADLINE, 0).expect_err("truncated packet"),
        ProtocolError::TruncatedPacket
    );

    let generation = WorkerGeneration::new(1).expect("generation");
    let mut session = ParentSession::new(generation);
    assert_eq!(
        session.begin_request(
            WorkerGeneration::new(2).expect("generation"),
            Operation::ThumbnailPng,
            64
        ),
        Err(ProtocolError::WorkerGenerationMismatch)
    );
    assert!(
        session
            .begin_request(generation, Operation::ThumbnailPng, 64)
            .is_ok()
    );
    assert_eq!(
        session.begin_request(generation, Operation::ThumbnailPng, 64),
        Err(ProtocolError::InvalidState)
    );
    assert!(matches!(
        session.accept_reply(Packet {
            envelope: WireEnvelope::reply(99, Status::Ok, Representation::Png, 1, 1, 0, 1),
            fds: Vec::new(),
        }),
        Err(ProtocolError::OrderingViolation)
    ));

    let bad_status = Packet {
        envelope: WireEnvelope {
            status: 77,
            ..WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, 1)
        },
        fds: Vec::new(),
    };
    assert_eq!(
        validate_reply_packet(bad_status).expect_err("bad status"),
        ProtocolError::MalformedPacket
    );
}

#[test]
fn fd_count_cases_and_ancillary_truncation_are_rejected_and_close_descriptors() {
    let (a, b) = control_socketpair().expect("socketpair");
    let fd = sealed_memfd("extra", b"x").expect("memfd");
    send_packet(
        b.as_fd(),
        WireEnvelope::ready(),
        &[fd.as_fd()],
        TEST_DEADLINE,
    )
    .expect("send extra fd");
    assert_eq!(
        recv_packet(a.as_fd(), TEST_DEADLINE, 0).expect_err("extra descriptor"),
        ProtocolError::UnexpectedDescriptors
    );
    drop((a, b, fd));
    assert_eq!(named_memfd_count("extra"), 0);

    let (a, b) = control_socketpair().expect("socketpair");
    let fd = sealed_memfd("truncated", b"x").expect("memfd");
    send_raw_with_fd(b.as_fd(), WireEnvelope::ready(), fd.as_fd());
    let mut buffer = [0u8; CONTROL_PACKET_BYTES];
    let mut iov = [IoSliceMut::new(&mut buffer)];
    let mut tiny = [MaybeUninit::uninit(); 1];
    let mut ancillary = RecvAncillaryBuffer::new(&mut tiny);
    let message = recvmsg(
        a.as_fd(),
        &mut iov,
        &mut ancillary,
        RecvFlags::CMSG_CLOEXEC.union(RecvFlags::TRUNC),
    )
    .expect("recvmsg");
    assert!(message.flags.contains(ReturnFlags::CTRUNC));
}

#[test]
fn output_validation_rejects_missing_seals_types_lengths_caps_and_metadata() {
    assert_eq!(
        validate_reply_packet(Packet {
            envelope: WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, 1),
            fds: Vec::new(),
        })
        .expect_err("missing descriptor"),
        ProtocolError::MissingDescriptor
    );

    let unsealed = memfd_create(
        "unsealed",
        MemfdFlags::CLOEXEC.union(MemfdFlags::ALLOW_SEALING),
    )
    .expect("memfd");
    rustix::io::write(&unsealed, b"abc").expect("write");
    assert_eq!(
        validate_reply_packet(Packet {
            envelope: WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, 3),
            fds: vec![unsealed],
        })
        .expect_err("missing seals"),
        ProtocolError::MissingSeals
    );

    let wrong_len = sealed_memfd("wrong-len", b"abc").expect("sealed");
    assert_eq!(
        validate_reply_packet(Packet {
            envelope: WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, 2),
            fds: vec![wrong_len],
        })
        .expect_err("wrong length"),
        ProtocolError::BadOutputLength
    );

    let too_large = sealed_memfd("too-large", b"x").expect("sealed");
    assert_eq!(
        validate_reply_packet(Packet {
            envelope: WireEnvelope::reply(
                1,
                Status::Ok,
                Representation::Png,
                1,
                1,
                0,
                THUMBNAIL_OUTPUT_CAP_BYTES + 1,
            ),
            fds: vec![too_large],
        })
        .expect_err("too large"),
        ProtocolError::OutputTooLarge
    );

    for envelope in [
        WireEnvelope::reply(1, Status::Ok, Representation::Png, 0, 1, 0, 1),
        WireEnvelope::reply(1, Status::Ok, Representation::Png, MAX_EDGE + 1, 1, 0, 1),
        WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 4, 1),
        WireEnvelope {
            representation: 99,
            ..WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, 1)
        },
    ] {
        let fd = sealed_memfd("bad-meta", b"x").expect("sealed");
        assert_eq!(
            validate_reply_packet(Packet {
                envelope,
                fds: vec![fd]
            })
            .expect_err("invalid metadata"),
            ProtocolError::InvalidMetadata
        );
    }

    let directory = tempfile::tempdir().expect("tempdir");
    let dir = rustix::fs::open(
        directory.path(),
        rustix::fs::OFlags::RDONLY.union(rustix::fs::OFlags::CLOEXEC),
        rustix::fs::Mode::empty(),
    )
    .expect("open directory");
    assert_eq!(
        validate_reply_packet(Packet {
            envelope: WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, 1),
            fds: vec![dir],
        })
        .expect_err("wrong descriptor type"),
        ProtocolError::WrongDescriptorType
    );
}

#[test]
fn peer_closure_eagain_bounded_waits_and_allocation_bounds() {
    let (a, b) = control_socketpair().expect("socketpair");
    drop(b);
    assert_eq!(
        recv_packet(a.as_fd(), TEST_DEADLINE, 0).expect_err("peer closure"),
        ProtocolError::PeerClosed
    );

    let (a, _b) = control_socketpair().expect("socketpair");
    assert_eq!(
        recv_packet(a.as_fd(), Duration::from_millis(1), 0).expect_err("timeout"),
        ProtocolError::Timeout
    );

    let mut envelope = WireEnvelope::reply(1, Status::Ok, Representation::Png, 1, 1, 0, u64::MAX);
    assert_eq!(
        validate_output_metadata(&envelope),
        Err(ProtocolError::OutputTooLarge)
    );
    envelope.output_len = THUMBNAIL_OUTPUT_CAP_BYTES;
    assert!(validate_output_metadata(&envelope).is_ok());
}

#[test]
fn raw_rgba_output_requires_exact_stride_and_length() {
    let bytes = vec![0u8; 8];
    let fd = sealed_memfd("raw-output", &bytes).expect("sealed raw output");
    let reply = validate_reply_packet(Packet {
        envelope: WireEnvelope::reply(1, Status::Ok, Representation::Rgba8, 1, 2, 4, 8),
        fds: vec![fd],
    })
    .expect("valid raw output");
    let JobReply::Success(output) = reply else {
        panic!("success expected");
    };
    assert_eq!(output.metadata().representation, Representation::Rgba8);
    assert_eq!(output.metadata().stride, 4);
    assert_eq!(output.read_all().expect("read raw output"), bytes);

    let fd = sealed_memfd("raw-bad-length", &[0u8; 8]).expect("sealed raw output");
    assert_eq!(
        validate_reply_packet(Packet {
            envelope: WireEnvelope::reply(1, Status::Ok, Representation::Rgba8, 1, 2, 4, 4),
            fds: vec![fd],
        })
        .expect_err("wrong raw length"),
        ProtocolError::BadOutputLength
    );
}

#[test]
fn high_entropy_maximum_size_output_and_requested_edge_succeed() {
    let mut bytes = vec![0u8; THUMBNAIL_OUTPUT_CAP_BYTES as usize];
    let mut state = 0x1234_5678_9abc_def0u64;
    for byte in &mut bytes {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        *byte = (state >> 32) as u8;
    }
    let fd = sealed_memfd("max-output", &bytes).expect("sealed max output");
    let reply = validate_reply_packet(Packet {
        envelope: WireEnvelope::reply(
            42,
            Status::Ok,
            Representation::Png,
            MAX_EDGE,
            MAX_EDGE,
            0,
            THUMBNAIL_OUTPUT_CAP_BYTES,
        ),
        fds: vec![fd],
    })
    .expect("validated output");
    let JobReply::Success(output) = reply else {
        panic!("success expected");
    };
    assert_eq!(output.metadata().width, MAX_EDGE);
    assert_eq!(output.metadata().height, MAX_EDGE);
    assert_eq!(output.read_all().expect("read all"), bytes);
}

#[test]
fn disposable_sealed_fixture_can_be_created_without_paths_or_uris() {
    let fixture = sealed_memfd("input-fixture", b"fixture bytes").expect("sealed fixture");
    let seals = rustix::fs::fcntl_get_seals(&fixture).expect("seals");
    assert!(
        seals.contains(
            SealFlags::SEAL
                .union(SealFlags::SHRINK)
                .union(SealFlags::GROW)
                .union(SealFlags::WRITE)
        )
    );
}

fn send_raw_with_fd(socket: impl AsFd, envelope: WireEnvelope, fd: impl AsFd) {
    let encoded = envelope.encode();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let fds = [fd.as_fd()];
    assert!(ancillary.push(SendAncillaryMessage::ScmRights(&fds)));
    let sent = sendmsg(
        socket.as_fd(),
        &[IoSlice::new(&encoded)],
        &mut ancillary,
        SendFlags::NOSIGNAL,
    )
    .expect("sendmsg");
    assert_eq!(sent, CONTROL_PACKET_BYTES);
}

fn named_memfd_count(name: &str) -> usize {
    let needle = format!("/memfd:{name}");
    std::fs::read_dir("/proc/self/fd")
        .expect("fd dir")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| target.to_string_lossy().starts_with(&needle))
        .count()
}
