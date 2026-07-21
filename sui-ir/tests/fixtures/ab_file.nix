# The file-eval A/B fixture: imports (parse+lower cost on the cold path)
# plus enough let/apply/binop work that per-eval traversal cost is visible.
let
  lib = import ./lib.nix;
  dir = import ./dir;
  f = x: y: x + y * 2 - (x - y);
  g = h: n: h n (n + 1);
  h3 = a1: a2: a3: f (g f a1) (f a2 a3);
  s1 = g f lib.sum;
  s2 = g f (s1 + dir.data.n);
  s3 = h3 s1 s2 3;
  s4 = f (s3 - s2) (s1 * 2);
  s5 = g (x: y: x * y - 1) (s4 - s3);
  s6 = h3 s5 s4 s3;
  s7 = f s6 (g f s5);
  s8 = g (x: y: x - y + 7) (s7 - s6);
  t1 = if s8 > s7 then s8 - s7 else s7 - s8;
  t2 = let u = t1 + s6; v = u * 2; in v - u;
  doubled = map lib.double [ s1 s2 s3 s4 ];
  folded = builtins.foldl' (a: b: a + b) 0 doubled;
  acc = s1 + s2 + s3 + s4 + s5 + s6 + s7 + s8 + t1 + t2 + folded;
in
acc * 2 - (acc / 3)
