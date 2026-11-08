//! Persist the released theme identity, never an editable copy of its tokens.
use nddev_opennetwork_design_tokens::Theme;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance(Theme);

impl Default for Appearance {
    fn default() -> Self {
        Self(Theme::Dark)
    }
}
impl Appearance {
    pub fn all() -> impl ExactSizeIterator<Item = Self> {
        Theme::ALL.into_iter().map(Self)
    }
    pub fn id(self) -> &'static str {
        self.0.id()
    }
    pub fn parse(id: &str) -> Option<Self> {
        Self::all().find(|theme| theme.id() == id)
    }
    #[cfg(feature = "viewer")]
    pub(crate) fn tokens(self) -> Theme {
        self.0
    }
}
impl Serialize for Appearance {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id())
    }
}
impl<'de> Deserialize<'de> for Appearance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        Self::parse(&id).ok_or_else(|| serde::de::Error::custom("unsupported appearance theme"))
    }
}
