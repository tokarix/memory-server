#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(String),
    #[error("embedding error: {0}")]
    Embedding(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{message}")]
    Policy {
        code: String,
        message: String,
        details: serde_json::Value,
        conflict: bool,
    },
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
        let data = match &err {
            Error::Policy { code, details, .. } => {
                Some(serde_json::json!({"code": code, "details": details}))
            }
            _ => None,
        };
        Self {
            code: error_code(&err),
            data,
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
        Error::Policy { conflict: true, .. } => rmcp::model::ErrorCode(-32_009),
        Error::Policy {
            conflict: false, ..
        } => rmcp::model::ErrorCode(-32_008),
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

    #[test]
    #[cfg(feature = "rmcp")]
    fn all_error_envelopes() {
        for (error, code, message) in [
            (
                Error::Database("fixture".into()),
                -32_000,
                "database error: fixture",
            ),
            (
                Error::Embedding("fixture".into()),
                -32_001,
                "embedding error: fixture",
            ),
            (
                Error::Transport("fixture".into()),
                -32_002,
                "transport error: fixture",
            ),
            (
                Error::NotFound("fixture".into()),
                -32_004,
                "fixture not found",
            ),
        ] {
            assert_eq!(error.to_string(), message);
            let converted: rmcp::ErrorData = error.into();
            assert_eq!(converted.code, rmcp::model::ErrorCode(code));
            assert_eq!(converted.message, message);
            assert_eq!(converted.data, None);
            assert_eq!(
                serde_json::to_value(converted).unwrap(),
                serde_json::json!({"code":code,"message":message})
            );
        }
    }
}
