//! The error type of the core. `thiserror` for the engine; the app layers map these
//! onto UI-visible messages (docs/design.md, "Stack": thiserror core / anyhow app).

use serialport;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("port '{0}' is not open")]
    NotOpen(String),

    #[error("session '{0}' is already open")]
    AlreadyOpen(String),

    #[error("the operation was cancelled")]
    Cancelled,

    #[error("a runtime task failed: {0}")]
    Task(#[from] tokio::task::JoinError),

    #[error("an engine channel closed unexpectedly")]
    ChannelClosed,

    /// The Lua pre-send hook rejected the send (contract §2.4 `SessionError::Hook`):
    /// the conversion script failed (or hit its watchdog). The UI surfaces the
    /// message as a log row; the bytes never reach the wire.
    #[error("lua tx hook: {0}")]
    Hook(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
