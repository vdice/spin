mod serde;

use std::collections::BTreeMap;

use ::serde::Deserialize;
use anyhow::Context;
use liquid::{ObjectView, Parser, Template};

use crate::provider::Provider;

use self::serde::{RawSlot, RawSlotOpts};

/// A configuration resolver.
#[derive(Deserialize)]
#[serde(try_from = "Tree")]
pub struct Resolver {
    tree: Tree,
    providers: Vec<Box<dyn Provider>>,
}

impl Resolver {
    /// Adds a config Provider to the Resolver.
    pub fn add_resolver(&mut self, resolver: impl Into<Box<dyn Provider>>) {
        self.providers.push(resolver.into());
    }

    /// Resolves a config value for the given path.
    pub fn resolve(&self, path: &Path) -> anyhow::Result<Option<String>> {
        let slot = self
            .tree
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("unknown config path {:?}", path))?;
        for provider in &self.providers {
            let res = provider
                .get(path)
                .with_context(|| format!("failed resolving config path {:?}", path))?;
            if res.is_some() {
                return Ok(res);
            }
        }
        // TODO: vars
        slot.resolve_default(&liquid::object!({}))
    }
}

impl TryFrom<Tree> for Resolver {
    type Error = anyhow::Error;

    fn try_from(mut tree: Tree) -> anyhow::Result<Self> {
        let parser = Parser::default();
        for slot in tree.0.values_mut() {
            slot.init_template(&parser)?;
        }
        Ok(Self {
            tree,
            providers: vec![],
        })
    }
}

#[derive(Deserialize)]
struct Tree(BTreeMap<Path, Slot>);

impl Tree {
    fn get(&self, path: &Path) -> Option<&Slot> {
        self.0.get(path)
    }
}

#[derive(Default, Deserialize)]
#[serde(from = "RawSlot")]
struct Slot {
    secret: bool,
    default: Option<String>,
    default_template: Option<Template>,
}

impl Slot {
    fn resolve_default(&self, vars: &impl ObjectView) -> anyhow::Result<Option<String>> {
        if let Some(ref template) = self.default_template {
            template
                .render(vars)
                .map(Some)
                .map_err(|err| anyhow::anyhow!("failed to resolve config value: {}", err))
        } else if let Some(ref default) = self.default {
            Ok(Some(default.clone()))
        } else {
            Ok(None)
        }
    }

    fn init_template(&mut self, parser: &Parser) -> anyhow::Result<()> {
        self.default_template = match self.default.as_deref() {
            Some(templ) if templ.contains(&['{', '}']) => Some(parser.parse(templ)?),
            _ => None,
        };
        Ok(())
    }
}

impl From<RawSlot> for Slot {
    fn from(raw: RawSlot) -> Self {
        match raw {
            RawSlot::Default(default) => Self {
                default: Some(default),
                ..Default::default()
            },
            RawSlot::Opts(RawSlotOpts {
                required,
                secret,
                default,
            }) => {
                let default = match default {
                    None if required => Some(String::new()),
                    other => other,
                };
                Self {
                    default,
                    secret,
                    ..Default::default()
                }
            }
        }
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let default = match self.default.as_deref() {
            Some(_) if self.secret => Some("<SECRET>"),
            not_secret => not_secret,
        };
        f.debug_struct("Slot")
            .field("secret", &self.secret)
            .field("default", &default)
            .finish()
    }
}

/// A configuration path.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(try_from = "String")]
pub struct Path(String);

impl Path {
    /// Creates a ConfigPath from a String.
    pub fn new(path: String) -> anyhow::Result<Self> {
        anyhow::ensure!(!path.is_empty(), "empty path");
        path.split('.').try_for_each(Key::validate)?;
        Ok(Path(path))
    }

    /// Produces an iterator over the keys of the path.
    pub fn keys(&self) -> impl Iterator<Item = Key<'_>> {
        self.0.split('.').map(Key)
    }

    /// Convert the path into a valid environment variable key.
    pub fn to_env_var(&self) -> String {
        self.0
            .replace('.', "__")
            .replace('-', "_")
            .to_ascii_uppercase()
    }
}

impl TryFrom<String> for Path {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A config key.
pub struct Key<'a>(&'a str);

impl<'a> Key<'a> {
    /// Creates a new Key.
    pub fn new(key: &'a str) -> anyhow::Result<Self> {
        Self::validate(key)?;
        Ok(Self(key))
    }

    // To allow various transformations:
    // - must start with an ASCII letter
    // - dashes are allowed; one at a time between other characters
    // - all other characters must be ASCII alphanum
    fn validate(key: &str) -> anyhow::Result<()> {
        let first = key.bytes().next().context("empty key")?;
        anyhow::ensure!(
            first.is_ascii_alphabetic(),
            "keys must start with an ASCII letter"
        );
        anyhow::ensure!(
            !key.contains("--"),
            "keys may not contain 2+ consecutive dashes"
        );
        for c in key.chars() {
            anyhow::ensure!(
                c.is_ascii_alphanumeric() || c == '-',
                "invalid character {:?} in key",
                c
            );
        }
        Ok(())
    }
}
