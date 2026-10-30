//! Voice messages (spec §13: "short async audio clips").
//!
//! A voice message is recorded to a WAV in memory and then sent down the
//! *existing* file-transfer path. That is deliberate rather than lazy: the
//! file path already does chunking, hash verification, missing-chunk
//! repair, content addressing and resume, and a parallel "voice message
//! transport" would be all of that written a second time and tested less.
//!
//! What this module owns is the part that is genuinely different:
//!
//! - a length cap, because a voice message is a message and not a podcast
//! - a recognisable name, so the receiver's file dialog says what it is
//! - recording through the *same* mic chain a call uses, so what people
//!   hear matches what they hear from you live
//!
//! **Leaving one for someone who is offline** ([`leave`]) goes the way an
//! offline file does: the recording is compressed with Opus (about 2 KB a
//! second, so the two-minute cap is ~240 KB), sealed and pinned to your
//! pin nodes ([`crate::sharing::share`]), and a short text carrying its
//! link goes through your message routes (§8) like any other message. The
//! receiver's app recognises the link ([`parse_link`]), fetches the clip
//! from the pin node ([`fetch`]) and plays it ([`load_fetched`]). A
//! client that doesn't know the format still shows readable text.
//!
//! Without a pin node nothing could hold the clip while they're away, so
//! [`leave`] says so instead of sending a link to nothing.

use antiphon_audio::{AudioChain, ChainConfig};

/// Longest voice message, in seconds.
///
/// Short by design. The spec calls these "short async audio clips", and a
/// cap is what keeps them a quick alternative to typing rather than a way
/// to make someone sit through five minutes they cannot skim.
pub const MAX_SECONDS: u32 = 120;

/// Builds the filename a voice message arrives under.
///
/// Timestamped so a run of them does not collide, and prefixed so the
/// receiver's save dialog makes it obvious what they are being offered
/// rather than showing an opaque hash or "clip.wav".
pub fn file_name(at_secs: u64) -> String {
    format!("voice-message-{at_secs}.wav")
}

/// Whether a name looks like a voice message, so the UI can present an
/// incoming one as a clip to play rather than a file to save.
pub fn is_voice_message(file_name: &str) -> bool {
    file_name.starts_with("voice-message-") && file_name.ends_with(".wav")
}

/// Runs recorded audio through `config`'s chain, exactly as a live call
/// would.
///
/// Recording raw and sending that would mean your voice messages sounded
/// unlike your calls -- no gate, no EQ, no compression -- which is a
/// surprise nobody wants from a feature that is supposed to be "the same
/// as talking".
pub fn process_recording(samples: &mut [f32], sample_rate: u32, config: &ChainConfig) {
    AudioChain::from_config(sample_rate, config).process(samples);
}

/// Trims silence from the start and end.
///
/// People press record, then think, then talk. Sending the thinking part
/// makes a three-second message four, and the receiver waits through it
/// every time they replay it.
///
/// `threshold` is a linear amplitude. Everything between the first and
/// last sample above it is kept, including quiet passages in the middle --
/// this trims the ends, it does not remove pauses from speech.
pub fn trim_silence(samples: &[f32], threshold: f32) -> &[f32] {
    let first = samples.iter().position(|s| s.abs() > threshold);
    let Some(first) = first else { return &[] };
    let last = samples
        .iter()
        .rposition(|s| s.abs() > threshold)
        .unwrap_or(samples.len() - 1);
    &samples[first..=last]
}

/// Encodes samples as a 16-bit mono WAV in memory, ready to hand to the
/// file-transfer path.
pub fn encode_wav(sample_rate: u32, samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        // Clamped before scaling: a sample past full scale would otherwise
        // wrap to the opposite polarity and arrive as a click.
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// The line that marks a message as an offline voice message; the link
/// follows it as JSON.
pub const LINK_MARKER: &str = "stoop-voice-message:";

/// An offline voice message's link: everything needed to fetch and open
/// the clip, and its length for the player.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct VoiceLink {
    pub name: String,
    pub size: u64,
    pub file_id: String,
    pub key: String,
    /// Pin nodes holding it, `host:port`.
    pub holders: Vec<String>,
    /// Length, in seconds.
    pub seconds: u32,
}

/// The message text carrying `link`: a human line first, for any client
/// that doesn't know the format, then the marker and the link.
pub fn link_text(link: &VoiceLink) -> String {
    let json = serde_json::to_string(link).unwrap_or_default();
    format!("Voice message ({}:{:02})\n{LINK_MARKER}{json}", link.seconds / 60, link.seconds % 60)
}

/// The voice message in a message's text, if it carries one.
pub fn parse_link(text: &str) -> Option<VoiceLink> {
    let json = text.lines().find_map(|l| l.strip_prefix(LINK_MARKER))?;
    serde_json::from_str(json).ok()
}

/// Where fetched and outgoing clips are kept: under the config directory,
/// sealed copies only travel.
fn clip_dir() -> Option<std::path::PathBuf> {
    Some(crate::config::config_dir()?.join("voice-messages"))
}

/// Leaves a voice message for a contact who may be offline (spec §13
/// "short async audio clips"): compresses `samples`, pins the clip to
/// your pin nodes and queues its link through your message routes.
/// Recorded in your history as sent, like a text.
pub fn leave(
    runtime: &tokio::runtime::Handle,
    contact_key_hex: &str,
    samples: &[f32],
    sample_rate: u32,
) -> Result<VoiceLink, String> {
    let max = MAX_SECONDS as usize * sample_rate as usize;
    let samples = &samples[..samples.len().min(max)];
    if samples.is_empty() {
        return Err("Nothing was recorded.".into());
    }
    let clip = antiphon_net::voice::encode_clip(samples, sample_rate);
    let dir = clip_dir().ok_or("no config directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = format!("voice-message-{at}.voice");
    let path = dir.join(&name);
    std::fs::write(&path, &clip).map_err(|e| e.to_string())?;
    let Some(crate::control::ControlMessage::FileLink { size, file_id, key, holders, .. }) =
        crate::sharing::share(runtime, &path, &name, contact_key_hex)
    else {
        return Err("Add a pin node under Shared files first: it holds the message until they're back online.".into());
    };
    if holders.is_empty() {
        // Seeded from here only: no use to someone who comes online
        // after this device has gone.
        return Err("No pin node took the message. Check your pin nodes under Shared files.".into());
    }
    let seconds = duration_secs(samples, sample_rate).round() as u32;
    let link = VoiceLink { name, size, file_id, key, holders, seconds };
    crate::delivery::leave_message(contact_key_hex, &link_text(&link), None)?;
    Ok(link)
}

/// Starts fetching a received voice message from its pin nodes, in the
/// background. [`load_fetched`] has it once it has arrived;
/// [`crate::sharing::status`] with its `file_id` says how it is going.
pub fn fetch(runtime: &tokio::runtime::Handle, link: &VoiceLink, from_key_hex: &str) {
    let Some(dir) = clip_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    crate::sharing::receive_link(
        &link.name,
        link.size,
        link.file_id.clone(),
        link.key.clone(),
        link.holders.clone(),
        None,
        from_key_hex.to_string(),
    );
    crate::sharing::download(runtime, &link.file_id, dir.join(fetched_name(link)));
}

/// The fetched clip, decoded at `sample_rate`, once it has arrived.
pub fn load_fetched(link: &VoiceLink, sample_rate: u32) -> Option<Vec<f32>> {
    let bytes = std::fs::read(clip_dir()?.join(fetched_name(link))).ok()?;
    antiphon_net::voice::decode_clip(&bytes, sample_rate)
}

/// The local name a fetched clip is saved under: its id, so a hostile
/// name in the link can't choose a path.
fn fetched_name(link: &VoiceLink) -> String {
    let id: String = link.file_id.chars().filter(char::is_ascii_hexdigit).take(64).collect();
    format!("received-{id}.voice")
}

/// How long a recording is, for the UI's counter.
pub fn duration_secs(samples: &[f32], sample_rate: u32) -> f32 {
    samples.len() as f32 / sample_rate.max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offline_voice_message_link_survives_the_message_text() {
        let link = VoiceLink {
            name: "voice-message-1.voice".into(),
            size: 12_345,
            file_id: "ab".repeat(32),
            key: "cd".repeat(32),
            holders: vec!["10.0.0.1:3479".into()],
            seconds: 75,
        };
        let text = link_text(&link);
        assert!(text.starts_with("Voice message (1:15)"), "{text}");
        assert_eq!(parse_link(&text), Some(link));
        assert_eq!(parse_link("just words"), None);
        assert_eq!(parse_link(&format!("{LINK_MARKER}{{broken")), None);
    }

    #[test]
    fn a_fetched_clip_is_saved_under_its_id_not_its_name() {
        let link = VoiceLink {
            name: "../../evil".into(),
            size: 1,
            file_id: "ab".repeat(32),
            key: String::new(),
            holders: vec![],
            seconds: 1,
        };
        assert_eq!(fetched_name(&link), format!("received-{}.voice", "ab".repeat(32)));
    }

    #[test]
    fn a_voice_message_name_is_recognisable() {
        let name = file_name(1_700_000_000);
        assert!(is_voice_message(&name), "{name} was not recognised");
    }

    #[test]
    fn ordinary_files_are_not_mistaken_for_voice_messages() {
        for name in ["report.pdf", "recording.wav", "voice-message.txt", "", "notes.wav"] {
            assert!(!is_voice_message(name), "{name} was wrongly treated as a voice message");
        }
    }

    #[test]
    fn successive_messages_do_not_collide() {
        assert_ne!(file_name(1_700_000_000), file_name(1_700_000_001));
    }

    #[test]
    fn silence_is_trimmed_from_both_ends_but_not_the_middle() {
        let mut samples = vec![0.0f32; 100];
        samples.extend(std::iter::repeat_n(0.5, 10));
        samples.extend(std::iter::repeat_n(0.0, 50)); // a pause mid-speech
        samples.extend(std::iter::repeat_n(0.4, 10));
        samples.extend(std::iter::repeat_n(0.0, 100));

        let trimmed = trim_silence(&samples, 0.01);

        // Ends gone, middle pause intact -- this trims dead air, it does
        // not edit the speech.
        assert_eq!(trimmed.len(), 70);
        assert!(trimmed[0].abs() > 0.01);
        assert!(trimmed[trimmed.len() - 1].abs() > 0.01);
    }

    #[test]
    fn an_entirely_silent_recording_trims_to_nothing() {
        assert!(trim_silence(&vec![0.0f32; 1000], 0.01).is_empty());
    }

    #[test]
    fn trimming_leaves_audio_that_is_loud_throughout_alone() {
        let samples = vec![0.5f32; 100];
        assert_eq!(trim_silence(&samples, 0.01).len(), 100);
    }

    #[test]
    fn the_wav_header_says_what_the_data_actually_is() {
        let samples = vec![0.5f32; 480];
        let wav = encode_wav(48_000, &samples);

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "should be mono");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            48_000,
            "sample rate in the header must match the audio"
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "should be 16-bit");
        assert_eq!(wav.len(), 44 + samples.len() * 2);
    }

    #[test]
    fn a_recorded_message_round_trips_through_the_soundboard_loader() {
        // The same decoder the soundboard uses reads these back, so a
        // voice message is playable by the rest of the app rather than
        // being a format only this module understands.
        let samples: Vec<f32> = (0..4_800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let wav = encode_wav(48_000, &samples);

        let dir = std::env::temp_dir().join(format!("antiphon-vm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vm.wav");
        std::fs::write(&path, &wav).unwrap();

        let loaded = crate::soundboard::load_wav(&path).unwrap();
        assert_eq!(loaded.sample_rate, 48_000);
        assert_eq!(loaded.samples.len(), samples.len());
    }

    #[test]
    fn loud_samples_are_clamped_rather_than_wrapped() {
        // A sample past full scale that wrapped would arrive as a click at
        // the opposite polarity -- much more noticeable than clipping.
        let wav = encode_wav(48_000, &[4.0, -4.0]);
        let a = i16::from_le_bytes([wav[44], wav[45]]);
        let b = i16::from_le_bytes([wav[46], wav[47]]);

        assert!(a > 32_000, "positive overload wrapped: {a}");
        assert!(b < -32_000, "negative overload wrapped: {b}");
    }

    #[test]
    fn duration_is_reported_in_real_seconds() {
        assert!((duration_secs(&vec![0.0; 48_000], 48_000) - 1.0).abs() < 1e-6);
        // A zero rate must not divide by zero.
        assert!(duration_secs(&[0.0; 10], 0).is_finite());
    }

    #[test]
    fn an_empty_recording_encodes_to_a_valid_header() {
        let wav = encode_wav(48_000, &[]);
        assert_eq!(wav.len(), 44);
        assert_eq!(&wav[0..4], b"RIFF");
    }
}
