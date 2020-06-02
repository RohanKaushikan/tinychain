pub mod chain;
mod dir;
pub mod file;
mod history;
mod store;

pub const RECORD_DELIMITER: char = 30 as char;
pub const GROUP_DELIMITER: char = 29 as char;

pub type Dir = dir::Dir;
pub type History<O> = history::History<O>;
pub type Store = store::Store;
