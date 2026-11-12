//! `complexity ROOT < paths`: for each repo-relative path on stdin, prints
//! `path<TAB>line<TAB>complexity` for every function, as counted for
//! `ripple_core::metrics` (for comparing with ESLint's `complexity` rule).
use std::io::BufRead;

fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: complexity ROOT < paths"));
    for line in std::io::stdin().lock().lines() {
        let rel = line.expect("stdin");
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else { continue };
        for (l, c) in ripple_js::function_complexities(&rel, &src) {
            println!("{rel}\t{l}\t{c}");
        }
    }
}
