//! Viewer-side speech transport. Audio never enters the harness protocol: only
//! the recognized text is submitted to the selected chat.

use std::io::Cursor;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use rodio::{Source, cpal};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::settings::VoiceSettings;

#[derive(Debug, PartialEq)]
pub enum Reply {
    Pending,
    Aborted,
    Superseded,
    Text(String),
}

/// Only speak the response following our own prompt, never a replayed answer
/// or a response to another device's later message.
pub fn reply_after(entries: &[zeron_doc::SessionMessageEntry], prompt: &str) -> Reply {
    use zeron_doc::{MessagePart, MessageRole, MessageStatus};
    let Some(index) = entries.iter().position(|entry| entry.id == prompt) else {
        return Reply::Pending;
    };
    let tail = &entries[index + 1..];
    if tail.iter().any(|entry| entry.role == MessageRole::User) {
        return Reply::Superseded;
    }
    let Some(last) = tail
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::Assistant)
    else {
        return Reply::Pending;
    };
    match last.status {
        Some(MessageStatus::Streaming) | None => Reply::Pending,
        Some(MessageStatus::Aborted) => Reply::Aborted,
        Some(MessageStatus::Complete) => Reply::Text(
            tail.iter()
                .filter(|entry| entry.role == MessageRole::Assistant)
                .flat_map(|entry| &entry.parts)
                .filter_map(|part| {
                    if let MessagePart::Text { text, .. } = part {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    }
}

/// Read prose without Markdown punctuation, URLs, or fenced source code.
pub fn speech_text(markdown: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut text = String::new();
    let mut skip: usize = 0;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::Image { .. }) => skip += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::Image) => {
                skip = skip.saturating_sub(1);
                text.push(' ');
            }
            Event::Text(value) | Event::Code(value) if skip == 0 => text.push_str(&value),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item)
                if skip == 0 =>
            {
                text.push(' ')
            }
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn speech_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for word in text.split_whitespace() {
        if chunk.len() + word.len() > 1600 && !chunk.is_empty() {
            chunks.push(std::mem::take(&mut chunk));
        }
        // Bound unbroken strings as well (for example a pasted URL or hash).
        for ch in word.chars() {
            if chunk.len() + ch.len_utf8() > 1800 {
                chunks.push(std::mem::take(&mut chunk));
            }
            chunk.push(ch);
        }
        chunk.push(' ');
        if chunk.len() > 500 && word.ends_with(['.', '!', '?']) {
            chunks.push(std::mem::take(&mut chunk));
        }
    }
    if !chunk.trim().is_empty() {
        chunks.push(chunk.trim().into());
    }
    chunks
}

const KEY_SERVICE: &str = "zeron-voice";
const MAX_RECORDING_SECONDS: u32 = 60;

fn credential(settings: &VoiceSettings) -> Result<keyring::Entry, String> {
    let url = endpoint(settings, "")?;
    // A saved key belongs to this endpoint. Editing the endpoint never forwards
    // the old key to a different server without explicitly saving it there.
    let account = format!("openrouter-{:x}", Sha256::digest(url.as_str().as_bytes()));
    keyring::Entry::new(KEY_SERVICE, &account).map_err(|e| e.to_string())
}

pub fn save_api_key(settings: &VoiceSettings, value: &str) -> Result<(), String> {
    let entry = credential(settings)?;
    if value.trim().is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    } else {
        if !value.trim().is_ascii() || value.trim().chars().any(char::is_whitespace) {
            return Err("API key must not contain whitespace or non-ASCII characters".into());
        }
        entry.set_password(value.trim()).map_err(|e| e.to_string())
    }
}

pub fn has_api_key(settings: &VoiceSettings) -> bool {
    api_key(settings).is_ok()
}

fn api_key(settings: &VoiceSettings) -> Result<String, String> {
    match credential(settings)?.get_password() {
        Ok(key) if !key.is_empty() => return Ok(key),
        _ => {}
    }
    if endpoint(settings, "")?.as_str() == "https://openrouter.ai/api/v1/"
        && let Ok(key) = std::env::var("OPENROUTER_API_KEY")
        && !key.trim().is_empty()
    {
        return Ok(key.trim().to_string());
    }
    Err("Set an OpenRouter API key for this endpoint in Voice settings".into())
}

fn endpoint(settings: &VoiceSettings, route: &str) -> Result<url::Url, String> {
    if settings.provider != "openrouter" {
        return Err("Unsupported voice provider".into());
    }
    let base = url::Url::parse(settings.endpoint.trim()).map_err(|_| "Invalid voice endpoint")?;
    let local = base.host_str() == Some("localhost")
        || base.host().is_some_and(|host| match host {
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
            _ => false,
        });
    if base.scheme() != "https" && !(base.scheme() == "http" && local) {
        return Err("Voice endpoint must use HTTPS (HTTP is allowed on localhost)".into());
    }
    if !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("Voice endpoint cannot contain credentials, query, or fragment".into());
    }
    let mut url = base;
    url.set_path(&format!("{}/{}", url.path().trim_end_matches('/'), route));
    Ok(url)
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(75))
        .build()
        .map_err(|e| e.to_string())
}

async fn response_bytes(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 | 403 => "Voice provider rejected the API key".into(),
            402 => "OpenRouter credits are insufficient".into(),
            429 => "Voice provider is rate limited. Try again shortly.".into(),
            status => format!(
                "Voice provider returned HTTP {status}. Check the model, voice and endpoint."
            ),
        });
    }
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err("Voice response exceeded the size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        if bytes.len() + chunk.len() > limit {
            return Err("Voice response exceeded the size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub fn validate_settings(settings: &VoiceSettings) -> Result<(), String> {
    endpoint(settings, "models")?;
    Ok(())
}

pub fn validate_dictation_settings(settings: &VoiceSettings) -> Result<(), String> {
    validate_settings(settings)?;
    if settings.transcription_model.trim().is_empty() {
        return Err("Choose a transcription model".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeechModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<SpeechModel>,
}

pub async fn models(settings: &VoiceSettings, modality: &str) -> Result<Vec<SpeechModel>, String> {
    let mut url = endpoint(settings, "models")?;
    url.query_pairs_mut()
        .append_pair("output_modalities", modality);
    let response = client()?
        .get(url)
        .bearer_auth(api_key(settings)?)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut list =
        serde_json::from_slice::<ModelList>(&response_bytes(response, 4 * 1024 * 1024).await?)
            .map_err(|_| "Voice provider returned an invalid model list")?
            .data;
    list.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(list)
}

#[derive(Deserialize)]
struct Transcript {
    text: String,
}

pub async fn transcribe(settings: &VoiceSettings, wav: Vec<u8>) -> Result<String, String> {
    transcribe_with_key(settings, wav, api_key(settings)?).await
}

async fn transcribe_with_key(
    settings: &VoiceSettings,
    wav: Vec<u8>,
    key: String,
) -> Result<String, String> {
    if settings.transcription_model.trim().is_empty() {
        return Err("Choose a transcription model in Voice settings".into());
    }
    let url = endpoint(settings, "audio/transcriptions")?;
    let response = client()?.post(url)
        .bearer_auth(key)
        .timeout(Duration::from_secs(75))
        .json(&serde_json::json!({
            "model": settings.transcription_model,
            "input_audio": {"data": base64::engine::general_purpose::STANDARD.encode(wav), "format": "wav"}
        }))
        .send().await.map_err(|e| e.to_string())?;
    let text = serde_json::from_slice::<Transcript>(&response_bytes(response, 1024 * 1024).await?)
        .map_err(|_| "Voice provider returned an invalid transcript")?
        .text;
    if text.trim().is_empty() {
        Err("No speech was recognized".into())
    } else {
        Ok(text)
    }
}

pub async fn synthesize(settings: &VoiceSettings, text: &str) -> Result<Vec<u8>, String> {
    synthesize_with_key(settings, text, api_key(settings)?).await
}

async fn synthesize_with_key(
    settings: &VoiceSettings,
    text: &str,
    key: String,
) -> Result<Vec<u8>, String> {
    if settings.speech_model.trim().is_empty() {
        return Err("Choose a speech model in Voice settings".into());
    }
    let url = endpoint(settings, "audio/speech")?;
    let mut body = serde_json::json!({
        "model": settings.speech_model,
        "input": text,
        "response_format": "mp3"
    });
    if !settings.voice.trim().is_empty() {
        body["voice"] = settings.voice.trim().into();
    }
    let response = client()?
        .post(url)
        .bearer_auth(key)
        .timeout(Duration::from_secs(75))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let audio = response_bytes(response, 32 * 1024 * 1024).await?;
    if audio.is_empty() {
        Err("Voice provider returned empty audio".into())
    } else {
        Ok(audio)
    }
}

pub struct Recording {
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    pub level: Arc<AtomicU32>,
    worker: Option<JoinHandle<Result<Vec<u8>, String>>>,
}

impl Recording {
    pub fn start() -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let stop_worker = stop.clone();
        let level_worker = level.clone();
        let muted_worker = muted.clone();
        let worker = std::thread::Builder::new()
            .name("voice-microphone".into())
            .spawn(move || capture(stop_worker, muted_worker, level_worker))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stop,
            muted,
            level,
            worker: Some(worker),
        })
    }

    pub async fn finish(mut self) -> Result<Vec<u8>, String> {
        self.stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .take()
            .expect("recording worker is owned until finish");
        tokio::task::spawn_blocking(move || worker.join())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|_| "Microphone worker stopped unexpectedly".to_string())?
    }

    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Samples are accumulated only in memory, downmixed to mono, and bounded to
/// one short utterance. The control thread owns the stream so cancellation can
/// release the microphone even when a device stops producing callbacks.
fn capture(
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
) -> Result<Vec<u8>, String> {
    if stop.load(Ordering::Acquire) {
        return Err("Recording cancelled".into());
    }
    let device = cpal::default_host().default_input_device().ok_or(
        "No microphone available. Check the system input device and microphone permission.",
    )?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("Microphone unavailable: {e}"))?;
    let config: cpal::StreamConfig = supported.clone().into();
    let rate = config.sample_rate;
    let samples = Arc::new(Mutex::new(CaptureBuffer::new(rate)));
    let error = Arc::new(Mutex::new(None));
    macro_rules! stream {
        ($sample:ty) => {
            input_stream::<$sample>(
                &device,
                &config,
                samples.clone(),
                error.clone(),
                muted,
                level,
            )
        };
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => stream!(f32),
        cpal::SampleFormat::F64 => stream!(f64),
        cpal::SampleFormat::I16 => stream!(i16),
        cpal::SampleFormat::I32 => stream!(i32),
        cpal::SampleFormat::I8 => stream!(i8),
        cpal::SampleFormat::U8 => stream!(u8),
        cpal::SampleFormat::U16 => stream!(u16),
        cpal::SampleFormat::U32 => stream!(u32),
        _ => return Err("The microphone uses an unsupported audio format".into()),
    }?;
    stream
        .play()
        .map_err(|e| format!("Could not start microphone: {e}"))?;
    let started = Instant::now();
    while !stop.load(Ordering::Acquire)
        && started.elapsed() < Duration::from_secs(MAX_RECORDING_SECONDS as u64)
    {
        if samples
            .lock()
            .map_err(|_| "Microphone buffer failed")?
            .finished
        {
            break;
        }
        if error
            .lock()
            .map_err(|_| "Microphone error state failed")?
            .is_some()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stream);
    if let Some(error) = error
        .lock()
        .map_err(|_| "Microphone error state failed")?
        .take()
    {
        return Err(error);
    }
    let samples = samples.lock().map_err(|_| "Microphone buffer failed")?;
    if !samples.heard_speech {
        return Err("No speech detected. Check the microphone or try again.".into());
    }
    Ok(wav_pcm16(&samples.samples, 1, rate))
}

struct CaptureBuffer {
    samples: Vec<i16>,
    rate: u32,
    voiced: usize,
    silence: usize,
    heard_speech: bool,
    finished: bool,
}

impl CaptureBuffer {
    fn new(rate: u32) -> Self {
        Self {
            samples: Vec::with_capacity(rate as usize * MAX_RECORDING_SECONDS as usize),
            rate,
            voiced: 0,
            silence: 0,
            heard_speech: false,
            finished: false,
        }
    }

    fn append(&mut self, mono: &[f32], muted: bool) -> u32 {
        if self.finished {
            return 0;
        }
        let rms = (mono.iter().map(|x| x * x).sum::<f32>() / mono.len().max(1) as f32).sqrt();
        if !muted {
            if rms >= 0.008 {
                self.voiced += mono.len();
                self.silence = 0;
                self.heard_speech |= self.voiced >= self.rate as usize / 5;
            } else {
                self.silence += mono.len();
            }
        } else {
            self.silence = 0;
        }
        let remaining = (self.rate as usize * MAX_RECORDING_SECONDS as usize)
            .saturating_sub(self.samples.len());
        self.samples
            .extend(mono.iter().take(remaining).map(|sample| {
                if muted {
                    0
                } else {
                    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                }
            }));
        self.finished = self.samples.len() >= self.rate as usize * MAX_RECORDING_SECONDS as usize
            || (!muted && !self.heard_speech && self.samples.len() >= self.rate as usize * 15);
        if muted {
            0
        } else {
            (rms * 500.0).min(100.0) as u32
        }
    }
}

fn input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Arc<Mutex<CaptureBuffer>>,
    error: Arc<Mutex<Option<String>>>,
    muted: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample as _;
    let channels = config.channels as usize;
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &_| {
                let mono: Vec<f32> = data
                    .chunks_exact(channels)
                    .map(|frame| {
                        frame
                            .iter()
                            .map(|&sample| f32::from_sample(sample))
                            .sum::<f32>()
                            / channels as f32
                    })
                    .collect();
                if let Ok(mut buffer) = samples.try_lock() {
                    level.store(
                        buffer.append(&mono, muted.load(Ordering::Acquire)),
                        Ordering::Relaxed,
                    );
                }
            },
            move |cause| {
                if let Ok(mut slot) = error.lock() {
                    *slot = Some(format!("Microphone disconnected: {cause}"));
                }
            },
            None,
        )
        .map_err(|e| format!("Could not open microphone: {e}"))
}

fn wav_pcm16(samples: &[i16], channels: u16, rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

pub fn play(mp3: Vec<u8>, stop: Arc<AtomicBool>, level: Arc<AtomicU32>) -> Result<(), String> {
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut sink = rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
    sink.log_on_drop(false);
    let player = rodio::Player::connect_new(sink.mixer());
    let decoder = rodio::Decoder::try_from(Cursor::new(mp3)).map_err(|e| e.to_string())?;
    let window =
        (decoder.sample_rate().get() as usize * decoder.channels().get() as usize / 30).max(1);
    player.append(Metered {
        source: decoder,
        level: level.clone(),
        window,
        count: 0,
        energy: 0.0,
    });
    while !player.empty() && !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(30));
    }
    if stop.load(Ordering::Acquire) {
        player.stop();
    }
    level.store(0, Ordering::Relaxed);
    Ok(())
}

struct Metered<S> {
    source: S,
    level: Arc<AtomicU32>,
    window: usize,
    count: usize,
    energy: f32,
}

impl<S: Source> Iterator for Metered<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let sample = self.source.next()?;
        self.energy += sample * sample;
        self.count += 1;
        if self.count >= self.window {
            self.level.store(
                ((self.energy / self.count as f32).sqrt() * 500.0).min(100.0) as u32,
                Ordering::Relaxed,
            );
            self.count = 0;
            self.energy = 0.0;
        }
        Some(sample)
    }
}

impl<S: Source> Source for Metered<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }
    fn channels(&self) -> rodio::ChannelCount {
        self.source.channels()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        self.source.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictation_and_read_aloud_configuration_are_independent() {
        let mut settings = VoiceSettings::default();
        assert!(settings.speech_model.is_empty());
        assert!(validate_dictation_settings(&settings).is_ok());
        settings.transcription_model.clear();
        settings.speech_model = "test/tts".into();
        assert!(validate_settings(&settings).is_ok());
        assert!(validate_dictation_settings(&settings).is_err());
    }

    #[test]
    fn dictation_keeps_listening_through_pauses_and_mute() {
        let mut capture = CaptureBuffer::new(1000);
        capture.append(&vec![0.05; 200], false);
        assert!(capture.heard_speech);
        capture.append(&vec![0.0; 1499], false);
        assert!(!capture.finished);
        capture.append(&[0.0], false);
        assert!(!capture.finished);
        let mut muted = CaptureBuffer::new(1000);
        assert_eq!(muted.append(&vec![0.8; 2000], true), 0);
        assert!(!muted.heard_speech);
        assert!(!muted.finished);
        assert!(muted.samples.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn capture_has_silence_and_duration_limits() {
        let mut silence = CaptureBuffer::new(1000);
        silence.append(&vec![0.0; 15000], false);
        assert!(silence.finished);
        assert!(!silence.heard_speech);
        let mut speech = CaptureBuffer::new(1000);
        speech.append(&vec![0.5; 65000], false);
        assert!(speech.finished);
        assert_eq!(speech.samples.len(), 60000);
    }

    #[test]
    fn prose_skips_code_and_image_and_chunks_unicode_safely() {
        assert_eq!(
            speech_text(
                "# Bonjour\n\n**Test** [lien](https://example.com).\n\n```rs\nsecret_code();\n```\n\n![image](x)\n\nSuite"
            ),
            "Bonjour Test lien. Suite"
        );
        let text = "é".repeat(5000);
        let chunks = speech_chunks(&text);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 1801));
        assert_eq!(chunks.concat().trim(), text);
    }

    #[test]
    fn response_is_scoped_to_our_prompt_and_waits_for_completion() {
        use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
        let entry = |id: &str, role, status| SessionMessageEntry {
            id: id.into(),
            role,
            status,
            created_at: 0,
            device_id: "test".into(),
            continuation_of: None,
            duration_ms: None,
            parts: vec![MessagePart::Text {
                id: "text".into(),
                text: id.into(),
            }],
        };
        let mut entries = vec![
            entry("old", MessageRole::Assistant, Some(MessageStatus::Complete)),
            entry("prompt", MessageRole::User, None),
        ];
        assert_eq!(reply_after(&entries, "prompt"), Reply::Pending);
        entries.push(entry(
            "answer",
            MessageRole::Assistant,
            Some(MessageStatus::Streaming),
        ));
        assert_eq!(reply_after(&entries, "prompt"), Reply::Pending);
        entries[2].status = Some(MessageStatus::Complete);
        assert_eq!(
            reply_after(&entries, "prompt"),
            Reply::Text("answer".into())
        );
        entries[2].status = Some(MessageStatus::Aborted);
        assert_eq!(reply_after(&entries, "prompt"), Reply::Aborted);
        entries.push(entry("other-device", MessageRole::User, None));
        assert_eq!(reply_after(&entries, "prompt"), Reply::Superseded);
    }

    fn mock_response(
        status: &str,
        response: &[u8],
    ) -> (VoiceSettings, JoinHandle<(String, serde_json::Value)>) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let settings = VoiceSettings {
            endpoint: format!("http://{}/api/v1", listener.local_addr().unwrap()),
            speech_model: "test/speech".into(),
            voice: "test-voice".into(),
            ..Default::default()
        };
        let status = status.to_string();
        let response = response.to_vec();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_string)
                })
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(&response).unwrap();
            (headers, serde_json::from_slice(&body).unwrap())
        });
        (settings, server)
    }

    #[tokio::test]
    async fn transcription_sends_json_wav_and_parses_text() {
        let (settings, server) = mock_response("200 OK", br#"{"text":"Bonjour"}"#);
        let wav = wav_pcm16(&[1, 2], 1, 16000);
        assert_eq!(
            transcribe_with_key(&settings, wav.clone(), "test-key".into())
                .await
                .unwrap(),
            "Bonjour"
        );
        let (headers, body) = server.join().unwrap();
        assert!(headers.starts_with("POST /api/v1/audio/transcriptions "));
        assert!(
            headers
                .to_lowercase()
                .contains("authorization: bearer test-key")
        );
        assert_eq!(body["input_audio"]["format"], "wav");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(body["input_audio"]["data"].as_str().unwrap())
                .unwrap(),
            wav
        );
        assert!(body.get("language").is_none());
    }

    #[tokio::test]
    async fn speech_reads_binary_mp3_and_rejects_provider_errors() {
        let (settings, server) = mock_response("200 OK", b"ID3-test-audio");
        assert_eq!(
            synthesize_with_key(&settings, "Bonjour", "test-key".into())
                .await
                .unwrap(),
            b"ID3-test-audio"
        );
        let (headers, body) = server.join().unwrap();
        assert!(headers.starts_with("POST /api/v1/audio/speech "));
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["input"], "Bonjour");
        assert_eq!(body["voice"], "test-voice");
        let (mut settings, server) = mock_response("200 OK", b"ID3-default-voice");
        settings.voice.clear();
        synthesize_with_key(&settings, "Bonjour", "test-key".into())
            .await
            .unwrap();
        assert!(server.join().unwrap().1.get("voice").is_none());
        let (settings, server) = mock_response("401 Unauthorized", b"private provider diagnostic");
        let error = synthesize_with_key(&settings, "Bonjour", "test-key".into())
            .await
            .unwrap_err();
        assert!(!error.contains("private"));
        assert!(!error.contains("test-key"));
        server.join().unwrap();
    }

    #[test]
    fn wav_header_matches_pcm_payload() {
        let wav = wav_pcm16(&[0, -1, 1], 1, 16_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50);
    }

    #[test]
    fn endpoint_rejects_credentials_and_insecure_remote_host() {
        let mut settings = VoiceSettings::default();
        assert_eq!(
            endpoint(&settings, "audio/speech").unwrap().as_str(),
            "https://openrouter.ai/api/v1/audio/speech"
        );
        settings.endpoint = "http://example.com/api/v1".into();
        assert!(endpoint(&settings, "audio/speech").is_err());
        settings.endpoint = "https://name:secret@example.com/api/v1".into();
        assert!(endpoint(&settings, "audio/speech").is_err());
    }
}
