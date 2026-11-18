use cjson_core::{Value, minify, parse, to_string, to_string_pretty};

fn main() {
    let raw = r#"{
        "project": "cjson-rs",
        "emoji": "\uD83D\uDE00",
        "metrics": [1, 2.5, -0]
    }"#;

    let value = parse(raw).expect("valid JSON");
    if let Some(name) = value.get("project").and_then(Value::as_str) {
        println!("project: {name}");
    }
    if let Some(emoji) = value.get("emoji").and_then(Value::as_str) {
        println!("emoji: {emoji}");
    }

    let built = Value::Object(vec![
        ("status".to_string(), Value::String("ok".to_string())),
        ("safe".to_string(), Value::Bool(true)),
        (
            "values".to_string(),
            Value::Array(vec![Value::Number(1.0), Value::Number(2.5)]),
        ),
    ]);

    println!("compact: {}", to_string(&built));
    println!("pretty:\n{}", to_string_pretty(&built));

    let with_comments = "{ \"a\" : 1 , /* block */ \"b\" : 2 } // line\n";
    println!("minified: {}", minify(with_comments));
}
