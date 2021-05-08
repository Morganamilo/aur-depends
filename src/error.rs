use std::fmt::{self, Debug, Display};

/// The error type for aur-depends.
#[derive(Debug)]
pub enum Error<E> {
    /// An error occurred in the alpm crate.
    Alpm(alpm::Error),
    /// An error occurred in the rua crate.
    Raur(E),
}

impl<E> From<alpm::Error> for Error<E> {
    fn from(e: alpm::Error) -> Error<E> {
        Error::Alpm(e)
    }
}

impl<E: Display> Display for Error<E> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Alpm(e) => Display::fmt(&e, fmt),
            Error::Raur(e) => Display::fmt(&e, fmt),
        }
    }
}

impl<E: Display + Debug> std::error::Error for Error<E> {}
