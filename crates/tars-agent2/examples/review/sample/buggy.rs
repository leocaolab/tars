// A tiny sample module with a few OBVIOUS bugs for the reviewer to find.

/// Returns the last element of `xs`.
/// BUG: off-by-one — `xs.len()` indexes one PAST the last element,
/// so this panics on every non-empty slice (and on empty too).
fn last(xs: &[i32]) -> i32 {
    xs[xs.len()]
}

/// Parses a port from a string.
/// BUG: `unwrap()` panics on any non-numeric input instead of
/// returning an error.
fn parse_port(s: &str) -> u16 {
    s.parse::<u16>().unwrap()
}

/// Sums the first `n` elements of `xs`.
/// BUG: unchecked index — if `n > xs.len()` this indexes out of bounds
/// and panics.
fn sum_first(xs: &[i32], n: usize) -> i32 {
    let mut total = 0;
    for i in 0..n {
        total += xs[i];
    }
    total
}

fn main() {
    let v = vec![10, 20, 30];
    println!("last     = {}", last(&v));
    println!("port     = {}", parse_port("not-a-number"));
    println!("sum_first= {}", sum_first(&v, 5));
}
