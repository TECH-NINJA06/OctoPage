use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use octopage::Value;
use serde_json::{Map, Value as Json, json};

const EXACT: i64 = 1 << 53;

/// A SQL value as JSON.
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(i) if (-EXACT..=EXACT).contains(i) => json!(i),
        Value::Integer(i) => json!({ "$int": i.to_string() }),
        Value::Real(f) if f.is_finite() => {
            serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number)
        }
        Value::Real(f) => {
            let text = if f.is_nan() {
                "NaN"
            } else if *f > 0.0 {
                "inf"
            } else {
                "-inf"
            };
            json!({ "$real": text })
        }
        Value::Text(t) => json!(t),
        Value::Blob(b) => json!({ "$blob": STANDARD.encode(b) }),
    }
}

/// A parameter from JSON.
pub fn from_json(json: &Json) -> Result<Value, String> {
    Ok(match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Integer(*b as i64),
        Json::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) if !n.to_string().contains(['.', 'e', 'E']) => Value::Integer(i),
            (_, Some(f)) => Value::Real(f),
            _ => return Err(format!("{n} is out of range")),
        },
        Json::String(s) => Value::Text(s.clone()),
        Json::Object(map) => tagged(map)?,
        Json::Array(_) => return Err("an array is not a SQL value".into()),
    })
}

fn tagged(map: &Map<String, Json>) -> Result<Value, String> {
    let single = |key: &str| map.len() == 1 && map.contains_key(key);
    let text = |key: &str| {
        map[key]
            .as_str()
            .ok_or_else(|| format!("{key} takes a string"))
    };
    if single("$blob") {
        return STANDARD
            .decode(text("$blob")?)
            .map(Value::Blob)
            .map_err(|e| format!("$blob is not base64: {e}"));
    }
    if single("$int") {
        return text("$int")?
            .parse()
            .map(Value::Integer)
            .map_err(|_| "$int is not a 64-bit integer".into());
    }
    if single("$real") {
        return match text("$real")? {
            "NaN" => Ok(Value::Real(f64::NAN)),
            "inf" => Ok(Value::Real(f64::INFINITY)),
            "-inf" => Ok(Value::Real(f64::NEG_INFINITY)),
            other => other
                .parse()
                .map(Value::Real)
                .map_err(|_| "$real is not a number".into()),
        };
    }
    Err("an object parameter is {\"$blob\": …}, {\"$int\": …} or {\"$real\": …}".into())
}

/// Parameters from a JSON array (missing: none).
pub fn params(json: Option<&Json>) -> Result<Vec<Value>, String> {
    match json {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| from_json(v).map_err(|e| format!("parameter {}: {e}", i + 1)))
            .collect(),
        Some(_) => Err("params is an array".into()),
    }
}

/// Rows as JSON: `{"columns": [...], "rows": [[...], ...]}`.
pub fn rows(rows: &octopage::Rows) -> Json {
    json!({
        "columns": rows.columns,
        "rows": rows
            .rows
            .iter()
            .map(|row| row.iter().map(to_json).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
        for value in [
            Value::Null,
            Value::Integer(42),
            Value::Integer(-7),
            Value::Integer(i64::MAX),
            Value::Integer(i64::MIN),
            Value::Real(2.0),
            Value::Real(-0.5),
            Value::Real(f64::INFINITY),
            Value::Text("héllo".into()),
            Value::Blob(vec![0, 1, 255]),
        ] {
            let json = to_json(&value);
            let text = serde_json::to_string(&json).unwrap();
            let back = from_json(&serde_json::from_str(&text).unwrap()).unwrap();
            assert_eq!(back, value, "{text}");
        }
        assert_eq!(
            serde_json::to_string(&to_json(&Value::Real(2.0))).unwrap(),
            "2.0"
        );
        assert_eq!(
            serde_json::to_string(&to_json(&Value::Integer(2))).unwrap(),
            "2"
        );
        let nan = from_json(&to_json(&Value::Real(f64::NAN))).unwrap();
        assert!(matches!(nan, Value::Real(f) if f.is_nan()));
        assert_eq!(from_json(&json!(true)).unwrap(), Value::Integer(1));
        assert!(from_json(&json!({"$blob": "!!"})).is_err());
        assert!(from_json(&json!({"x": 1})).is_err());
        assert!(params(Some(&json!("x"))).is_err());
    }
}
