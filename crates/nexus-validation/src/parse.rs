//! Strict serde_json parsing: duplicate-key rejection and explicit budgets.
//!
//! serde_json owns the lexer, string/number decoding, and its internal
//! recursion limit; this module only supplies a `DeserializeSeed` that
//! builds a [`Value`] while counting nodes, bounding depth, and rejecting
//! duplicate object keys. The budgets passed by callers are tighter than
//! serde_json's recursion limit, which therefore stays a backstop rather
//! than the first limit hit.

use std::cell::Cell;
use std::fmt;
use std::rc::Rc;

use nexus_core::AgentError;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::{input_error, limit_error};

/// Why strict parsing failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseFault {
    /// Syntax error, trailing content, or an unfinite number.
    Malformed,
    /// The same object key appeared twice at one level.
    DuplicateKey,
    /// A value sat deeper than the caller's depth budget.
    DepthExceeded,
    /// More values were present than the caller's node budget.
    NodeBudgetExceeded,
}

impl ParseFault {
    pub(crate) fn into_argument_error(self) -> AgentError {
        match self {
            Self::Malformed => input_error("arguments are not valid JSON"),
            Self::DuplicateKey => input_error("arguments contain a duplicate object key"),
            Self::DepthExceeded => limit_error("argument depth budget exhausted"),
            Self::NodeBudgetExceeded => limit_error("argument node budget exhausted"),
        }
    }

    pub(crate) fn into_schema_error(self) -> AgentError {
        match self {
            Self::Malformed => input_error("schema is not valid JSON"),
            Self::DuplicateKey => input_error("schema contains a duplicate object key"),
            Self::DepthExceeded => limit_error("schema depth budget exhausted"),
            Self::NodeBudgetExceeded => limit_error("schema node budget exhausted"),
        }
    }
}

#[derive(Clone)]
struct Budget {
    max_depth: usize,
    max_nodes: usize,
    nodes: Rc<Cell<usize>>,
    fault: Rc<Cell<Option<ParseFault>>>,
}

/// Parses exactly one JSON value from `text`, including EOF and no trailing
/// content, under explicit depth and node budgets.
pub(crate) fn parse_strict(
    text: &str,
    max_depth: usize,
    max_nodes: usize,
) -> Result<Value, ParseFault> {
    let budget = Budget {
        max_depth,
        max_nodes,
        nodes: Rc::new(Cell::new(0)),
        fault: Rc::new(Cell::new(None)),
    };
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let seed = StrictValueSeed {
        depth: 0,
        budget: budget.clone(),
    };
    match seed.deserialize(&mut deserializer) {
        Ok(value) => match deserializer.end() {
            Ok(()) => Ok(value),
            Err(_) => Err(ParseFault::Malformed),
        },
        Err(_) => Err(budget.fault.get().unwrap_or(ParseFault::Malformed)),
    }
}

/// One value in the strict walk. The owned `Rc` handles are cheap clones so
/// depth and node budgets stay shared across the whole document.
struct StrictValueSeed {
    depth: usize,
    budget: Budget,
}

impl<'de> DeserializeSeed<'de> for StrictValueSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.depth > self.budget.max_depth {
            self.budget.fault.set(Some(ParseFault::DepthExceeded));
            return Err(de::Error::custom("JSON depth budget exceeded"));
        }
        let used = self.budget.nodes.get() + 1;
        if used > self.budget.max_nodes {
            self.budget.fault.set(Some(ParseFault::NodeBudgetExceeded));
            return Err(de::Error::custom("JSON node budget exceeded"));
        }
        self.budget.nodes.set(used);
        deserializer.deserialize_any(StrictValueVisitor {
            depth: self.depth,
            budget: self.budget,
        })
    }
}

struct StrictValueVisitor {
    depth: usize,
    budget: Budget,
}

impl StrictValueVisitor {
    fn child_seed(&self) -> StrictValueSeed {
        StrictValueSeed {
            depth: self.depth + 1,
            budget: self.budget.clone(),
        }
    }

    fn fault<E: de::Error>(&self, fault: ParseFault) -> E {
        self.budget.fault.set(Some(fault));
        de::Error::custom("JSON parse rejected")
    }
}

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| de::Error::custom("JSON number is not finite"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(self.child_seed())? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(self.child_seed())?;
            if object.insert(key, value).is_some() {
                return Err(self.fault(ParseFault::DuplicateKey));
            }
        }
        Ok(Value::Object(object))
    }
}
