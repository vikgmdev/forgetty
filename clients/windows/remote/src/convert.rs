//! MessagePack `Value` <-> `serde_json::Value`. The wire carries Uuids as
//! strings, so going through JSON keeps the shell-core serde types (which use
//! the human readable Uuid form) independent of the MessagePack serializer.

use forgetty_client::proto::Value;

pub fn to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => {
            if let Some(n) = i.as_i64() {
                J::from(n)
            } else if let Some(n) = i.as_u64() {
                J::from(n)
            } else {
                J::Null
            }
        }
        Value::F32(f) => serde_json::Number::from_f64(f64::from(*f)).map_or(J::Null, J::Number),
        Value::F64(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Value::String(s) => J::String(s.as_str().map(str::to_owned).unwrap_or_else(|| {
            String::from_utf8_lossy(s.as_bytes()).into_owned()
        })),
        Value::Binary(_) | Value::Ext(..) => J::Null,
        Value::Array(a) => J::Array(a.iter().map(to_json).collect()),
        Value::Map(m) => J::Object(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::String(s) => s.as_str().unwrap_or("").to_owned(),
                        other => other.to_string(),
                    };
                    (key, to_json(v))
                })
                .collect(),
        ),
    }
}

pub fn from_json(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        J::String(s) => Value::String(s.as_str().into()),
        J::Array(a) => Value::Array(a.iter().map(from_json).collect()),
        J::Object(o) => Value::Map(o.iter().map(|(k, v)| (Value::String(k.as_str().into()), from_json(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let j = json!({"a": [1, 2.5, "x", null, true], "b": {"c": -7}, "big": 18446744073709551615u64});
        assert_eq!(to_json(&from_json(&j)), j);
    }

    #[test]
    fn f32_and_binary() {
        assert_eq!(to_json(&Value::F32(0.5)), json!(0.5));
        assert_eq!(to_json(&Value::Binary(vec![1, 2])), serde_json::Value::Null);
    }
}
