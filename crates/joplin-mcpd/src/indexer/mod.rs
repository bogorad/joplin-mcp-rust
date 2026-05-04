mod item_content;
pub mod parser;
pub mod rebuild;
pub mod refresh;
pub mod source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoplinItemType {
    Note = 1,
    Folder = 2,
    Tag = 5,
    NoteTag = 6,
    Resource = 9,
    Revision = 13,
}

impl JoplinItemType {
    pub fn from_i32(value: i32) -> Option<Self> {
        match value {
            1 => Some(Self::Note),
            2 => Some(Self::Folder),
            5 => Some(Self::Tag),
            6 => Some(Self::NoteTag),
            9 => Some(Self::Resource),
            13 => Some(Self::Revision),
            _ => None,
        }
    }
}
