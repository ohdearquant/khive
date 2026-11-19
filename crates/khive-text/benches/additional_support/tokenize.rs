use criterion::{BatchSize, Bencher};
use khive_text::Tokenizer;
use std::hint::black_box;

pub const WORDS: &str = "café naïve résumé κόσμος ";
pub const SIZES: [usize; 3] = [200, 2_000, 20_000];

pub fn repeated(unit: &str, target_chars: usize) -> (String, usize) {
    let unit_chars = unit.chars().count();
    assert!(unit_chars > 0);
    let repetitions = target_chars.div_ceil(unit_chars);
    let text = unit.repeat(repetitions);
    assert_eq!(text.chars().count(), repetitions * unit_chars);
    assert!((target_chars..target_chars + unit_chars).contains(&text.chars().count()));
    (text, repetitions)
}

pub fn run<T: Tokenizer>(b: &mut Bencher<'_>, tokenizer: &T, text: &str, expected: &[String]) {
    assert_eq!(tokenizer.tokenize(text), expected);
    b.iter_batched(
        || (),
        |()| black_box(tokenizer.tokenize(black_box(text))),
        BatchSize::PerIteration,
    );
    assert_eq!(tokenizer.tokenize(text), expected);
}
