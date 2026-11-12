//! Sources whose spans tests/extract.rs pins and whose facts tests/metrics.rs checks.

/// Rust control flow: global state, branches, conditions, panics.
pub const RUST_STRUCTURE: &str = "static mut COUNT: i32 = 0;
fn f(x: Option<i32>) -> i32 {
    if x.is_some() && x.unwrap() > 0 {
        for i in 0..3 {
            if i == 1 || i == 2 { panic!(\"no\"); }
        }
    } else if x.is_none() {
        return x.expect(\"e\");
    }
    match x { Some(v) => v, None => 0 }
}
";

/// C control flow: global state, branches, goto, longjmp.
pub const C_STRUCTURE: &str = "int counter = 0;
static int cache[4];
const int limit = 3;
extern int shared;
int f(int a) {
    if (a > 0 && a < limit) {
        while (a--) { if (a == 2) goto done; }
    } else if (a == 0) { longjmp(env, 1); }
    switch (a) { case 1: return 1; default: return a ? 2 : 3; }
done:
    return 0;
}
";
