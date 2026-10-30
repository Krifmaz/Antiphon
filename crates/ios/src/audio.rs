//! The audio session: microphone permission and voice-call mode.
//!
//! AVAudioSession is safe to use from any thread, which matters because
//! the call engine switches modes from its own.

use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord, AVAudioSessionModeDefault,
    AVAudioSessionModeVoiceChat, AVAudioSessionPortOverride, AVAudioSessionRecordPermission,
    AVAudioSessionSetActiveOptions,
};

/// Whether the session is in voice-call mode now, so leaving a call that
/// never set it up does not deactivate someone else's audio.
static IN_CALL: AtomicBool = AtomicBool::new(false);

/// Puts the session in its out-of-call state at launch.
///
/// The default category, SoloAmbient, cannot record at all: cpal would
/// open the microphone and get silence. PlayAndRecord is needed for the
/// mic test as much as for a call; DefaultToSpeaker stops it playing
/// through the earpiece, which PlayAndRecord otherwise does.
pub fn init() {
    if let Err(e) = set_category(false) {
        log::warn!("could not set the audio category: {e}");
    }
}

/// Sets PlayAndRecord with the mode and options for being in a call or
/// not; see [`init`] and [`set_call_audio`]. Any thread. The error is
/// AVFAudio's description, for the log.
fn set_category(in_call: bool) -> Result<(), String> {
    // SAFETY: plain property reads and a setter with valid arguments;
    // the statics are constant strings from AVFAudio.
    unsafe {
        let session = AVAudioSession::sharedInstance();
        let category = AVAudioSessionCategoryPlayAndRecord.ok_or("no PlayAndRecord category")?;
        let (mode, options) = if in_call {
            // VoiceChat turns on the system's own echo canceller and
            // automatic gain, tuned for speech, and routes to the earpiece.
            (AVAudioSessionModeVoiceChat, AVAudioSessionCategoryOptions::AllowBluetoothHFP)
        } else {
            (
                AVAudioSessionModeDefault,
                AVAudioSessionCategoryOptions::AllowBluetoothHFP | AVAudioSessionCategoryOptions::DefaultToSpeaker,
            )
        };
        let mode = mode.ok_or("no audio mode")?;
        session.setCategory_mode_options_error(category, mode, options).map_err(|e| e.to_string())
    }
}

/// Whether the user has allowed the microphone.
///
/// Deprecated in iOS 17 for AVAudioApplication, which does not exist on
/// the iOS 15 and 16 this app also supports.
#[expect(deprecated, reason = "the replacement, AVAudioApplication, is not on iOS 15 and 16")]
pub fn has_mic_permission() -> bool {
    // SAFETY: a property read.
    let permission = unsafe { AVAudioSession::sharedInstance().recordPermission() };
    permission == AVAudioSessionRecordPermission::Granted
}

/// Shows the system's microphone prompt the first time; afterwards iOS
/// answers from the stored choice without asking. The prompt makes the
/// app resign active and come back, so the shared code's `resumed` check
/// sees the answer; the callback only logs it.
pub fn request_mic_permission() {
    let block = RcBlock::new(|granted: Bool| log::info!("microphone permission: {}", granted.as_bool()));
    // SAFETY: as above; the block is copied by AVFAudio.
    #[expect(deprecated, reason = "the replacement, AVAudioApplication, is not on iOS 15 and 16")]
    unsafe {
        AVAudioSession::sharedInstance().requestRecordPermission(&block)
    };
}

/// Voice-call mode, or back to normal. In a call the session is
/// activated explicitly, since an app in the background may not start
/// audio unless its session is already active; out of one it is
/// deactivated with NotifyOthersOnDeactivation, so music another app was
/// playing before the call resumes.
pub fn set_call_audio(in_call: bool, speaker: bool) {
    let was = IN_CALL.swap(in_call, Ordering::Relaxed);
    if let Err(e) = set_category(in_call) {
        log::warn!("could not set the audio category: {e}");
    }
    // SAFETY: setters with valid arguments.
    unsafe {
        let session = AVAudioSession::sharedInstance();
        if in_call {
            if let Err(e) = session.setActive_error(true) {
                log::warn!("could not activate the audio session: {e}");
            }
            let port = if speaker { AVAudioSessionPortOverride::Speaker } else { AVAudioSessionPortOverride::None };
            if let Err(e) = session.overrideOutputAudioPort_error(port) {
                log::warn!("could not switch the speaker: {e}");
            }
        } else if was {
            if let Err(e) =
                session.setActive_withOptions_error(false, AVAudioSessionSetActiveOptions::NotifyOthersOnDeactivation)
            {
                log::warn!("could not deactivate the audio session: {e}");
            }
        }
    }
}
