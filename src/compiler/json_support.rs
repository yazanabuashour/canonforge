use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::{
    Deserialize,
    de::{Error as _, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::protected_fs::read_bound_private_file;

fn canonicalize_json_numbers(value: &mut Value, label: &str) -> Result<()> {
    match value {
        Value::Number(number) => *number = canonical_integer(number, label)?,
        Value::Array(values) => {
            for value in values {
                canonicalize_json_numbers(value, label)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                canonicalize_json_numbers(value, label)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
    Ok(())
}

fn canonical_integer(number: &serde_json::Number, label: &str) -> Result<serde_json::Number> {
    const MAX_EXACT_JSON_INTEGER: i64 = 9_007_199_254_740_991;

    let token = number.as_str();
    let (negative, unsigned) = token
        .strip_prefix('-')
        .map_or((false, token), |value| (true, value));
    let (coefficient, exponent) = unsigned
        .split_once('e')
        .or_else(|| unsigned.split_once('E'))
        .map_or((unsigned, None), |(value, exponent)| {
            (value, Some(exponent))
        });
    let (whole, fraction) = coefficient
        .split_once('.')
        .map_or((coefficient, ""), |parts| parts);
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Ok(0.into());
    }
    let exponent = exponent
        .map_or(Ok(0_i64), str::parse::<i64>)
        .with_context(|| format!("{label} contains an out-of-range JSON exponent"))?;
    let fraction_digits = i64::try_from(fraction.len())
        .with_context(|| format!("{label} contains an oversized JSON number"))?;
    let shift = exponent
        .checked_sub(fraction_digits)
        .with_context(|| format!("{label} contains an out-of-range JSON exponent"))?;
    let integer = if shift < 0 {
        let removed = usize::try_from(
            shift
                .checked_neg()
                .context("negative decimal shift cannot be negated")?,
        )
        .with_context(|| format!("{label} contains an oversized JSON number"))?;
        let kept = significant
            .len()
            .checked_sub(removed)
            .with_context(|| format!("{label} contains a non-integral number"))?;
        ensure!(
            significant
                .get(kept..)
                .is_some_and(|suffix| suffix.bytes().all(|byte| byte == b'0')),
            "{label} contains a non-integral number"
        );
        significant
            .get(..kept)
            .context("integer prefix is not valid UTF-8")?
            .to_owned()
    } else {
        let appended = usize::try_from(shift)
            .with_context(|| format!("{label} contains an oversized JSON number"))?;
        ensure!(
            significant
                .len()
                .checked_add(appended)
                .is_some_and(|length| { length <= MAX_EXACT_JSON_INTEGER.to_string().len() }),
            "{label} contains an out-of-range integer"
        );
        format!("{significant}{}", "0".repeat(appended))
    };
    let magnitude = integer
        .parse::<i64>()
        .with_context(|| format!("{label} contains an out-of-range integer"))?;
    ensure!(
        magnitude <= MAX_EXACT_JSON_INTEGER,
        "{label} contains an out-of-range integer"
    );
    Ok(if negative {
        magnitude
            .checked_neg()
            .context("JSON integer cannot be negated")?
            .into()
    } else {
        magnitude.into()
    })
}

pub(super) fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            value.to_string()
        }
    }
}

pub(super) fn locator_str<'a>(locator: &'a Value, key: &str) -> Result<&'a str> {
    locator
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("locator is missing string field {key}"))
}

pub(super) fn locator_strings<'a>(locator: &'a Value, key: &str) -> Result<Vec<&'a str>> {
    locator
        .get(key)
        .and_then(Value::as_array)
        .context("locator is missing an array field")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("locator field {key} contains a non-string"))
        })
        .collect()
}

pub(super) fn locator_usize(locator: &Value, key: &str) -> Result<usize> {
    locator
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .with_context(|| format!("locator is missing integer field {key}"))
}

struct DuplicateChecked;

impl<'de> Deserialize<'de> for DuplicateChecked {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DuplicateCheckedVisitor;

        impl<'de> Visitor<'de> for DuplicateCheckedVisitor {
            type Value = DuplicateChecked;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object members")
            }

            fn visit_bool<E>(self, _value: bool) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_i64<E>(self, _value: i64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_u64<E>(self, _value: u64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_f64<E>(self, _value: f64) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_str<E>(self, _value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(DuplicateChecked)
            }

            fn visit_string<E>(self, _value: String) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(DuplicateChecked)
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while sequence.next_element::<DuplicateChecked>()?.is_some() {}
                Ok(DuplicateChecked)
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(A::Error::custom("duplicate JSON object member"));
                    }
                    map.next_value::<DuplicateChecked>()?;
                }
                Ok(DuplicateChecked)
            }
        }

        deserializer.deserialize_any(DuplicateCheckedVisitor)
    }
}

pub(super) fn parse_unique_json(bytes: &[u8], label: &str) -> Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    DuplicateChecked::deserialize(&mut deserializer).with_context(|| format!("parse {label}"))?;
    deserializer
        .end()
        .with_context(|| format!("parse trailing data in {label}"))?;
    serde_json::from_slice(bytes).with_context(|| format!("parse {label}"))
}

pub(super) fn contract_validator(schema: &str) -> Result<jsonschema::Validator> {
    let schema: Value = serde_json::from_str(schema).context("parse embedded contract schema")?;
    jsonschema::validator_for(&schema).context("compile embedded contract schema")
}

pub(super) fn read_validated_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    validator: &jsonschema::Validator,
    label: &str,
) -> Result<T> {
    let snapshot = read_bound_private_file(path)?;
    let mut value = parse_unique_json(&snapshot.bytes, &path.display().to_string())?;
    validate_contract_value(&value, validator, label)?;
    canonicalize_json_numbers(&mut value, label)?;
    serde_json::from_value(value).with_context(|| format!("parse {label} {}", path.display()))
}

pub(super) fn validate_contract_value(
    value: &Value,
    validator: &jsonschema::Validator,
    label: &str,
) -> Result<()> {
    if let Err(error) = validator.validate(value) {
        bail!(
            "{label} violates its schema at {}: {}",
            error.instance_path(),
            error.masked()
        );
    }
    Ok(())
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
