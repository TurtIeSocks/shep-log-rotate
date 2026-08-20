#![forbid(unsafe_code)]
// TODO(task-7): remove once every module below has a caller. Until then an
// otherwise-empty tree warns on unused modules and unused imports.
#![allow(dead_code)]

mod config;
mod error;
mod naming;
mod prune;
mod rotate;
mod tick;

fn main() {
    println!("not yet implemented");
}
