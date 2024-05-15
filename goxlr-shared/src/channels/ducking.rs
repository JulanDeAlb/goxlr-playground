use enum_map::Enum;
use strum::{Display, EnumIter};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "clap")]
use clap::ValueEnum;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Display, Enum, EnumIter)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DuckingInput {
    Mic,
}
