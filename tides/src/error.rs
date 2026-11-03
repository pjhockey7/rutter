//! What can go wrong opening or reading a tide database.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// SQLite said no: missing file, corrupt file, locked file.
    Sqlite(rusqlite::Error),
    /// A SQLite file, but not one of ours.
    ///
    /// Distinguished from an empty database because handing `rutter run` a
    /// chart where it wanted tides is an easy mistake, and "no stations" is
    /// a poor way to report it.
    NotATideDatabase,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "{e}"),
            Self::NotATideDatabase => write!(
                f,
                "not a tide database — build one with `rutter tides --near <lat>,<lon>`"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::NotATideDatabase => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}
