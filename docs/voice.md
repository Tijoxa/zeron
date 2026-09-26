# Dictation and read aloud

Voice settings belong to the Zeron viewer, independently of the chat harness.

## Setup

Open **Settings → Voice**, choose **OpenRouter**, and configure the API base URL
(default: `https://openrouter.ai/api/v1`). Enter a key, then choose a dictation
model from the live list or enter its identifier manually. Save the settings.
Only the transcription model is needed for the microphone.

The **read-aloud model** and its voice are optional. Configure them only to use
**Read aloud**, the speaker action next to Copy on assistant responses. No
response is spoken automatically, and playback never starts the microphone.
A voice may be left blank only when the model documents a default voice.

Keys are stored in the operating system credential store for the configured
endpoint, never in `ui-settings.json`. Loading models also saves a newly entered
key. The default OpenRouter endpoint supports `OPENROUTER_API_KEY` as a fallback.

## Dictation controls

- **Microphone:** starts recording and replaces the input area with a moving
  waveform. The microphone becomes a Stop button.
- **Stop:** finishes recording and inserts the transcription in the draft,
  ready to edit. Existing draft text and attachments are preserved.
- **Send while recording:** finishes recording, transcribes, and submits the
  result through the usual chat flow. Clicking Send while transcription is
  pending also requests submission when it completes.
- **Cancel while transcribing:** discards the pending transcription.
- Switching chats or opening settings cancels recording and pending work.
- Pauses in speech do not submit anything. The 60-second recording limit ends
  capture and inserts a draft. Fifteen seconds without detected speech stops
  capture with an error. Errors preserve the existing draft.
- Sending uses the usual chat rules, including queuing while the agent is busy.
  If submission is blocked, the recognized text stays in the draft.

## Read aloud

Hover an assistant response and click its speaker action. A separate status
shows preparation/playback and a control to stop it. Long responses are split
into shorter requests; fenced code and image content are omitted. Reading is
explicit and does not send a new chat message.

## Devices and data

Capture and playback use the viewer's default system microphone and speakers,
even if the harness is on another device. Microphone permission may need to be
enabled in system settings. Audio stays in memory and is sent to the configured
provider for transcription; response text is sent for synthesis only when Read
aloud is requested. Provider retention and pricing apply. Zeron saves no audio
files. Transcribed text is saved in chat history only after submission.

Linux needs an available Secret Service keyring to save credentials. Linux
builds require `libasound2-dev` and `libdbus-1-dev` alongside the GPUI dependencies.
macOS bundles include a microphone usage description.

Protocol references: [OpenRouter TTS](https://openrouter.ai/docs/guides/overview/multimodal/tts)
and [OpenRouter transcription](https://openrouter.ai/blog/tutorials/transcription-on-openrouter/).
