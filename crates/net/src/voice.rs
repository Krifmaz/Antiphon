//! The voice codec: Opus, with the bitrate set by the link (spec §2).
//!
//! Until this module, call audio went out as raw 32-bit samples -- about
//! 1.5 Mbit/s for one voice, which no poor mobile connection can carry.
//! Opus is the codec built for exactly this: speech from 6 kbit/s up, with
//! its own in-band forward error correction (FEC) and discontinuous
//! transmission (DTX) for silence. The implementation is `mousiki`, a pure
//! Rust port of the reference `libopus`, so the Windows, Android and iOS
//! builds still need no C toolchain.
//!
//! # Tiers
//!
//! [`TIERS`] is the ladder the rate controller ([`crate::rate`]) moves the
//! sender along, from 32 kbit/s super-wideband down to the floor: 6 kbit/s
//! mediumband (6 kHz of audio) in 60 ms packets. That floor is about as low as speech stays
//! intelligible -- below it Opus is a robotic smear -- and the longer
//! packet matters as much as the bitrate. Each packet pays ~65 bytes of
//! headers (this module's 5, the frame's 14, the AEAD tag's 16, UDP and
//! IPv4's 28), which at 20 ms packets is 26 kbit/s before a single bit of
//! voice. At 60 ms it is under 9. [`wire_kbps`] gives the total. (Opus
//! treats the floor's 6 kbit/s as a target and lands nearer 7 to 8 on
//! continuous speech, so in practice the floor is 15 to 17 kbit/s on the
//! wire while talking, and close to nothing in silence.)
//!
//! Super-wideband (12 kHz of audio) is the ceiling rather than fullband:
//! it is all a voice needs, and `mousiki`'s fullband CELT path trips one
//! of its own internal checks. At the other end, the band is capped at
//! mediumband rather than narrowband, because `mousiki`'s forced
//! narrowband mode comes out as clipped noise (measured; see the tests).
//!
//! # Wire format (version 2)
//!
//! ```text
//! [version: u8 = 2]
//! [sequence: u32 BE]   // application frame number; see `fec` for why
//! [opus packet...]     // the rest of the datagram
//! ```
//!
//! The backup copy of the previous frame that version 1 carried
//! explicitly ([`crate::fec`]) is now Opus's own in-band FEC, inside the
//! packet: the receiver asks the decoder for it when exactly the previous
//! frame is missing. Version 1 payloads are still decoded, so a peer on an
//! older build is heard -- although it cannot hear this one.
//!
//! # Silence
//!
//! With DTX on, Opus marks silent frames as not worth sending. They are
//! skipped *without* advancing the sequence number, so the receiver sees
//! no gap to conceal or ask to have resent: exactly what already happens
//! between push-to-talk presses.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mousiki::{Application, Bandwidth, Bitrate, Channels, Decoder, Encoder, FrameDuration, Signal};

use crate::fec;

/// The payload version this module writes.
pub const VERSION: u8 = 2;

/// Version byte and sequence number.
const HEADER_LEN: usize = 1 + 4;

/// Opus's own sample rates. A device at any other rate (44.1 kHz,
/// typically) is resampled to 48 kHz on the way in and back on the way
/// out.
const OPUS_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// The largest Opus packet (RFC 6716: 1275 bytes per frame, up to three
/// frames in a 60 ms packet), with room to spare.
const MAX_PACKET: usize = 4_000;

/// The longest frame a packet can hold: 120 ms at 48 kHz.
const MAX_FRAME_SAMPLES: usize = 5_760;

/// An Opus packet this short is DTX's "nothing to send".
const DTX_PACKET_MAX: usize = 2;

/// Bytes each voice packet costs on the wire beyond the Opus packet:
/// this module's header (5), the protocol frame header (14), the AEAD tag
/// (16), and UDP over IPv4 (8 + 20). A relay, the VPN or IPv6 add more.
pub const WIRE_OVERHEAD_BYTES: u32 = HEADER_LEN as u32 + 14 + 16 + 8 + 20;

/// How much audio a tier keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    /// 6 kHz of audio: a little better than a phone line. (Opus's 4 kHz
    /// narrowband is not offered: `mousiki` decodes it as loud noise.)
    Medium,
    /// 8 kHz: clearly better than a phone line.
    Wide,
    /// 12 kHz: everything a voice needs.
    SuperWide,
}

/// One rung of the bitrate ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceTier {
    /// The Opus target, bits per second.
    pub bitrate: u32,
    /// The widest audio band the encoder may use.
    pub band: Band,
    /// Audio per packet. Longer packets cost fewer headers per second and
    /// add that much latency.
    pub frame_ms: u32,
}

/// The ladder, best first. Index 0 is a good connection; the last is the
/// floor, the least that still carries intelligible speech.
pub const TIERS: [VoiceTier; 6] = [
    VoiceTier { bitrate: 32_000, band: Band::SuperWide, frame_ms: 20 },
    VoiceTier { bitrate: 24_000, band: Band::Wide, frame_ms: 20 },
    VoiceTier { bitrate: 16_000, band: Band::Wide, frame_ms: 20 },
    VoiceTier { bitrate: 12_000, band: Band::Wide, frame_ms: 40 },
    VoiceTier { bitrate: 8_000, band: Band::Medium, frame_ms: 60 },
    VoiceTier { bitrate: 6_000, band: Band::Medium, frame_ms: 60 },
];

/// The lowest tier: 6 kbit/s mediumband, 60 ms packets.
pub const FLOOR_TIER: usize = TIERS.len() - 1;

/// What a tier costs on the wire while someone is talking, in kbit/s:
/// the codec's bitrate plus every packet's headers. Silence costs almost
/// nothing, thanks to DTX.
pub fn wire_kbps(tier: usize) -> f32 {
    let t = TIERS[tier.min(FLOOR_TIER)];
    let packets_per_second = 1000.0 / t.frame_ms as f32;
    t.bitrate as f32 / 1000.0 + packets_per_second * WIRE_OVERHEAD_BYTES as f32 * 8.0 / 1000.0
}

/// The rate the codec runs at for a device at `device_rate`: the same,
/// when Opus supports it, so nothing is resampled.
fn codec_rate(device_rate: u32) -> u32 {
    if OPUS_RATES.contains(&device_rate) {
        device_rate
    } else {
        48_000
    }
}

/// Turns captured audio into voice payloads, one per packet.
pub struct VoiceSender {
    encoder: Encoder,
    codec_rate: u32,
    /// From the device's rate to the codec's, when they differ.
    resampler: Option<Resampler>,
    /// Audio at the codec's rate waiting for a whole packet's worth.
    pending: Vec<f32>,
    /// The next packet's sequence number.
    sequence: u32,
    tier: usize,
    loss_pct: u8,
}

impl VoiceSender {
    /// A sender for audio captured at `device_rate`, starting at the best
    /// tier with FEC and DTX on.
    pub fn new(device_rate: u32) -> Self {
        let rate = codec_rate(device_rate);
        let encoder = Encoder::builder(rate, Channels::Mono, Application::Voip)
            .build()
            .expect("Opus accepts its own sample rates in mono");
        let mut sender = Self {
            encoder,
            codec_rate: rate,
            resampler: (rate != device_rate).then(|| Resampler::new(device_rate, rate)),
            pending: Vec::new(),
            sequence: 0,
            tier: usize::MAX,
            loss_pct: 0,
        };
        // Errors from these setters mean an out-of-range value, which the
        // constants here never are; ignoring them keeps a call going on
        // the encoder's defaults rather than ending it.
        let _ = sender.encoder.set_signal(Signal::Voice);
        let _ = sender.encoder.set_inband_fec(true);
        let _ = sender.encoder.set_dtx(true);
        sender.set_tier(0);
        sender
    }

    /// The tier in use.
    pub fn tier(&self) -> usize {
        self.tier
    }

    /// Moves to `tier` (clamped to the floor). Takes effect from the next
    /// packet; audio already waiting is kept.
    pub fn set_tier(&mut self, tier: usize) {
        let tier = tier.min(FLOOR_TIER);
        if tier == self.tier {
            return;
        }
        self.tier = tier;
        let t = TIERS[tier];
        let _ = self.encoder.set_bitrate(Bitrate::Bits(t.bitrate as i32));
        let _ = self.encoder.set_max_bandwidth(match t.band {
            Band::Medium => Bandwidth::Mediumband,
            Band::Wide => Bandwidth::Wideband,
            Band::SuperWide => Bandwidth::Superwideband,
        });
        let _ = self.encoder.set_frame_duration(match t.frame_ms {
            20 => FrameDuration::Ms20,
            40 => FrameDuration::Ms40,
            _ => FrameDuration::Ms60,
        });
    }

    /// Tells the encoder how much loss to expect, in percent. Opus spends
    /// that much more of the bitrate on in-band FEC; at 0 it sends none.
    pub fn set_expected_loss(&mut self, pct: u8) {
        let pct = pct.min(30);
        if pct != self.loss_pct {
            self.loss_pct = pct;
            let _ = self.encoder.set_packet_loss_perc(pct as i32);
        }
    }

    /// Samples per packet at the codec's rate.
    fn frame_len(&self) -> usize {
        (self.codec_rate * TIERS[self.tier].frame_ms / 1000) as usize
    }

    /// Takes a block of captured audio (any length) and returns every
    /// packet it completed, as `(sequence, payload)`. Usually zero or one.
    pub fn push(&mut self, samples: &[f32]) -> Vec<(u32, Vec<u8>)> {
        match &mut self.resampler {
            Some(r) => r.process(samples, &mut self.pending),
            None => self.pending.extend_from_slice(samples),
        }
        let mut out = Vec::new();
        let mut packet = [0u8; MAX_PACKET];
        loop {
            let len = self.frame_len();
            if self.pending.len() < len {
                break;
            }
            let encoded = self.encoder.encode_float(&self.pending[..len], &mut packet);
            self.pending.drain(..len);
            let Ok(n) = encoded else { continue };
            if n <= DTX_PACKET_MAX {
                continue; // silence: nothing to send, and no gap left
            }
            let mut payload = Vec::with_capacity(HEADER_LEN + n);
            payload.push(VERSION);
            payload.extend_from_slice(&self.sequence.to_be_bytes());
            payload.extend_from_slice(&packet[..n]);
            out.push((self.sequence, payload));
            self.sequence = self.sequence.wrapping_add(1);
        }
        out
    }

    /// Transmission stopped (push-to-talk released, muted): drops the
    /// part-packet waiting, and the encoder's memory of the audio before
    /// the pause, so the first packet after it carries no FEC copy of
    /// unrelated speech.
    pub fn pause(&mut self) {
        self.pending.clear();
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
        let _ = self.encoder.reset_state();
    }
}

/// Marks a voice clip encoded by [`encode_clip`].
const CLIP_MAGIC: &[u8; 4] = b"AVC1";

/// Encodes a whole recording, such as a voice message, as Opus at 16
/// kbit/s wideband: about 2 KB a second, against 192 KB for raw audio.
///
/// Unlike a call there is no DTX -- skipped silence would shorten the
/// clip -- and no FEC, since a file arrives whole or not at all. The
/// container is this app's own and minimal: a 4-byte tag, then each Opus
/// packet as `[length: u16 BE][packet]`. Every packet is 20 ms.
pub fn encode_clip(samples: &[f32], device_rate: u32) -> Vec<u8> {
    let rate = codec_rate(device_rate);
    let mut resampled = Vec::with_capacity(samples.len());
    match (rate != device_rate).then(|| Resampler::new(device_rate, rate)) {
        Some(mut r) => r.process(samples, &mut resampled),
        None => resampled.extend_from_slice(samples),
    }
    let frame = (rate / 50) as usize;
    // The last part-frame is padded with silence rather than dropped.
    resampled.resize(resampled.len().div_ceil(frame) * frame, 0.0);

    let mut encoder =
        Encoder::builder(rate, Channels::Mono, Application::Voip).build().expect("Opus accepts its own sample rates in mono");
    let _ = encoder.set_signal(Signal::Voice);
    let _ = encoder.set_bitrate(Bitrate::Bits(16_000));
    let _ = encoder.set_max_bandwidth(Bandwidth::Wideband);
    let mut out = CLIP_MAGIC.to_vec();
    let mut packet = [0u8; MAX_PACKET];
    for chunk in resampled.chunks_exact(frame) {
        let Ok(n) = encoder.encode_float(chunk, &mut packet) else { continue };
        out.extend_from_slice(&(n as u16).to_be_bytes());
        out.extend_from_slice(&packet[..n]);
    }
    out
}

/// Decodes a clip made by [`encode_clip`] to audio at `device_rate`.
/// `None` for anything that isn't one, or is cut short.
pub fn decode_clip(bytes: &[u8], device_rate: u32) -> Option<Vec<f32>> {
    let mut rest = bytes.strip_prefix(CLIP_MAGIC)?;
    let mut rx = VoiceReceiver::new(device_rate);
    let mut out = Vec::new();
    let mut pcm = vec![0f32; MAX_FRAME_SAMPLES];
    while !rest.is_empty() {
        let (len, after) = rest.split_first_chunk::<2>()?;
        let len = u16::from_be_bytes(*len) as usize;
        let packet = after.get(..len)?;
        rest = &after[len..];
        let n = rx.decoder.decode_float(packet, &mut pcm, false).ok()?;
        let decoded = pcm[..n].to_vec();
        out.extend(rx.at_device_rate(&decoded));
    }
    Some(out)
}

/// Counts packets seen and lost on the wire, for the receiver's reports.
///
/// Loss is counted when the sequence jumps: a jump of three means two
/// packets missing. A packet that turns up later (reordered, or resent)
/// is not taken back off, which slightly overstates loss on a link that
/// reorders -- erring towards a lower bitrate, the safe side.
#[derive(Default, Debug)]
pub struct LossCounter {
    received: AtomicU64,
    lost: AtomicU64,
}

impl LossCounter {
    /// Loss since the last call, in percent, and resets the count. `None`
    /// when nothing arrived at all -- no news, rather than 0% or 100%.
    pub fn take_percent(&self) -> Option<u8> {
        let received = self.received.swap(0, Ordering::Relaxed);
        let lost = self.lost.swap(0, Ordering::Relaxed);
        if received == 0 {
            return None;
        }
        Some(((lost * 100) / (received + lost)).min(100) as u8)
    }
}

/// Turns voice payloads back into audio, for one sender.
pub struct VoiceReceiver {
    decoder: Decoder,
    /// From the codec's rate back to the device's, when they differ.
    resampler: Option<Resampler>,
    /// The newest sequence number decoded so far.
    newest: Option<u32>,
    loss: Arc<LossCounter>,
}

impl VoiceReceiver {
    /// A receiver whose audio comes out at `device_rate`.
    pub fn new(device_rate: u32) -> Self {
        let rate = codec_rate(device_rate);
        Self {
            decoder: Decoder::new(rate, Channels::Mono).expect("Opus accepts its own sample rates in mono"),
            resampler: (rate != device_rate).then(|| Resampler::new(rate, device_rate)),
            newest: None,
            loss: Arc::new(LossCounter::default()),
        }
    }

    /// This sender's loss count, for building reports elsewhere.
    pub fn loss(&self) -> Arc<LossCounter> {
        self.loss.clone()
    }

    /// Decodes one payload. A version 2 payload that arrives just after a
    /// single missing packet also yields that packet, rebuilt from the
    /// FEC copy inside this one, as the backup. Version 1 payloads (older
    /// peers) are passed through [`fec::decode`]. `None` for anything that
    /// won't parse: dropped like a lost packet.
    pub fn decode(&mut self, payload: &[u8]) -> Option<fec::Decoded> {
        match payload.first() {
            Some(&fec::VERSION) => fec::decode(payload),
            Some(&VERSION) if payload.len() > HEADER_LEN => self.decode_opus(payload),
            _ => None,
        }
    }

    fn decode_opus(&mut self, payload: &[u8]) -> Option<fec::Decoded> {
        let sequence = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
        let packet = &payload[HEADER_LEN..];
        let samples = self.decoder.packet_samples(packet).ok()?;
        if samples == 0 || samples > MAX_FRAME_SAMPLES {
            return None;
        }

        // Ahead of everything so far, or the first: count what was
        // skipped. Behind: a late or resent packet, decoded but not
        // counted again.
        let ahead = match self.newest {
            None => Some(1),
            Some(newest) => {
                let step = sequence.wrapping_sub(newest);
                (step != 0 && step < u32::MAX / 2).then_some(step)
            }
        };
        let mut backup = None;
        if let Some(step) = ahead {
            self.newest = Some(sequence);
            self.loss.received.fetch_add(1, Ordering::Relaxed);
            self.loss.lost.fetch_add(u64::from(step - 1), Ordering::Relaxed);
            // Exactly one missing: this packet's FEC copy is that one.
            // (Two or more, and it only covers the last of them -- not
            // worth a frame placed in the wrong slot.)
            if step == 2 {
                let mut pcm = vec![0f32; samples];
                if let Ok(n) = self.decoder.decode_float(packet, &mut pcm, true) {
                    pcm.truncate(n);
                    backup = Some(self.at_device_rate(&pcm));
                }
            }
        }

        let mut pcm = vec![0f32; samples];
        let n = self.decoder.decode_float(packet, &mut pcm, false).ok()?;
        pcm.truncate(n);
        Some(fec::Decoded { sequence, primary: self.at_device_rate(&pcm), backup })
    }

    fn at_device_rate(&mut self, pcm: &[f32]) -> Vec<f32> {
        match &mut self.resampler {
            Some(r) => {
                let mut out = Vec::with_capacity(pcm.len() * 2);
                r.process(pcm, &mut out);
                out
            }
            None => pcm.to_vec(),
        }
    }
}

/// A streaming linear-interpolation resampler: enough for speech between
/// 44.1 and 48 kHz, where the two rates are close and the voice band sits
/// far below either Nyquist limit.
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output sample falls, in input samples, counted from
    /// the first sample of the next block; -1 is `previous`.
    position: f64,
    /// The last sample of the previous block.
    previous: f32,
}

impl Resampler {
    /// Converts `from` Hz to `to` Hz.
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, position: 0.0, previous: 0.0 }
    }

    /// Forgets the previous block, for a stream that starts over.
    pub fn reset(&mut self) {
        self.position = 0.0;
        self.previous = 0.0;
    }

    /// Appends `input`, resampled, to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let last = (input.len() - 1) as f64;
        let at = |i: isize| if i < 0 { self.previous } else { input[i as usize] };
        while self.position <= last {
            let i = self.position.floor();
            let frac = (self.position - i) as f32;
            let a = at(i as isize);
            let b = if (i as usize) < input.len() - 1 || i < 0.0 { at(i as isize + 1) } else { a };
            out.push(a + (b - a) * frac);
            self.position += self.step;
        }
        self.position -= input.len() as f64;
        self.previous = input[input.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Something voice-like: a buzzy harmonic series on a gliding pitch,
    /// switching on and off a few times a second like syllables.
    fn speech(rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let f0 = 140.0 + 30.0 * (t * 3.0).sin();
                let on = if (t * 2.5).fract() < 0.7 { 1.0 } else { 0.0 };
                let tone: f32 = (1..12)
                    .map(|h| (2.0 * std::f32::consts::PI * f0 * h as f32 * t).sin() / h as f32)
                    .sum();
                tone * 0.15 * on
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Runs `audio` through a sender in 10 ms blocks, as a sound card
    /// would deliver it.
    fn send_all(sender: &mut VoiceSender, audio: &[f32], rate: u32) -> Vec<(u32, Vec<u8>)> {
        audio.chunks(rate as usize / 100).flat_map(|b| sender.push(b)).collect()
    }

    #[test]
    fn every_tier_round_trips_speech_at_about_its_bitrate() {
        let audio = speech(48_000, 3.0);
        for (tier, t) in TIERS.iter().enumerate() {
            let mut tx = VoiceSender::new(48_000);
            tx.set_tier(tier);
            let mut rx = VoiceReceiver::new(48_000);
            let packets = send_all(&mut tx, &audio, 48_000);
            let bytes: usize = packets.iter().map(|(_, p)| p.len() - HEADER_LEN).sum();
            let kbps = bytes as f32 * 8.0 / 3.0 / 1000.0;
            let target = t.bitrate as f32 / 1000.0;
            // Near the target. Opus overshoots a little at the bottom --
            // its lowest modes have a minimum cost per frame -- so the
            // floor, asked for 6, measures about 8 on this buzzy test
            // signal; less on real speech, with its pauses.
            assert!(kbps < target + 3.0 && kbps > target * 0.4, "tier {tier}: {kbps} kbit/s");

            let mut out = Vec::new();
            for (_, p) in &packets {
                out.extend(rx.decode(p).unwrap().primary);
            }
            // Level, not waveform: the codec delays and reshapes the
            // signal, but speech in must come out as speech-loud audio.
            let ratio = rms(&out) / rms(&audio);
            assert!((0.5..1.6).contains(&ratio), "tier {tier}: level ratio {ratio}");
        }
    }

    #[test]
    fn the_floor_is_about_six_kbit_s_of_voice_and_fifteen_on_the_wire() {
        assert_eq!(TIERS[FLOOR_TIER].bitrate, 6_000);
        let wire = wire_kbps(FLOOR_TIER);
        assert!((14.0..16.0).contains(&wire), "{wire}");
        // The same bitrate in 20 ms packets would cost twice as much.
        assert!(wire_kbps(0) > 50.0);
    }

    #[test]
    fn silence_sends_almost_nothing_and_leaves_no_gap() {
        let mut tx = VoiceSender::new(48_000);
        let mut audio = speech(48_000, 1.0);
        audio.extend(vec![0.0; 48_000 * 2]);
        audio.extend(speech(48_000, 1.0));
        let packets = send_all(&mut tx, &audio, 48_000);
        // Four seconds at 20 ms would be 200 packets; DTX skips most of
        // the two silent seconds.
        assert!(packets.len() < 150, "{} packets", packets.len());
        // Sequence numbers stay contiguous, so nothing is concealed.
        for (i, (seq, _)) in packets.iter().enumerate() {
            assert_eq!(*seq, i as u32);
        }
    }

    #[test]
    fn a_single_lost_packet_is_rebuilt_from_the_next_ones_fec() {
        let audio = speech(48_000, 2.0);
        let mut tx = VoiceSender::new(48_000);
        tx.set_expected_loss(20);
        let packets = send_all(&mut tx, &audio, 48_000);
        let mut rx = VoiceReceiver::new(48_000);
        let mut rebuilt = 0;
        for (i, (_, p)) in packets.iter().enumerate() {
            if i % 10 == 5 {
                continue; // lost
            }
            let d = rx.decode(p).unwrap();
            if i % 10 == 6 {
                let backup = d.backup.expect("the packet after a loss carries its FEC copy");
                assert_eq!(backup.len(), d.primary.len());
                if rms(&backup) > 0.01 {
                    rebuilt += 1;
                }
            } else {
                assert!(d.backup.is_none(), "no backup when nothing was lost");
            }
        }
        // Most losses fall in speech, and those come back as audio.
        assert!(rebuilt >= 5, "{rebuilt} rebuilt");
        // One in ten lost.
        let loss = rx.loss().take_percent().unwrap();
        assert!((8..=12).contains(&loss), "{loss}%");
        assert_eq!(rx.loss().take_percent(), None, "reset after reading");
    }

    #[test]
    fn a_device_at_44_1_khz_is_resampled_both_ways() {
        let audio = speech(44_100, 2.0);
        let mut tx = VoiceSender::new(44_100);
        let mut rx = VoiceReceiver::new(44_100);
        let mut out = Vec::new();
        for (_, p) in send_all(&mut tx, &audio, 44_100) {
            out.extend(rx.decode(&p).unwrap().primary);
        }
        // Nearly every sample comes back, at the device's own rate (less
        // the codec's look-ahead and the last part-packet).
        let missing = audio.len() as i64 - out.len() as i64;
        assert!((0..2_000).contains(&missing), "{missing} samples short");
        let ratio = rms(&out) / rms(&audio);
        assert!((0.5..1.6).contains(&ratio), "level ratio {ratio}");
    }

    #[test]
    fn changing_tier_mid_call_keeps_the_stream_going() {
        let audio = speech(48_000, 2.0);
        let mut tx = VoiceSender::new(48_000);
        let mut rx = VoiceReceiver::new(48_000);
        let mut out = Vec::new();
        for (i, block) in audio.chunks(480).enumerate() {
            if i == 100 {
                tx.set_tier(FLOOR_TIER);
            }
            for (_, p) in tx.push(block) {
                out.extend(rx.decode(&p).unwrap().primary);
            }
        }
        assert!(out.len() > audio.len() * 9 / 10);
        assert_eq!(rx.loss().take_percent(), Some(0));
    }

    #[test]
    fn older_peers_raw_frames_are_still_heard() {
        let mut rx = VoiceReceiver::new(48_000);
        let raw = fec::encode(9, &[0.25; 480], None);
        let d = rx.decode(&raw).unwrap();
        assert_eq!(d.sequence, 9);
        assert_eq!(d.primary, vec![0.25; 480]);
    }

    #[test]
    fn a_recorded_clip_comes_back_the_same_length_at_a_fraction_of_the_size() {
        for rate in [48_000, 44_100] {
            let audio = speech(rate, 3.0);
            let clip = encode_clip(&audio, rate);
            // 16 kbit/s is 2 KB a second; raw f32 would be 4 bytes a sample.
            assert!(clip.len() < 8_000, "{} bytes for 3 s", clip.len());
            let back = decode_clip(&clip, rate).unwrap();
            let diff = back.len() as i64 - audio.len() as i64;
            assert!((-2..=rate as i64 / 50 + 10).contains(&diff), "{diff} samples off at {rate}");
            let ratio = rms(&back) / rms(&audio);
            assert!((0.5..1.6).contains(&ratio), "level ratio {ratio}");
        }
        assert!(decode_clip(b"nope", 48_000).is_none());
        let mut cut = encode_clip(&speech(48_000, 0.5), 48_000);
        cut.truncate(cut.len() - 3);
        assert!(decode_clip(&cut, 48_000).is_none(), "a truncated clip is refused");
    }

    #[test]
    fn garbage_is_dropped_not_decoded() {
        let mut rx = VoiceReceiver::new(48_000);
        assert!(rx.decode(&[]).is_none());
        assert!(rx.decode(&[VERSION, 0, 0, 0]).is_none());
        assert!(rx.decode(&[7, 1, 2, 3, 4, 5]).is_none());
    }

    #[test]
    fn the_resampler_keeps_its_place_across_blocks() {
        let input: Vec<f32> = (0..4_410).map(|i| i as f32).collect();
        let mut whole = Vec::new();
        Resampler::new(44_100, 48_000).process(&input, &mut whole);
        let mut pieces = Vec::new();
        let mut r = Resampler::new(44_100, 48_000);
        for block in input.chunks(97) {
            r.process(block, &mut pieces);
        }
        assert_eq!(whole.len(), pieces.len());
        for (a, b) in whole.iter().zip(&pieces) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
        // 0.1 s in, 0.1 s out.
        assert!((4_799..=4_801).contains(&whole.len()), "{}", whole.len());
    }
}
