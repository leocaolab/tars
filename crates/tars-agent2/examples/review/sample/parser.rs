// Simple key=value parser with a couple of obvious bugs.

/// Parses "key=value" into (key, value).
/// BUG: unwrap() panics if there is no '=' in the input.
fn parse_kv(s: &str) -> (String, String) {
    let mut parts = s.splitn(2, '=');
    let k = parts.next().unwrap();
    let v = parts.next().unwrap();
    (k.to_string(), v.to_string())
}

/// Returns the first whitespace-separated token.
/// BUG: indexes [0] without checking for empty input -> panics on "".
fn first_token(s: &str) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    tokens[0].to_string()
}

fn main() {
    println!("{:?}", parse_kv("no-equals-here"));
    println!("{}", first_token(""));
}
