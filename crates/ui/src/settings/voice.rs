//! Viewer-side voice preferences, shared by all chats and harnesses.

use gpui::{Context, Entity, IntoElement, Render, Task, Window, div, prelude::*, px};

use crate::composer::ComposerInput;
use crate::settings::{self, SavePolicy, VoiceSettings, widgets};
use crate::theme::Theme;
use crate::voice::{self, SpeechModel};

pub struct VoicePage {
    scroll: widgets::PageScroll,
    endpoint: Entity<ComposerInput>,
    transcription_model: Entity<ComposerInput>,
    speech_model: Entity<ComposerInput>,
    voice: Entity<ComposerInput>,
    api_key: Entity<ComposerInput>,
    transcription_models: Vec<SpeechModel>,
    speech_models: Vec<SpeechModel>,
    loading: bool,
    provider_open: bool,
    models_open: Option<bool>,
    catalog_endpoint: Option<String>,
    key_available: bool,
    notice: Option<String>,
    task: Option<Task<()>>,
}

impl VoicePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = settings::current(cx).voice;
        let key_available = voice::has_api_key(&config);
        let field = |placeholder, value: String, cx: &mut Context<Self>| {
            let input = cx.new(|cx| {
                ComposerInput::new(placeholder, cx)
                    .with_accessibility_role(gpui::Role::TextInput)
                    .with_single_line()
            });
            input.update(cx, |input, cx| input.set_text(value, cx));
            input
        };
        Self {
            scroll: Default::default(),
            endpoint: field("OpenRouter API endpoint", config.endpoint, cx),
            transcription_model: field("Transcription model", config.transcription_model, cx),
            speech_model: field("Speech model", config.speech_model, cx),
            voice: field("Voice name", config.voice, cx),
            api_key: cx.new(|cx| {
                ComposerInput::new("New API key (optional)", cx)
                    .with_accessibility_role(gpui::Role::TextInput)
                    .with_masked()
            }),
            transcription_models: Vec::new(),
            speech_models: Vec::new(),
            loading: false,
            provider_open: false,
            models_open: None,
            catalog_endpoint: None,
            key_available,
            notice: None,
            task: None,
        }
    }

    fn config(&self, cx: &gpui::App) -> VoiceSettings {
        VoiceSettings {
            provider: "openrouter".into(),
            endpoint: self.endpoint.read(cx).text().trim().to_string(),
            transcription_model: self.transcription_model.read(cx).text().trim().to_string(),
            speech_model: self.speech_model.read(cx).text().trim().to_string(),
            voice: self.voice.read(cx).text().trim().to_string(),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let config = self.config(cx);
        let key = self.api_key.read(cx).text().trim().to_string();
        if let Err(error) = crate::voice::validate_settings(&config) {
            self.notice = Some(error);
            cx.notify();
            return;
        }
        if !key.is_empty() {
            if let Err(error) = voice::save_api_key(&config, &key) {
                self.notice = Some(format!("Could not save API key: {error}"));
                cx.notify();
                return;
            }
            self.api_key.update(cx, |input, cx| input.set_text("", cx));
            self.key_available = true;
        }
        settings::update(SavePolicy::Immediate, cx, |s| s.voice = config);
        self.notice = Some("Voice settings saved".into());
        cx.notify();
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let config = self.config(cx);
        let key = self.api_key.read(cx).text().trim().to_string();
        self.loading = true;
        self.notice = None;
        let requested_endpoint = config.endpoint.clone();
        let submitted_key = key.clone();
        let request = gpui_tokio::Tokio::spawn(cx, async move {
            if !key.is_empty() {
                if let Err(error) = voice::save_api_key(&config, &key) {
                    return (Err(error.clone()), Err(error));
                }
            }
            tokio::join!(
                voice::models(&config, "transcription"),
                voice::models(&config, "speech")
            )
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let (transcription, speech) = request
                .await
                .unwrap_or_else(|error| (Err(error.to_string()), Err(error.to_string())));
            this.update(cx, |page, cx| {
                page.loading = false;
                if page.config(cx).endpoint != requested_endpoint {
                    page.notice = Some("Endpoint changed. Load models again.".into());
                    cx.notify();
                    return;
                }
                match (transcription, speech) {
                    (Ok(transcription), Ok(speech)) => {
                        page.transcription_models = transcription;
                        page.speech_models = speech;
                        page.catalog_endpoint = Some(requested_endpoint);
                        page.notice = Some("Available models loaded".into());
                        page.key_available = true;
                        if page.api_key.read(cx).text().trim() == submitted_key {
                            page.api_key.update(cx, |input, cx| input.set_text("", cx));
                        }
                    }
                    (Err(error), _) | (_, Err(error)) => page.notice = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn field(
        &self,
        theme: &Theme,
        title: &'static str,
        input: Entity<ComposerInput>,
    ) -> gpui::AnyElement {
        div()
            .mt(px(14.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(widgets::row_title(theme, title))
            .child(
                div()
                    .min_h(px(42.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .child(input),
            )
            .into_any_element()
    }

    fn model_choices(
        &self,
        is_speech: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if self.models_open != Some(is_speech) {
            return None;
        }
        let models = if is_speech {
            &self.speech_models
        } else {
            &self.transcription_models
        };
        Some(
            div()
                .id(if is_speech {
                    "voice-speech-list"
                } else {
                    "voice-transcription-list"
                })
                .mt(px(6.0))
                .max_h(px(220.0))
                .overflow_y_scroll()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.border)
                .when(models.is_empty(), |list| {
                    list.child(div().p(px(10.0)).child(if self.loading {
                        "Loading models…"
                    } else {
                        "No models loaded. Check your endpoint and API key, then refresh."
                    }))
                })
                .children(models.iter().map(|model| {
                    let id = model.id.clone();
                    let label = if model.name.is_empty() {
                        id.clone()
                    } else {
                        format!("{} · {}", model.name, id)
                    };
                    div()
                        .id(gpui::ElementId::Name(
                            format!("voice-model-{is_speech}-{id}").into(),
                        ))
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(px(13.0))
                        .cursor_pointer()
                        .hover(|row| row.bg(theme.bg))
                        .child(label)
                        .on_click(cx.listener(move |page, _, _, cx| {
                            let field = if is_speech {
                                &page.speech_model
                            } else {
                                &page.transcription_model
                            };
                            field.update(cx, |input, cx| input.set_text(id.clone(), cx));
                            page.models_open = None;
                            cx.notify();
                        }))
                }))
                .into_any_element(),
        )
    }
}

impl Render for VoicePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if self
            .catalog_endpoint
            .as_deref()
            .is_some_and(|endpoint| endpoint != self.config(cx).endpoint)
        {
            self.transcription_models.clear();
            self.speech_models.clear();
            self.catalog_endpoint = None;
            self.key_available = false;
        }
        let mut card = widgets::section_card(&theme).p(px(18.0))
            .child(widgets::row_title(&theme, "Provider"))
            .child(widgets::ghost_action(&theme).id("voice-provider").mt(px(6.0))
                .aria_label("Voice provider")
                .child("OpenRouter ▾")
                .on_click(cx.listener(|page, _, _, cx| { page.provider_open = !page.provider_open; cx.notify(); })))
            .when(self.provider_open, |card| card.child(
                widgets::ghost_action(&theme).id("voice-provider-openrouter").mt(px(4.0))
                    .child("✓ OpenRouter")
                    .on_click(cx.listener(|page, _, _, cx| { page.provider_open = false; cx.notify(); }))))
            .child(self.field(&theme, "Endpoint", self.endpoint.clone()))
            .child(self.field(&theme, "OpenRouter API key", self.api_key.clone()))
            .child(div().mt(px(6.0)).text_size(px(12.0)).text_color(theme.text_muted)
                .child("Stored securely for this endpoint. Loading models also saves the entered key."))
            .child(self.field(&theme, "Dictation model (speech to text)", self.transcription_model.clone()))
            .child(widgets::ghost_action(&theme).id("voice-choose-transcription").mt(px(6.0))
                .child("Choose transcription model ▾")
                .on_click(cx.listener(|page, _, _, cx| {
                    page.models_open = if page.models_open == Some(false) { None } else { Some(false) };
                    if page.transcription_models.is_empty() { page.discover(cx); }
                    cx.notify();
                })))
            .children(self.model_choices(false, &theme, cx))
            .child(self.field(&theme, "Read-aloud model (optional)", self.speech_model.clone()))
            .child(div().mt(px(6.0)).text_size(px(12.0)).text_color(theme.text_muted)
                .child("Only used when you choose Read aloud on a response. Dictation does not need this model."))
            .child(widgets::ghost_action(&theme).id("voice-choose-speech").mt(px(6.0))
                .child("Choose speech model ▾")
                .on_click(cx.listener(|page, _, _, cx| {
                    page.models_open = if page.models_open == Some(true) { None } else { Some(true) };
                    if page.speech_models.is_empty() { page.discover(cx); }
                    cx.notify();
                })))
            .children(self.model_choices(true, &theme, cx))
            .child(self.field(&theme, "Voice", self.voice.clone()))
            .child(div().mt(px(6.0)).text_size(px(12.0)).text_color(theme.text_muted)
                .child("Enter a supported voice (for example alloy for OpenAI). Leave blank only if the model documents a default voice."))
            .child(div().mt(px(8.0)).text_size(px(12.0)).text_color(theme.text_muted)
                .child(if self.key_available { "API key available" } else { "Add an API key or set OPENROUTER_API_KEY" }));

        card = card.child(
            div()
                .mt(px(16.0))
                .flex()
                .gap(px(10.0))
                .child(
                    widgets::ghost_action(&theme)
                        .id("voice-model-discover")
                        .on_click(cx.listener(|page, _, _, cx| page.discover(cx)))
                        .child(if self.loading {
                            "Loading…"
                        } else {
                            "Discover models"
                        }),
                )
                .child(
                    widgets::ghost_action(&theme)
                        .id("voice-settings-save")
                        .on_click(cx.listener(|page, _, _, cx| page.save(cx)))
                        .child("Save"),
                ),
        );
        if let Some(notice) = &self.notice {
            card = card.child(div().mt(px(12.0)).child(notice.clone()));
        }
        div()
            .id("voice-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll.scroll)
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Voice", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Voice is shared across chats and independent of their agent and model.",
                    ))
                    .child(card),
            )
    }
}
