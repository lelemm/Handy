#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::audio_toolkit::{is_microphone_access_denied, is_no_input_device_error};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{
    get_settings, AppSettings, FlushPostProcessContext, APPLE_INTELLIGENCE_PROVIDER_ID,
};
use crate::shortcut;
use crate::tray::{change_tray_icon, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_transcribing_overlay,
};
use crate::TranscriptionCoordinator;
use ferrous_opencc::{config::BuiltinConfig, OpenCC};
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::Manager;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};

#[derive(Clone, serde::Serialize)]
struct RecordingErrorEvent {
    error_type: String,
    detail: Option<String>,
}

/// Drop guard that notifies the [`TranscriptionCoordinator`] when the
/// transcription pipeline finishes — whether it completes normally or panics.
struct FinishGuard(AppHandle);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.try_state::<TranscriptionCoordinator>() {
            c.notify_processing_finished();
        }
    }
}

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

// Transcribe Action
struct TranscribeAction {
    post_process: bool,
}

struct FlushSession {
    cancelled: Arc<AtomicBool>,
    done_rx: oneshot::Receiver<FlushWorkerResult>,
}

struct FlushWorkerResult {
    emitted_text: String,
    inserted_any: bool,
    next_chunk_index: u64,
}

static FLUSH_SESSIONS: Lazy<Mutex<HashMap<String, FlushSession>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn cancel_flush_sessions() {
    let sessions = {
        let mut guard = FLUSH_SESSIONS.lock().unwrap();
        guard
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>()
    };

    for session in sessions {
        session.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Field name for structured output JSON schema
const TRANSCRIPTION_FIELD: &str = "transcription";

/// Strip invisible Unicode characters that some LLMs may insert
fn strip_invisible_chars(s: &str) -> String {
    s.replace(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}'], "")
}

/// Build a system prompt from the user's prompt template.
/// Removes `${output}` placeholder since the transcription is sent as the user message.
fn build_system_prompt(prompt_template: &str) -> String {
    prompt_template.replace("${output}", "").trim().to_string()
}

fn flush_context_input(transcription: &str, prior_context: Option<&str>) -> String {
    match prior_context.filter(|context| !context.trim().is_empty()) {
        Some(context) => format!(
            "Previous finalized text for context:\n{}\n\nCurrent chunk to clean and return:\n{}",
            context, transcription
        ),
        None => transcription.to_string(),
    }
}

fn flush_context_instruction(prior_context: Option<&str>) -> &'static str {
    if prior_context
        .map(|context| !context.trim().is_empty())
        .unwrap_or(false)
    {
        "\nUse the previous finalized text only for context. Return only the cleaned current chunk."
    } else {
        ""
    }
}

async fn post_process_transcription(
    settings: &AppSettings,
    transcription: &str,
    prior_context: Option<&str>,
) -> Option<String> {
    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return None;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return None;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return None;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return None;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return None;
    }

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Disable reasoning for providers where post-processing rarely benefits from it.
    // - custom: top-level reasoning_effort (works for local OpenAI-compat servers)
    // - openrouter: nested reasoning object; exclude:true also keeps reasoning text
    //   out of the response so it can't pollute structured-output JSON parsing
    let (reasoning_effort, reasoning) = match provider.id.as_str() {
        "custom" => (Some("none".to_string()), None),
        "openrouter" => (
            None,
            Some(crate::llm_client::ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
        ),
        _ => (None, None),
    };

    if provider.supports_structured_output {
        debug!("Using structured outputs for provider '{}'", provider.id);

        let system_prompt = format!(
            "{}{}",
            build_system_prompt(&prompt),
            flush_context_instruction(prior_context)
        );
        let user_content = flush_context_input(transcription, prior_context);

        // Handle Apple Intelligence separately since it uses native Swift APIs
        if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                if !apple_intelligence::check_apple_intelligence_availability() {
                    debug!(
                        "Apple Intelligence selected but not currently available on this device"
                    );
                    return None;
                }

                let token_limit = model.trim().parse::<i32>().unwrap_or(0);
                return match apple_intelligence::process_text_with_system_prompt(
                    &system_prompt,
                    &user_content,
                    token_limit,
                ) {
                    Ok(result) => {
                        if result.trim().is_empty() {
                            debug!("Apple Intelligence returned an empty response");
                            None
                        } else {
                            let result = strip_invisible_chars(&result);
                            debug!(
                                "Apple Intelligence post-processing succeeded. Output length: {} chars",
                                result.len()
                            );
                            Some(result)
                        }
                    }
                    Err(err) => {
                        error!("Apple Intelligence post-processing failed: {}", err);
                        None
                    }
                };
            }

            #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
            {
                debug!("Apple Intelligence provider selected on unsupported platform");
                return None;
            }
        }

        // Define JSON schema for transcription output
        let json_schema = serde_json::json!({
            "type": "object",
            "properties": {
                (TRANSCRIPTION_FIELD): {
                    "type": "string",
                    "description": "The cleaned and processed transcription text"
                }
            },
            "required": [TRANSCRIPTION_FIELD],
            "additionalProperties": false
        });

        match crate::llm_client::send_chat_completion_with_schema(
            &provider,
            api_key.clone(),
            &model,
            user_content,
            Some(system_prompt),
            Some(json_schema),
            reasoning_effort.clone(),
            reasoning.clone(),
        )
        .await
        {
            Ok(Some(content)) => {
                // Parse the JSON response to extract the transcription field
                match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(json) => {
                        if let Some(transcription_value) =
                            json.get(TRANSCRIPTION_FIELD).and_then(|t| t.as_str())
                        {
                            let result = strip_invisible_chars(transcription_value);
                            debug!(
                                "Structured output post-processing succeeded for provider '{}'. Output length: {} chars",
                                provider.id,
                                result.len()
                            );
                            return Some(result);
                        } else {
                            error!("Structured output response missing 'transcription' field");
                            return Some(strip_invisible_chars(&content));
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse structured output JSON: {}. Returning raw content.",
                            e
                        );
                        return Some(strip_invisible_chars(&content));
                    }
                }
            }
            Ok(None) => {
                error!("LLM API response has no content");
                return None;
            }
            Err(e) => {
                warn!(
                    "Structured output failed for provider '{}': {}. Falling back to legacy mode.",
                    provider.id, e
                );
                // Fall through to legacy mode below
            }
        }
    }

    // Legacy mode: Replace ${output} variable in the prompt with the actual text
    let user_content = flush_context_input(transcription, prior_context);
    let processed_prompt = format!(
        "{}{}",
        prompt.replace("${output}", &user_content),
        flush_context_instruction(prior_context)
    );
    debug!("Processed prompt length: {} chars", processed_prompt.len());

    match crate::llm_client::send_chat_completion(
        &provider,
        api_key,
        &model,
        processed_prompt,
        reasoning_effort,
        reasoning,
    )
    .await
    {
        Ok(Some(content)) => {
            let content = strip_invisible_chars(&content);
            debug!(
                "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                provider.id,
                content.len()
            );
            Some(content)
        }
        Ok(None) => {
            error!("LLM API response has no content");
            None
        }
        Err(e) => {
            error!(
                "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                provider.id,
                e
            );
            None
        }
    }
}

async fn maybe_convert_chinese_variant(
    settings: &AppSettings,
    transcription: &str,
) -> Option<String> {
    // Check if language is set to Simplified or Traditional Chinese
    let is_simplified = settings.selected_language == "zh-Hans";
    let is_traditional = settings.selected_language == "zh-Hant";

    if !is_simplified && !is_traditional {
        debug!("selected_language is not Simplified or Traditional Chinese; skipping translation");
        return None;
    }

    debug!(
        "Starting Chinese translation using OpenCC for language: {}",
        settings.selected_language
    );

    // Use OpenCC to convert based on selected language
    let config = if is_simplified {
        // Convert Traditional Chinese to Simplified Chinese
        BuiltinConfig::Tw2sp
    } else {
        // Convert Simplified Chinese to Traditional Chinese
        BuiltinConfig::S2tw
    };

    match OpenCC::from_config(config) {
        Ok(converter) => {
            let converted = converter.convert(transcription);
            debug!(
                "OpenCC translation completed. Input length: {}, Output length: {}",
                transcription.len(),
                converted.len()
            );
            Some(converted)
        }
        Err(e) => {
            error!("Failed to initialize OpenCC converter: {}. Falling back to original transcription.", e);
            None
        }
    }
}

pub(crate) struct ProcessedTranscription {
    pub final_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
}

pub(crate) async fn process_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;

    if let Some(converted_text) = maybe_convert_chinese_variant(&settings, transcription).await {
        final_text = converted_text;
    }

    if post_process {
        if let Some(processed_text) = post_process_transcription(&settings, &final_text, None).await
        {
            post_processed_text = Some(processed_text.clone());
            final_text = processed_text;

            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                if let Some(prompt) = settings
                    .post_process_prompts
                    .iter()
                    .find(|prompt| &prompt.id == prompt_id)
                {
                    post_process_prompt = Some(prompt.prompt.clone());
                }
            }
        }
    } else if final_text != transcription {
        post_processed_text = Some(final_text.clone());
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
    }
}

pub(crate) async fn process_flush_transcription_output(
    app: &AppHandle,
    transcription: &str,
    post_process: bool,
    prior_context: &str,
) -> ProcessedTranscription {
    let settings = get_settings(app);
    let mut final_text = transcription.to_string();
    let mut post_processed_text: Option<String> = None;
    let mut post_process_prompt: Option<String> = None;

    if let Some(converted_text) = maybe_convert_chinese_variant(&settings, transcription).await {
        final_text = converted_text;
    }

    if post_process {
        let llm_context = match settings.flush_post_process_context {
            FlushPostProcessContext::FullSession if !prior_context.trim().is_empty() => {
                Some(prior_context)
            }
            _ => None,
        };

        if let Some(processed_text) =
            post_process_transcription(&settings, &final_text, llm_context).await
        {
            post_processed_text = Some(processed_text.clone());
            final_text = processed_text;

            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                if let Some(prompt) = settings
                    .post_process_prompts
                    .iter()
                    .find(|prompt| &prompt.id == prompt_id)
                {
                    post_process_prompt = Some(prompt.prompt.clone());
                }
            }
        }
    } else if final_text != transcription {
        post_processed_text = Some(final_text.clone());
    }

    ProcessedTranscription {
        final_text,
        post_processed_text,
        post_process_prompt,
    }
}

const FLUSH_MIN_SAMPLE_COUNT: usize = 16_000;

fn pad_short_flush_samples(mut samples: Vec<f32>) -> Vec<f32> {
    if samples.len() < FLUSH_MIN_SAMPLE_COUNT && !samples.is_empty() {
        samples.resize(FLUSH_MIN_SAMPLE_COUNT * 5 / 4, 0.0);
    }
    samples
}

fn append_emitted_text(emitted_text: &mut String, text: &str) {
    if text.trim().is_empty() {
        return;
    }

    if !emitted_text.is_empty() {
        emitted_text.push('\n');
    }
    emitted_text.push_str(text);
}

async fn paste_without_auto_submit(app: &AppHandle, text: String) -> bool {
    let (tx, rx) = oneshot::channel();
    let ah = app.clone();
    let run_result = app.run_on_main_thread(move || {
        let result = utils::paste_without_auto_submit(text, ah.clone());
        let _ = tx.send(result);
    });

    if let Err(e) = run_result {
        error!("Failed to run flush paste on main thread: {:?}", e);
        return false;
    }

    match rx.await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            error!("Failed to paste flush transcription: {}", e);
            let _ = app.emit("paste-error", ());
            false
        }
        Err(e) => {
            error!("Flush paste result channel closed: {}", e);
            false
        }
    }
}

async fn send_final_auto_submit(app: &AppHandle) {
    let (tx, rx) = oneshot::channel();
    let ah = app.clone();
    let run_result = app.run_on_main_thread(move || {
        let result = utils::send_auto_submit_if_enabled(ah.clone());
        let _ = tx.send(result);
    });

    if let Err(e) = run_result {
        error!("Failed to run auto-submit on main thread: {:?}", e);
        return;
    }

    match rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            error!("Failed to auto-submit flush transcription: {}", e);
            let _ = app.emit("paste-error", ());
        }
        Err(e) => error!("Flush auto-submit result channel closed: {}", e),
    }
}

async fn process_flush_chunk(
    app: &AppHandle,
    tm: &Arc<TranscriptionManager>,
    hm: &Arc<HistoryManager>,
    samples: Vec<f32>,
    post_process: bool,
    prior_context: &str,
    chunk_index: u64,
    cancelled: &Arc<AtomicBool>,
) -> Option<String> {
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }

    let samples = pad_short_flush_samples(samples);
    if samples.is_empty() {
        return None;
    }

    let sample_count = samples.len();
    let file_name = format!(
        "handy-{}-flush-{}.wav",
        chrono::Utc::now().timestamp_millis(),
        chunk_index
    );
    let wav_path = hm.recordings_dir().join(&file_name);
    let wav_path_for_verify = wav_path.clone();
    let samples_for_wav = samples.clone();
    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
        crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
    });

    let transcription_time = Instant::now();
    let transcription_result = tm.transcribe_keep_loaded(samples);

    let wav_saved = match wav_handle.await {
        Ok(Ok(())) => {
            match crate::audio_toolkit::verify_wav_file(&wav_path_for_verify, sample_count) {
                Ok(()) => true,
                Err(e) => {
                    error!("Flush WAV verification failed: {}", e);
                    false
                }
            }
        }
        Ok(Err(e)) => {
            error!("Failed to save flush WAV file: {}", e);
            false
        }
        Err(e) => {
            error!("Flush WAV save task panicked: {}", e);
            false
        }
    };

    if cancelled.load(Ordering::Relaxed) {
        return None;
    }

    match transcription_result {
        Ok(transcription) => {
            debug!(
                "Flush transcription completed in {:?}: '{}'",
                transcription_time.elapsed(),
                transcription
            );

            let processed = process_flush_transcription_output(
                app,
                &transcription,
                post_process,
                prior_context,
            )
            .await;

            if cancelled.load(Ordering::Relaxed) {
                return None;
            }

            if wav_saved {
                if let Err(err) = hm.save_entry(
                    file_name,
                    transcription,
                    post_process,
                    processed.post_processed_text.clone(),
                    processed.post_process_prompt.clone(),
                ) {
                    error!("Failed to save flush history entry: {}", err);
                }
            }

            if processed.final_text.is_empty() {
                return None;
            }

            if paste_without_auto_submit(app, processed.final_text.clone()).await {
                Some(processed.final_text)
            } else {
                None
            }
        }
        Err(err) => {
            debug!("Flush transcription error: {}", err);
            if wav_saved {
                if let Err(save_err) =
                    hm.save_entry(file_name, String::new(), post_process, None, None)
                {
                    error!("Failed to save failed flush history entry: {}", save_err);
                }
            }
            None
        }
    }
}

fn start_flush_worker(
    app: &AppHandle,
    binding_id: String,
    mut rx: UnboundedReceiver<Vec<f32>>,
    post_process: bool,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = oneshot::channel();

    if let Some(old_session) = FLUSH_SESSIONS.lock().unwrap().insert(
        binding_id.clone(),
        FlushSession {
            cancelled: cancelled.clone(),
            done_rx,
        },
    ) {
        old_session.cancelled.store(true, Ordering::Relaxed);
    }

    let ah = app.clone();
    let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
    let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

    tauri::async_runtime::spawn(async move {
        let mut emitted_text = String::new();
        let mut inserted_any = false;
        let mut next_chunk_index = 0_u64;

        while let Some(samples) = rx.recv().await {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }

            utils::emit_flush_pending_chunks(&ah, rx.len() + 1);

            let processed_text = {
                let chunk_future = process_flush_chunk(
                    &ah,
                    &tm,
                    &hm,
                    samples,
                    post_process,
                    &emitted_text,
                    next_chunk_index,
                    &cancelled,
                );
                tokio::pin!(chunk_future);

                let mut ticker = tokio::time::interval(Duration::from_millis(250));
                loop {
                    tokio::select! {
                        result = &mut chunk_future => break result,
                        _ = ticker.tick() => {
                            utils::emit_flush_pending_chunks(&ah, rx.len() + 1);
                        }
                    }
                }
            };

            utils::emit_flush_pending_chunks(&ah, rx.len());

            if let Some(text) = processed_text {
                append_emitted_text(&mut emitted_text, &text);
                inserted_any = true;
            }

            next_chunk_index += 1;
        }

        utils::emit_flush_pending_chunks(&ah, 0);

        let _ = done_tx.send(FlushWorkerResult {
            emitted_text,
            inserted_any,
            next_chunk_index,
        });
    });
}

fn take_flush_session(binding_id: &str) -> Option<FlushSession> {
    FLUSH_SESSIONS.lock().unwrap().remove(binding_id)
}

fn start_flush_worker_if_needed(
    app: &AppHandle,
    binding_id: &str,
    post_process: bool,
    flush_rx: Option<UnboundedReceiver<Vec<f32>>>,
) {
    if let Some(rx) = flush_rx {
        start_flush_worker(app, binding_id.to_string(), rx, post_process);
    }
}

async fn finish_flush_session(
    app: AppHandle,
    tm: Arc<TranscriptionManager>,
    hm: Arc<HistoryManager>,
    samples: Option<Vec<f32>>,
    post_process: bool,
    session: FlushSession,
) {
    let worker_result = match session.done_rx.await {
        Ok(result) => result,
        Err(e) => {
            error!("Flush worker result channel closed: {}", e);
            FlushWorkerResult {
                emitted_text: String::new(),
                inserted_any: false,
                next_chunk_index: 0,
            }
        }
    };

    let emitted_text = worker_result.emitted_text;
    let mut inserted_any = worker_result.inserted_any;

    if !session.cancelled.load(Ordering::Relaxed) {
        if let Some(samples) = samples {
            if !samples.is_empty() {
                utils::emit_flush_pending_chunks(&app, 1);
            }

            if process_flush_chunk(
                &app,
                &tm,
                &hm,
                samples,
                post_process,
                &emitted_text,
                worker_result.next_chunk_index,
                &session.cancelled,
            )
            .await
            .is_some()
            {
                inserted_any = true;
            }

            utils::emit_flush_pending_chunks(&app, 0);
        }
    }

    if session.cancelled.load(Ordering::Relaxed) {
        utils::emit_flush_pending_chunks(&app, 0);
    }

    if inserted_any && !session.cancelled.load(Ordering::Relaxed) {
        send_final_auto_submit(&app).await;
    }

    tm.maybe_unload_immediately("flush session");
    utils::hide_recording_overlay(&app);
    change_tray_icon(&app, TrayIconState::Idle);
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Load ASR model and VAD model in parallel
        tm.initiate_model_load();
        let rm_clone = Arc::clone(&rm);
        std::thread::spawn(move || {
            if let Err(e) = rm_clone.preload_vad() {
                debug!("VAD pre-load failed: {}", e);
            }
        });

        let binding_id = binding_id.to_string();
        change_tray_icon(app, TrayIconState::Recording);
        show_recording_overlay(app);

        // Get the microphone mode to determine audio feedback timing
        let settings = get_settings(app);
        let is_always_on = settings.always_on_microphone;
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_error: Option<String> = None;
        if is_always_on {
            // Always-on mode: Play audio feedback immediately, then apply mute after sound finishes
            debug!("Always-on mode: Playing audio feedback immediately");
            let rm_clone = Arc::clone(&rm);
            let app_clone = app.clone();
            // The blocking helper exits immediately if audio feedback is disabled,
            // so we can always reuse this thread to ensure mute happens right after playback.
            std::thread::spawn(move || {
                play_feedback_sound_blocking(&app_clone, SoundType::Start);
                rm_clone.apply_mute();
            });

            match rm.try_start_recording(&binding_id) {
                Ok(flush_rx) => {
                    start_flush_worker_if_needed(app, &binding_id, self.post_process, flush_rx);
                }
                Err(e) => {
                    debug!("Recording failed: {}", e);
                    recording_error = Some(e);
                }
            }
        } else {
            // On-demand mode: Start recording first, then play audio feedback, then apply mute
            // This allows the microphone to be activated before playing the sound
            debug!("On-demand mode: Starting recording first, then audio feedback");
            let recording_start_time = Instant::now();
            match rm.try_start_recording(&binding_id) {
                Ok(flush_rx) => {
                    debug!("Recording started in {:?}", recording_start_time.elapsed());
                    start_flush_worker_if_needed(app, &binding_id, self.post_process, flush_rx);
                    // Small delay to ensure microphone stream is active
                    let app_clone = app.clone();
                    let rm_clone = Arc::clone(&rm);
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        debug!("Handling delayed audio feedback/mute sequence");
                        // Helper handles disabled audio feedback by returning early, so we reuse it
                        // to keep mute sequencing consistent in every mode.
                        play_feedback_sound_blocking(&app_clone, SoundType::Start);
                        rm_clone.apply_mute();
                    });
                }
                Err(e) => {
                    debug!("Failed to start recording: {}", e);
                    recording_error = Some(e);
                }
            }
        }

        if recording_error.is_none() {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);
        } else {
            // Starting failed (for example due to blocked microphone permissions).
            // Revert UI state so we don't stay stuck in the recording overlay.
            utils::hide_recording_overlay(app);
            change_tray_icon(app, TrayIconState::Idle);
            if let Some(err) = recording_error {
                let error_type = if is_microphone_access_denied(&err) {
                    "microphone_permission_denied"
                } else if is_no_input_device_error(&err) {
                    "no_input_device"
                } else {
                    "unknown"
                };
                let _ = app.emit(
                    "recording-error",
                    RecordingErrorEvent {
                        error_type: error_type.to_string(),
                        detail: Some(err),
                    },
                );
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        // Unregister the cancel shortcut when transcription stops
        shortcut::unregister_cancel_shortcut(app);

        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        change_tray_icon(app, TrayIconState::Transcribing);
        show_transcribing_overlay(app);

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let post_process = self.post_process;
        let flush_session = take_flush_session(&binding_id);

        tauri::async_runtime::spawn(async move {
            let _guard = FinishGuard(ah.clone());
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            let stop_recording_time = Instant::now();
            let samples_opt = rm.stop_recording(&binding_id);

            if let Some(session) = flush_session {
                match &samples_opt {
                    Some(samples) => debug!(
                        "Flush recording stopped in {:?}, final sample count: {}",
                        stop_recording_time.elapsed(),
                        samples.len()
                    ),
                    None => debug!("No final samples retrieved from flush recording stop"),
                }

                finish_flush_session(
                    ah.clone(),
                    Arc::clone(&tm),
                    Arc::clone(&hm),
                    samples_opt,
                    post_process,
                    session,
                )
                .await;
                return;
            }

            if let Some(samples) = samples_opt {
                debug!(
                    "Recording stopped and samples retrieved in {:?}, sample count: {}",
                    stop_recording_time.elapsed(),
                    samples.len()
                );

                if samples.is_empty() {
                    debug!("Recording produced no audio samples; skipping persistence");
                    utils::hide_recording_overlay(&ah);
                    change_tray_icon(&ah, TrayIconState::Idle);
                } else {
                    // Save WAV concurrently with transcription
                    let sample_count = samples.len();
                    let file_name = format!("handy-{}.wav", chrono::Utc::now().timestamp());
                    let wav_path = hm.recordings_dir().join(&file_name);
                    let wav_path_for_verify = wav_path.clone();
                    let samples_for_wav = samples.clone();
                    let wav_handle = tauri::async_runtime::spawn_blocking(move || {
                        crate::audio_toolkit::save_wav_file(&wav_path, &samples_for_wav)
                    });

                    // Transcribe concurrently with WAV save
                    let transcription_time = Instant::now();
                    let transcription_result = tm.transcribe(samples);

                    // Await WAV save and verify
                    let wav_saved = match wav_handle.await {
                        Ok(Ok(())) => {
                            match crate::audio_toolkit::verify_wav_file(
                                &wav_path_for_verify,
                                sample_count,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("WAV verification failed: {}", e);
                                    false
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            error!("Failed to save WAV file: {}", e);
                            false
                        }
                        Err(e) => {
                            error!("WAV save task panicked: {}", e);
                            false
                        }
                    };

                    match transcription_result {
                        Ok(transcription) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                transcription
                            );

                            if post_process {
                                show_processing_overlay(&ah);
                            }
                            let processed =
                                process_transcription_output(&ah, &transcription, post_process)
                                    .await;

                            // Save to history if WAV was saved
                            if wav_saved {
                                if let Err(err) = hm.save_entry(
                                    file_name,
                                    transcription,
                                    post_process,
                                    processed.post_processed_text.clone(),
                                    processed.post_process_prompt.clone(),
                                ) {
                                    error!("Failed to save history entry: {}", err);
                                }
                            }

                            if processed.final_text.is_empty() {
                                utils::hide_recording_overlay(&ah);
                                change_tray_icon(&ah, TrayIconState::Idle);
                            } else {
                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                let final_text = processed.final_text;
                                ah.run_on_main_thread(move || {
                                    match utils::paste(final_text, ah_clone.clone()) {
                                        Ok(()) => debug!(
                                            "Text pasted successfully in {:?}",
                                            paste_time.elapsed()
                                        ),
                                        Err(e) => {
                                            error!("Failed to paste transcription: {}", e);
                                            let _ = ah_clone.emit("paste-error", ());
                                        }
                                    }
                                    utils::hide_recording_overlay(&ah_clone);
                                    change_tray_icon(&ah_clone, TrayIconState::Idle);
                                })
                                .unwrap_or_else(|e| {
                                    error!("Failed to run paste on main thread: {:?}", e);
                                    utils::hide_recording_overlay(&ah);
                                    change_tray_icon(&ah, TrayIconState::Idle);
                                });
                            }
                        }
                        Err(err) => {
                            debug!("Global Shortcut Transcription error: {}", err);
                            // Save entry with empty text so user can retry
                            if wav_saved {
                                if let Err(save_err) = hm.save_entry(
                                    file_name,
                                    String::new(),
                                    post_process,
                                    None,
                                    None,
                                ) {
                                    error!("Failed to save failed history entry: {}", save_err);
                                }
                            }
                            utils::hide_recording_overlay(&ah);
                            change_tray_icon(&ah, TrayIconState::Idle);
                        }
                    }
                }
            } else {
                debug!("No samples retrieved from recording stop");
                utils::hide_recording_overlay(&ah);
                change_tray_icon(&ah, TrayIconState::Idle);
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction {
            post_process: false,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction { post_process: true }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});
