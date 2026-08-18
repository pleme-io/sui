let
  seed = { a = { b = 1; c = 2; }; };
  plain = i: { x = i; y = { z = i; }; w = i + 1; };
  other = i: let a = i; b = a + 1; in { p = a; q = b; };
in
builtins.foldl' (acc: i: acc + (plain i).w + (other i).q) (seed.a.b - 1) (builtins.genList (i: i) 40000)
