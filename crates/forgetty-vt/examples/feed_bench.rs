//! Feed speed of libghostty-vt: 16 KiB chunks of mixed output through `Terminal::feed`.
//!
//! `cargo run --release -p forgetty-vt --example feed_bench`
//! Compare builds of the library with `LD_LIBRARY_PATH=<dir with libghostty-vt.so.0>`, or build
//! with `FORGETTY_ZIG_CPU=native` for a local run tuned for this CPU.
use std::time::Instant;

fn main() {
    let palette = [forgetty_core::Rgba::rgb(200, 200, 200); 16];
    // Three kinds of output, as a shell session produces them: plain ASCII lines, SGR colors,
    // and non-ASCII text (wide characters and accents).
    let mut corpus = Vec::new();
    for i in 0..4000u32 {
        corpus.extend_from_slice(format!("line {i:05} plain ascii text for a terminal, with some words in it\r\n").as_bytes());
        corpus.extend_from_slice(format!("\x1b[1;3{}mcolor {i}\x1b[0m \x1b[38;5;{}mindexed\x1b[0m \x1b[38;2;10;{};200mtruecolor\x1b[0m\r\n", i % 8, i % 256, i % 256).as_bytes());
        corpus.extend_from_slice("日本語のテキスト 中文文本 caf\u{e9} na\u{ef}ve \u{1f600} done\r\n".as_bytes());
    }
    let mb = 96usize;
    let mut big = Vec::with_capacity(mb << 20);
    while big.len() < mb << 20 {
        big.extend_from_slice(&corpus);
    }
    big.truncate(mb << 20);

    let mut best = f64::MAX;
    for _ in 0..7 {
        let mut t = forgetty_vt::Terminal::new(40, 120, palette);
        let t0 = Instant::now();
        for chunk in big.chunks(16 * 1024) {
            t.feed(chunk);
        }
        best = best.min(t0.elapsed().as_secs_f64());
    }
    println!("feed_bench: {} MiB in 16 KiB chunks, best of 7: {:.3} s, {:.1} MiB/s", mb, best, mb as f64 / best);
}
