import * as primary from "./primary";
import * as secondary from "./secondary";

export const first = primary.render(1);
export const repeated = primary.render(2) + primary.render(3) + primary.render(4);
export const astral = "🛰️"; export const unicode = primary.render(5);
export const other = secondary.render(6);
// no_symbol_here
