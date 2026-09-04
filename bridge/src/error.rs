use std::fmt::{self, Display};

#[derive(Debug)]
pub struct Error(String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Display) -> Self {
        Self(message.to_string())
    }
}

impl Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(error)
    }
}

impl From<crate::json::Error> for Error {
    fn from(error: crate::json::Error) -> Self {
        Self::new(error)
    }
}

impl From<crate::framing::FrameError> for Error {
    fn from(error: crate::framing::FrameError) -> Self {
        Self::new(error)
    }
}

impl From<crate::root::RootError> for Error {
    fn from(error: crate::root::RootError) -> Self {
        Self::new(error)
    }
}

impl From<crate::scene::SceneError> for Error {
    fn from(error: crate::scene::SceneError) -> Self {
        Self::new(error)
    }
}

pub trait Context<T> {
    fn context(self, message: impl Display) -> Result<T>;
}

impl<T, E: Display> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl Display) -> Result<T> {
        self.map_err(|error| Error(format!("{message}: {error}")))
    }
}

#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::new(format_args!($($arg)*)))
    };
}
