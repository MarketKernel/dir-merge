pub mod align;
pub mod inline;
pub mod lines;

pub use inline::{InlineDiff, inline_diff};
pub use lines::{DiffModel, DiffOptions, RowInfo, SegKind, WhitespaceMode};
