# A shared function inside a container compares EQUAL — CppNix's pointer
# hack fires for a nested value, which really is one Value* on both sides.
# Every row here is TRUE in nix. Making the lambda arm unconditionally false
# (the obvious "fix" for f == f) turns these red; that is why they exist.
let
  f = x: x;
  g = f;
  e = { inherit f; };
in
{
  list        = [ f ] == [ f ];
  attrs       = { a = f; } == { a = f; };
  inheritAttr = { inherit f; } == { inherit f; };
  nestedList  = [ [ f ] ] == [ [ f ] ];
  nestedPath  = { a.b = f; } == { a.b = f; };
  boundList   = let l = [ f ]; in l == l;
  mixedAttrs  = let x = { a = 1; f = y: y; }; in x == x;
  aliasLet    = [ g ] == [ f ];
  aliasInner  = (let h = f; in [ h ]) == [ f ];
  viaApply    = (y: [ y ]) f == [ f ];
  viaElem     = builtins.elem f [ f ];
  viaFilter   = builtins.filter (x: true) [ f ] == [ f ];
  viaWith     = with { inherit f; }; [ f ] == [ f ];
  mergedAttrs = (e // {}) == (e // {});
}
