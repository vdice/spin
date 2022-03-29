use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq)]
pub struct RawSection(pub HashMap<String, RawSlot>);

#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RawSlot {
    Default(String),
    Opts(RawSlotOpts),
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawSlotOpts {
    pub required: bool,
    pub secret: bool,
    pub default: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_section() {
        let section: RawSection = toml::toml! {
            simple_key = "simple"
            required_key = { required = true }
            secret_default = { default = "TOP-SECRET", secret = true }
        }
        .try_into()
        .unwrap();

        assert_eq!(
            section,
            RawSection(HashMap::from([
                (
                    "simple_key".to_string(),
                    RawSlot::Default("simple".to_string())
                ),
                (
                    "required_key".to_string(),
                    RawSlot::Opts(RawSlotOpts {
                        required: true,
                        ..Default::default()
                    })
                ),
                (
                    "secret_default".to_string(),
                    RawSlot::Opts(RawSlotOpts {
                        secret: true,
                        default: Some("TOP-SECRET".to_string()),
                        ..Default::default()
                    })
                ),
            ]))
        );
    }
}
