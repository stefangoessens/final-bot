use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)] // variants will be exercised as modules land in later tasks
pub enum BotError {
    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("websocket error: {0}")]
    Ws(#[source] Box<tokio_tungstenite::tungstenite::Error>),

    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("unexpected error: {0}")]
    Other(String),
}

pub type BotResult<T> = Result<T, BotError>;

impl From<tokio_tungstenite::tungstenite::Error> for BotError {
    fn from(value: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Ws(Box::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_basic_variants() {
        let _ = BotError::Config("missing key".to_string());
        let _ = BotError::Other("unexpected".to_string());
        let _ = BotError::Io(std::io::Error::new(std::io::ErrorKind::Other, "io"));
    }

    #[tokio::test]
    async fn constructs_join_variant() {
        let handle = tokio::spawn(async {});
        handle.abort();
        let err = handle.await.unwrap_err();
        let _ = BotError::Join(err);
    }
}
