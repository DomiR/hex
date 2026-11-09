//! `hex transcribe <file>`: one WAV file through an installed model. With
//! Hex running it goes to the running app's `POST /transcriptions`, on the
//! model Hex already holds; on Linux and Windows with no Hex running it
//! loads the model here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;

use super::{ControlReply, Output, print, running};
use crate::transcription::service::{AudioClip, AudioError};

/// The largest file the running Hex accepts (`local_api`'s audio limit).
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// How long a transcription of the largest file may take.
const READ_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Args, Debug)]
pub struct TranscribeArgs {
    /// A WAV file: PCM (8 to 32 bit) or 32-bit float, 8 to 192 kHz, up to
    /// 8 channels, at most 64 MiB.
    pub file: PathBuf,
    /// An installed model id from `hex model list` (default: the selected
    /// model).
    #[arg(long)]
    pub model: Option<String>,
    /// A language the model supports (default: the model's remembered one).
    #[arg(long)]
    pub language: Option<String>,
    #[command(flatten)]
    pub output: Output,
}

pub(super) fn transcribe(args: TranscribeArgs) -> u8 {
    let output = args.output;
    print(run(args).unwrap_or_else(|reply| reply), output)
}

fn run(args: TranscribeArgs) -> Result<ControlReply, ControlReply> {
    let wav = read(&args.file)?;
    let clip = AudioClip::decode_wav(std::io::Cursor::new(wav.as_slice()))
        .map_err(|error| audio_error(&args.file, error))?;
    let settings = crate::app_settings::AppSettings::read().unwrap_or_default();
    let model = args
        .model
        .unwrap_or_else(|| settings.transcription.model.as_str().to_owned());
    let selection = crate::control::model_selection(&settings, &model, args.language.as_deref())?;
    let text = if running() {
        in_running_hex(&args.file, &selection, &wav)?
    } else {
        here(&selection, clip)?
    };
    let text = text.trim();
    if text.is_empty() {
        return Err(ControlReply::error(
            "no-speech",
            format!("no speech in {}", args.file.display()),
        ));
    }
    Ok(ControlReply::Transcript { text: text.into() })
}

fn read(path: &Path) -> Result<Vec<u8>, ControlReply> {
    let failed = |error: std::io::Error| {
        ControlReply::error(
            "invalid-audio",
            format!("could not read {}: {error}", path.display()),
        )
    };
    let metadata = std::fs::metadata(path).map_err(failed)?;
    if !metadata.is_file() {
        return Err(ControlReply::error(
            "invalid-audio",
            format!("{} is not a file", path.display()),
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ControlReply::error(
            "resource-exhausted",
            format!("{} is larger than 64 MiB", path.display()),
        ));
    }
    let bytes = std::fs::read(path).map_err(failed)?;
    if !is_wav(&bytes) {
        return Err(ControlReply::error(
            "unsupported-audio",
            format!(
                "{} is not a WAV file; convert it first, for example `ffmpeg -i {} -ar 16000 -ac 1 out.wav`",
                path.display(),
                path.display()
            ),
        ));
    }
    Ok(bytes)
}

/// A RIFF file whose form type is WAVE.
fn is_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

fn audio_error(path: &Path, error: AudioError) -> ControlReply {
    match error {
        AudioError::Invalid => ControlReply::error(
            "invalid-audio",
            format!("{} is not a readable WAV file", path.display()),
        ),
        AudioError::Unsupported => ControlReply::error(
            "unsupported-audio",
            format!(
                "{} uses a WAV format Hex can't read; Hex reads PCM (8 to 32 bit) or 32-bit float, 8 to 192 kHz, up to 8 channels",
                path.display()
            ),
        ),
        AudioError::ResourceExhausted => ControlReply::error(
            "resource-exhausted",
            format!("{} is empty or too long", path.display()),
        ),
    }
}

fn in_running_hex(
    path: &Path,
    selection: &crate::transcription::models::TranscriptionSelection,
    wav: &[u8],
) -> Result<String, ControlReply> {
    let (status, body) = crate::local_api::post_transcription(
        selection.model.as_str(),
        &selection.language,
        wav,
        READ_TIMEOUT,
    )
    .map_err(|error| ControlReply::error("transport", format!("{error:#}")))?;
    let value = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
    if status == 200 {
        return value
            .get("transcript")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ControlReply::error("transport", "Hex sent an invalid transcript"));
    }
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| format!("http-{status}"), str::to_owned);
    Err(refused(path, selection, code))
}

/// The running Hex's refusal, named for a person.
fn refused(
    path: &Path,
    selection: &crate::transcription::models::TranscriptionSelection,
    code: String,
) -> ControlReply {
    let model = selection.model.as_str();
    let message = match code.as_str() {
        "model-not-ready" => {
            format!("{model} isn't installed; run `hex model install {model}`")
        }
        "unsupported-audio" => audio_message(path, AudioError::Unsupported),
        "invalid-audio" => audio_message(path, AudioError::Invalid),
        "resource-exhausted" => audio_message(path, AudioError::ResourceExhausted),
        "queue-full" => "Hex is transcribing another file; try again".into(),
        "unsupported-model" | "unknown-model" => {
            format!("{model} can't transcribe {}", selection.language)
        }
        "model-load-failed" => format!("Hex could not load {model}; see its log"),
        "http-404" => "the running Hex can't transcribe files; update Hex".into(),
        _ => format!("Hex could not transcribe {} ({code})", path.display()),
    };
    ControlReply::error(code, message)
}

fn audio_message(path: &Path, error: AudioError) -> String {
    match audio_error(path, error) {
        ControlReply::Error { message, .. } => message,
        _ => unreachable!("audio_error always refuses"),
    }
}

#[cfg(not(target_os = "macos"))]
fn here(
    selection: &crate::transcription::models::TranscriptionSelection,
    clip: AudioClip,
) -> Result<String, ControlReply> {
    let failed = |error: color_eyre::Report| ControlReply::error("failed", format!("{error:#}"));
    crate::transcription::local_transcriber::LocalTranscriber::load(selection)
        .map_err(failed)?
        .transcribe(&clip.into_samples())
        .map_err(failed)
}

/// macOS runs models only inside the app.
#[cfg(target_os = "macos")]
fn here(
    _: &crate::transcription::models::TranscriptionSelection,
    _: AudioClip,
) -> Result<String, ControlReply> {
    Err(super::not_running())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_riff_wave_files_are_wav() {
        assert!(is_wav(b"RIFF\0\0\0\0WAVEfmt "));
        assert!(!is_wav(b"ID3\x04\0\0\0\0\0\0\0\0"));
        assert!(!is_wav(b"RIFF\0\0\0\0AVI "));
        assert!(!is_wav(b"RIFF"));
    }

    #[test]
    fn a_missing_or_foreign_file_is_refused_before_hex_is_asked() {
        let directory = std::env::temp_dir().join(format!("hex-transcribe-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let missing = directory.join("missing.wav");
        assert!(matches!(
            read(&missing),
            Err(ControlReply::Error { code, .. }) if code == "invalid-audio"
        ));
        let mp3 = directory.join("note.mp3");
        std::fs::write(&mp3, b"ID3\x04\0\0\0\0\0\0\0\0").unwrap();
        assert!(matches!(
            read(&mp3),
            Err(ControlReply::Error { code, message })
                if code == "unsupported-audio" && message.contains("ffmpeg")
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_refusal_names_the_fix() {
        let selection = crate::transcription::models::TranscriptionSelection::default();
        let reply = refused(Path::new("a.wav"), &selection, "model-not-ready".into());
        let ControlReply::Error { message, .. } = reply else {
            panic!("expected a refusal");
        };
        assert!(message.contains("hex model install"));
    }
}
