//! The settings file, as edited by hand or left by another version: any
//! value of any setting is read as a sane one, and reading what was read
//! gives the same.
#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use flatshot::beautify::Style;
use flatshot::config::Config;
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

/// A JSON value, a few levels deep at most.
fn value(u: &mut Unstructured<'_>, depth: u32) -> arbitrary::Result<Value> {
    Ok(match u.int_in_range(0..=if depth > 2 { 4 } else { 6 })? {
        0 => Value::Null,
        1 => Value::Bool(u.arbitrary()?),
        2 => Value::from(u.arbitrary::<i64>()?),
        3 => serde_json::Number::from_f64(u.arbitrary()?).map(Value::Number).unwrap_or(Value::Null),
        4 => Value::String(u.arbitrary()?),
        5 => Value::Array((0..u.int_in_range(0..=4)?).map(|_| value(u, depth + 1)).collect::<arbitrary::Result<_>>()?),
        _ => {
            let mut m = Map::new();
            for _ in 0..u.int_in_range(0..=4)? {
                m.insert(u.arbitrary()?, value(u, depth + 1)?);
            }
            Value::Object(m)
        }
    })
}

/// The settings' own keys (and the beautify style's), each with a value or
/// left out, and a few made-up ones.
fn settings(u: &mut Unstructured<'_>, keys: &[String]) -> arbitrary::Result<Map<String, Value>> {
    let mut m = Map::new();
    for k in keys {
        if u.arbitrary()? {
            m.insert(k.clone(), value(u, 0)?);
        }
    }
    for _ in 0..u.int_in_range(0..=2)? {
        m.insert(u.arbitrary()?, value(u, 0)?);
    }
    Ok(m)
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let config_keys: Vec<String> = Config::default().to_map().keys().cloned().collect();
    let style_keys: Vec<String> = Style::default().to_map().keys().cloned().collect();
    let Ok(m) = settings(&mut u, &config_keys) else { return };
    let cfg = Config::from_map(&m);
    assert_eq!(Config::from_map(&cfg.to_map()), cfg, "read again, it changed");
    let Ok(m) = settings(&mut u, &style_keys) else { return };
    let style = Style::from_map(&m);
    assert_eq!(Style::from_map(&style.to_map()).to_map(), style.to_map());
    let _ = flatshot::keys::Keymap::new(&cfg.keys);
    let _ = <Vec<u8> as Arbitrary>::arbitrary(&mut u);
});
