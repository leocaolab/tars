// Small math helpers with obvious bugs.

/// Percentage of `part` out of `whole`.
/// BUG: divides by `whole` without checking zero -> divide-by-zero panic.
fn percent(part: i32, whole: i32) -> i32 {
    part * 100 / whole
}

/// Factorial of `n`.
/// BUG: loops `1..n` (excludes n) -> off-by-one, returns (n-1)! not n!.
fn factorial(n: u32) -> u32 {
    let mut acc = 1;
    for i in 1..n {
        acc *= i;
    }
    acc
}

fn main() {
    println!("{}", percent(5, 0));
    println!("{}", factorial(5));
}
