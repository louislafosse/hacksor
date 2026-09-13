//! Hacksor: a fully local, single-client penetration testing assistant built on
//! the Codex agent harness (`codex-core`) embedded in-process. No cloud infra,
//! no `codex app-server` subprocess: the Tauri backend owns the agent loop and
//! streams events straight to the webview.

mod commands;
mod harness;
mod models;
mod platform;
mod runtime;
mod settings;
mod skills;

use std::sync::Arc;

use tauri::Manager;
use tokio::sync::Mutex;

use commands::AppState;
use settings::Settings;

/// The Hacksor persona / security-authorization prompt, injected as Codex
/// developer instructions on top of the harness's built-in tool protocol.
const DEVELOPER_PROMPT: &str = include_str!("../../prompts/hacksor-developer.md");

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hacksor=info,codex_core=warn".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let codex_home = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join("codex-home");
            std::fs::create_dir_all(&codex_home).ok();

            let settings = Settings::load(&codex_home);
            settings.export_env();

            // Extract bundled security skills and detect local tools once.
            let skills_dir = skills::extract_skills(&codex_home);
            let notes_dir = codex_home.join("notes");
            std::fs::create_dir_all(&notes_dir).ok();
            let env_note = skills::env_note(&skills_dir, &notes_dir);

            app.manage(AppState {
                settings: Mutex::new(settings),
                harness: Mutex::new(None),
                codex_home,
                developer_prompt: DEVELOPER_PROMPT.to_string(),
                env_note,
                opencodex: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_providers,
            commands::verify_turn,
            commands::review_action,
            commands::list_models,
            commands::get_settings,
            commands::save_settings,
            commands::start_chat,
            commands::send_message,
            commands::interrupt,
            commands::fork_thread,
            commands::list_threads,
            commands::resume_thread,
            commands::read_transcript,
            commands::thread_provider,
            commands::build_carryover,
            commands::usage_recent,
            commands::search_chats,
            commands::list_recents,
            commands::regenerate,
            commands::submit_approval,
            commands::pick_directory,
            commands::pick_files,
            commands::read_notes,
            commands::write_note,
            commands::delete_note,
            commands::save_text_dialog,
            commands::kali_status,
            commands::start_kali,
            commands::stop_kali,
            commands::opencodex_status,
            commands::ensure_opencodex_cmd,
            commands::open_opencodex_setup,
            commands::ocx_provider_list,
            commands::ocx_add_provider,
            commands::ocx_set_default,
            commands::runtime_status,
            commands::build_runtime,
            commands::rebuild_runtime,
            commands::prepare_runtime,
            commands::get_dockerfile,
            commands::save_dockerfile,
            commands::get_persona,
            commands::save_persona,
            commands::stop_runtime,
            commands::refresh_intel,
            commands::services_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while running Hacksor")
        .run(|_app, event| {
            // Tear down the Docker runtime container on exit so quitting never
            // leaves an orphaned `hacksor-runtime` behind.
            if let tauri::RunEvent::Exit = event {
                commands::cleanup_runtime();
            }
        });
}

// Silence an unused-import lint when Arc is only used transitively.
#[allow(unused)]
fn _ensure_arc(_: Arc<()>) {}
