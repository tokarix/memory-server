#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("embedding error: {0}")]
    Embedding(String),
}

impl From<Error> for rmcp::ErrorData {
    fn from(err: Error) -> Self {
        Self {
            code: error_code(&err),
            data: None,
            message: err.to_string().into(),
        }
    }
}

fn error_code(err: &Error) -> i32 {
    match err {
        Error::Database(_) => -32_000,
        Error::Embedding(_) => -32_001,
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
    fn error_to_mcp() {
        let err = Error::Embedding("ollama down".to_owned());
        let mcp: rmcp::ErrorData = err.into();
        assert_eq!(mcp.code, -32_001);
        assert!(mcp.message.contains("ollama down"));
    }

    #[test]
    fn error_codes() {
        assert_eq!(
            error_code(&Error::Embedding(String::new())),
            -32_001
        );
    }
}
