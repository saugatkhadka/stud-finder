//! Memory attribution for `ripple-memprof` (feature `mem-profile` only):
//! frees the extractor's state piece by piece and reports the freed bytes.

use crate::JsExtractor;

pub fn breakdown(x: &mut JsExtractor, live: &dyn Fn() -> isize) -> Vec<(String, isize)> {
    let mut out = Vec::new();
    let Some(st) = x.state.take() else { return out };
    let mut st = Some(st);
    let s = st.as_ref().expect("taken above");
    out.push(("count.js.parsed".into(), s.parsed.len() as isize));
    out.push(("count.js.result_edges".into(), s.results.values().map(|r| r.edges.len()).sum::<usize>() as isize));
    let mut step = |label: &str, f: &mut dyn FnMut()| {
        let before = live();
        f();
        out.push((format!("measured.{label}"), before - live()));
    };
    step("js.parsed", &mut || st.as_mut().expect("present").parsed = Default::default());
    step("js.results", &mut || st.as_mut().expect("present").results = Default::default());
    step("js.rest(resolver,config)", &mut || st = None);
    out
}
