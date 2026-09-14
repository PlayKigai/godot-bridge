//! POSIX implementation of the platform API.

mod fs;
mod ipc;
mod lock;
mod misc;
mod proc;
mod watch;

pub use fs::{
    fallback_runtime_dir, open_nofollow_read, open_private, remove_socket, runtime_dir,
    socket_path, socket_path_for_state,
};
pub use ipc::{serve_socket, socket_request, SocketHandle};
pub use lock::{try_lock, LockGuard};
pub use misc::{open_url, stdout_file, tune_allocator};
pub use proc::{
    kill_group, kill_recorded, pid_alive_with_ticks, port_listener_belongs_to_process,
    process_start_ticks, spawn_gui_process, spawn_headless_process,
};
pub(crate) use watch::{watch_project_into, ProjectWatcher};
