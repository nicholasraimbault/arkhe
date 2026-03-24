//! Tag encoding for io_uring user_data.
//!
//! Every SQE submitted to the ring carries a u64 user_data tag that identifies
//! what produced the completion. The io_uring event loop decodes this tag to
//! dispatch to the correct system.
//!
//! Layout: upper 8 bits = discriminant, lower 56 bits = ServiceId (or 0).
//!
//! Zero unsafe in this file.

use io_uring::opcode;
use io_uring::types::{Fd, Timespec};
use std::os::fd::AsRawFd;

const TAG_SIGNAL: u64 = 0;
const TAG_PIDFD: u64 = 1;
const TAG_INOTIFY: u64 = 2;
const TAG_SPLICE: u64 = 3;
const TAG_ACCEPT: u64 = 4;
const TAG_PSI: u64 = 5;
const TAG_RESTART: u64 = 6;
const TAG_STOP_TIMEOUT: u64 = 7;
const TAG_DEPS_POLL: u64 = 8;

const ID_MASK: u64 = (1 << 56) - 1;

/// Decoded tag from a completion queue entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Signal,
    Pidfd(usize),
    Inotify,
    Splice(usize),
    Accept(usize),
    Psi(usize),
    Restart(usize),
    StopTimeout(usize),
    DepsPoll,
}

/// Encode a Tag into a u64 for io_uring user_data.
pub fn encode_tag(tag: Tag) -> u64 {
    match tag {
        Tag::Signal => TAG_SIGNAL << 56,
        Tag::Pidfd(id) => (TAG_PIDFD << 56) | (id as u64 & ID_MASK),
        Tag::Inotify => TAG_INOTIFY << 56,
        Tag::Splice(id) => (TAG_SPLICE << 56) | (id as u64 & ID_MASK),
        Tag::Accept(id) => (TAG_ACCEPT << 56) | (id as u64 & ID_MASK),
        Tag::Psi(id) => (TAG_PSI << 56) | (id as u64 & ID_MASK),
        Tag::Restart(id) => (TAG_RESTART << 56) | (id as u64 & ID_MASK),
        Tag::StopTimeout(id) => (TAG_STOP_TIMEOUT << 56) | (id as u64 & ID_MASK),
        Tag::DepsPoll => TAG_DEPS_POLL << 56,
    }
}

/// Decode a u64 user_data back into a Tag.
pub fn decode_tag(user_data: u64) -> Tag {
    let discriminant = user_data >> 56;
    let id = (user_data & ID_MASK) as usize;
    match discriminant {
        TAG_SIGNAL => Tag::Signal,
        TAG_PIDFD => Tag::Pidfd(id),
        TAG_INOTIFY => Tag::Inotify,
        TAG_SPLICE => Tag::Splice(id),
        TAG_ACCEPT => Tag::Accept(id),
        TAG_PSI => Tag::Psi(id),
        TAG_RESTART => Tag::Restart(id),
        TAG_STOP_TIMEOUT => Tag::StopTimeout(id),
        TAG_DEPS_POLL => Tag::DepsPoll,
        _ => Tag::Signal,
    }
}

/// Build a multishot poll SQE for the given fd and tag (POLLIN).
pub fn build_poll_multishot(fd: &impl AsRawFd, tag: Tag) -> io_uring::squeue::Entry {
    build_poll_multishot_mask(fd, libc::POLLIN as u32, tag)
}

/// Build a multishot poll SQE with a custom event mask.
pub fn build_poll_multishot_mask(
    fd: &impl AsRawFd,
    mask: u32,
    tag: Tag,
) -> io_uring::squeue::Entry {
    opcode::PollAdd::new(Fd(fd.as_raw_fd()), mask)
        .multi(true)
        .build()
        .user_data(encode_tag(tag))
}

/// Build a one-shot timeout SQE. The Timespec pointer must remain valid
/// until the CQE is reaped (caller manages lifetime via sys::alloc_timespec).
pub fn build_timeout(ts: *const Timespec, tag: Tag) -> io_uring::squeue::Entry {
    opcode::Timeout::new(ts).build().user_data(encode_tag(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trip() {
        for id in [0, 1, 42, (1 << 56) - 1] {
            assert_eq!(decode_tag(encode_tag(Tag::Pidfd(id))), Tag::Pidfd(id));
            assert_eq!(decode_tag(encode_tag(Tag::Splice(id))), Tag::Splice(id));
            assert_eq!(decode_tag(encode_tag(Tag::Accept(id))), Tag::Accept(id));
            assert_eq!(decode_tag(encode_tag(Tag::Psi(id))), Tag::Psi(id));
            assert_eq!(decode_tag(encode_tag(Tag::Restart(id))), Tag::Restart(id));
            assert_eq!(
                decode_tag(encode_tag(Tag::StopTimeout(id))),
                Tag::StopTimeout(id)
            );
        }
        assert_eq!(decode_tag(encode_tag(Tag::Signal)), Tag::Signal);
        assert_eq!(decode_tag(encode_tag(Tag::Inotify)), Tag::Inotify);
        assert_eq!(decode_tag(encode_tag(Tag::DepsPoll)), Tag::DepsPoll);
    }

    #[test]
    fn tag_discriminants_are_distinct() {
        let tags = [
            encode_tag(Tag::Signal),
            encode_tag(Tag::Pidfd(0)),
            encode_tag(Tag::Inotify),
            encode_tag(Tag::Splice(0)),
            encode_tag(Tag::Accept(0)),
            encode_tag(Tag::Psi(0)),
            encode_tag(Tag::Restart(0)),
            encode_tag(Tag::StopTimeout(0)),
            encode_tag(Tag::DepsPoll),
        ];
        for i in 0..tags.len() {
            for j in (i + 1)..tags.len() {
                assert_ne!(tags[i] >> 56, tags[j] >> 56);
            }
        }
    }
}
