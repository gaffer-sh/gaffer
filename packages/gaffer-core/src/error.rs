#[derive(Debug, thiserror::Error)]
pub enum GafferError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
