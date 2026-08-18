//! How many planned groups does a real file carry, and how many expressions?
//! Decides whether a cheap pre-filter on the plan lookup can stay sparse.
use sui_ir::lower_file;
fn main() {
    for p in std::env::args().skip(1) {
        let Ok(src) = std::fs::read_to_string(&p) else { continue };
        match lower_file(&src) {
            Ok(prog) => println!("{:>6} exprs {:>4} plans   {p}", prog.exprs.len(), prog.plans.len()),
            Err(e) => println!("     -  lower failed ({e})   {p}"),
        }
    }
}
