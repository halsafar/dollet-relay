use bytes::{Bytes, BytesMut};

pub const TS_PACKET_SIZE: usize = 188;

/// A null packet, per ISO/IEC 13818-1: sync byte, PID 0x1FFF, no adaptation
/// field, payload-only, stuffed with 0xFF. Decoders discard it by PID, which
/// is exactly what a keepalive needs — it must occupy the wire without
/// reaching the demuxer.
const NULL_PACKET: [u8; TS_PACKET_SIZE] = {
    let mut packet = [0xFFu8; TS_PACKET_SIZE];
    packet[0] = 0x47;
    packet[1] = 0x1F;
    packet[2] = 0xFF;
    packet[3] = 0x10;
    packet
};

pub fn null_packet() -> Bytes {
    Bytes::from_static(&NULL_PACKET)
}

/// Turns an arbitrarily-sliced byte stream into packet-aligned ring chunks.
pub struct Packetizer {
    target: usize,
    buf: BytesMut,
}

impl Packetizer {
    pub fn new(target: usize) -> Self {
        // A chunk that is not a whole number of packets would hand every
        // client a split packet at its boundary.
        let target = (target / TS_PACKET_SIZE).max(1) * TS_PACKET_SIZE;
        Self {
            target,
            buf: BytesMut::with_capacity(target * 2),
        }
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<Bytes> {
        self.buf.extend_from_slice(data);

        // Only whole packets may be published; a trailing fragment waits for
        // the read that completes it. `target` is a packet multiple, so every
        // split below lands on a packet boundary.
        let mut whole = self.buf.len() / TS_PACKET_SIZE * TS_PACKET_SIZE;
        let mut out = Vec::new();
        while whole >= self.target {
            out.push(self.buf.split_to(self.target).freeze());
            whole -= self.target;
        }
        out
    }

    /// Discard everything not yet published. Called on every input teardown,
    /// not just a URL switch: the trailing partial packet belongs to a process
    /// that is gone, and concatenating it onto the next process's first bytes
    /// produces a corrupt packet that breaks audio decoder sync.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_packet_is_a_valid_stuffed_ts_packet() {
        let p = null_packet();
        assert_eq!(p.len(), TS_PACKET_SIZE);
        assert_eq!(p[0], 0x47);
        // PID 0x1FFF with adaptation_field_control = payload only.
        assert_eq!(u16::from_be_bytes([p[1], p[2]]) & 0x1FFF, 0x1FFF);
        assert_eq!(p[3] & 0x30, 0x10);
        assert!(p[4..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn emits_only_target_sized_chunks() {
        let mut p = Packetizer::new(TS_PACKET_SIZE * 4);
        assert!(p.push(&[0u8; TS_PACKET_SIZE * 3]).is_empty());

        let out = p.push(&[0u8; TS_PACKET_SIZE * 5]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| c.len() == TS_PACKET_SIZE * 4));
    }

    #[test]
    fn reassembles_packets_split_across_reads() {
        let mut p = Packetizer::new(TS_PACKET_SIZE);
        let packet: Vec<u8> = (0..TS_PACKET_SIZE).map(|i| i as u8).collect();

        assert!(p.push(&packet[..100]).is_empty());
        let out = p.push(&packet[100..]);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..], &packet[..]);
    }

    #[test]
    fn target_is_rounded_down_to_whole_packets() {
        let mut p = Packetizer::new(TS_PACKET_SIZE * 2 + 7);
        let out = p.push(&[0u8; TS_PACKET_SIZE * 2]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), TS_PACKET_SIZE * 2);
    }

    #[test]
    fn target_below_one_packet_still_emits_one_packet() {
        let mut p = Packetizer::new(10);
        let out = p.push(&[0u8; TS_PACKET_SIZE]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), TS_PACKET_SIZE);
    }

    #[test]
    fn reset_drops_the_partial_packet_so_it_cannot_splice_onto_the_next_input() {
        let mut p = Packetizer::new(TS_PACKET_SIZE);
        assert!(p.push(&[0xAA; 50]).is_empty());
        p.reset();

        let next: Vec<u8> = std::iter::repeat_n(0x47u8, TS_PACKET_SIZE).collect();
        let out = p.push(&next);
        assert_eq!(out.len(), 1);
        assert!(out[0].iter().all(|&b| b == 0x47), "spliced stale bytes");
    }
}
