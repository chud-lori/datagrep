use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MongoError {
    #[error("datagrep supports query expressions, not arbitrary JavaScript — use a raw command document for anything else")]
    UnsupportedJs,

    #[error("unexpected end of input at byte {at}: expected {expected}")]
    UnexpectedEof { at: usize, expected: &'static str },

    #[error("unexpected token at byte {at}: expected {expected}, found {found:?}")]
    Unexpected {
        at: usize,
        expected: &'static str,
        found: String,
    },

    #[error("invalid {kind} literal {value:?} at byte {at}: {reason}")]
    InvalidLiteral {
        kind: &'static str,
        value: String,
        at: usize,
        reason: &'static str,
    },

    #[error("trailing input at byte {at}: {found:?}")]
    TrailingInput { at: usize, found: String },
}
