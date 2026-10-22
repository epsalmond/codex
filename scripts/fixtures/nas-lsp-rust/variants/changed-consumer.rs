use crate::{primary, secondary};

pub fn first() -> i32 { secondary::render(1) }
pub fn repeated() -> i32 { primary::render(2) + primary::render(3) + primary::render(4) }
pub fn unicode() -> i32 { let _marker = "🛰️"; primary::render(5) }
pub fn other() -> i32 { secondary::render(6) }
// no_symbol_here
