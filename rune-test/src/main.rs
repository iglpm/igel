use math_engine::{compute_stats, is_prime, sieve_of_eratosthenes};
use text_processor::{render_banner, AsciiTable};

fn main() {
    println!("{}", render_banner("RUNE BUILD SYSTEM", "Multi-Crate Demo Project"));
    println!();

    println!("1. Computing primes up to 100 using Sieve of Eratosthenes...");
    let primes = sieve_of_eratosthenes(100);
    println!("   Found {} primes: {:?}", primes.len(), primes);
    println!();

    println!("2. Verifying large prime numbers:");
    let candidates = [104729, 104730, 999983, 1000000];
    for &num in &candidates {
        println!("   • {:>7} is prime? {}", num, if is_prime(num) { "✓ YES" } else { "✗ NO" });
    }
    println!();

    println!("3. Generating statistical summary of primes <= 100:");
    let sample_floats: Vec<f64> = primes.iter().map(|&p| p as f64).collect();
    if let Some(stats) = compute_stats(&sample_floats) {
        let mut table = AsciiTable::new(vec!["Metric", "Value"]);
        table.add_row(vec!["Total Primes".to_string(), stats.count.to_string()]);
        table.add_row(vec!["Minimum Prime".to_string(), format!("{:.1}", stats.min)]);
        table.add_row(vec!["Maximum Prime".to_string(), format!("{:.1}", stats.max)]);
        table.add_row(vec!["Mean Prime".to_string(), format!("{:.2}", stats.mean)]);
        print!("{}", table.render());
    }
    println!();

    println!("4. Fast integer formatting via crates.io `itoa`:");
    let mut itoa_buf = itoa::Buffer::new();
    let formatted = itoa_buf.format(1234567890_u64);
    println!("   Formatted integer with itoa: {}", formatted);

    println!("\n✨ Demo project compiled and executed successfully via Rune!");
}
