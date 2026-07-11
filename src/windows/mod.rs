mod clipboard;
mod dpapi;
mod ipc;
mod shell;

pub use clipboard::{TextCaptureResult, capture_selected_text};
pub use dpapi::{protect_token, unprotect_token};
pub use ipc::{forward_paths, pipe_name, start_pipe_server};
pub use shell::{is_context_menu_registered, register_context_menu, unregister_context_menu};
