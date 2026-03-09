#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(String),
    #[error("embedding error: {0}")]
    Embedding(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("transport error: {0}")]
    Transport(String),
}

#[cfg(feature = "sqlx")]
impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err.to_string())
    }
}

#[cfg(feature = "rmcp")]
impl From<Error> for rmcp::ErrorData {
    fn from(err: Error) -> Self {
        Self {
            code: error_code(&err),
            data: None,
            message: err.to_string().into(),
        }
    }
}

#[cfg(feature = "rmcp")]
fn error_code(err: &Error) -> rmcp::model::ErrorCode {
    match err {
        Error::Database(_) => rmcp::model::ErrorCode(-32_000),
        Error::Embedding(_) => rmcp::model::ErrorCode(-32_001),
        Error::Transport(_) => rmcp::model::ErrorCode(-32_002),
        Error::NotFound(_) => rmcp::model::ErrorCode(-32_004),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let err = Error::Embedding("test failure".to_owned());
        assert_eq!(err.to_string(), "embedding error: test failure");
    }

    #[test]
    #[cfg(feature = "rmcp")]
    fn error_to_mcp() {
        let err = Error::Embedding("ollama down".to_owned());
        let mcp: rmcp::ErrorData = err.into();
        assert_eq!(mcp.code, rmcp::model::ErrorCode(-32_001));
        assert!(mcp.message.contains("ollama down"));
    }

    #[test]
    #[cfg(feature = "rmcp")]
    fn error_codes() {
        assert_eq!(
            error_code(&Error::Embedding(String::new())),
            rmcp::model::ErrorCode(-32_001)
        );
    }
}
