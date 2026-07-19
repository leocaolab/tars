// A small utility module with one obvious bug.

/// Returns the integer average of `xs`.
/// BUG: divides by `xs.len()` without checking for empty — this is a
/// divide-by-zero panic on an empty slice.
fn average(xs: &[i32]) -> i32 {
    let sum: i32 = xs.iter().sum();
    sum / xs.len() as i32
}

fn main() {
    let empty: [i32; 0] = [];
    println!("avg = {}", average(&empty));
}
