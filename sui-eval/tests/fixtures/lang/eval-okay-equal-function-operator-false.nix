# Two functions compared AT THE OPERATOR are never equal, even when they are
# the same closure: CppNix evaluates each operand into its own stack Value, so
# the pointer hack at the head of eqValues cannot fire and nFunction returns
# false. Every `==` row here is FALSE in nix; the final `!=` row is its
# inverse and must be TRUE, which is what keeps a fix from satisfying this
# fixture by hardcoding `false` at the operator.
let
  f = x: x;
  a = { inherit f; };
in
{
  sameBinding   = f == f;
  distinctSame  = (x: x) == (x: x);
  viaSelect     = a.f == a.f;
  viaHead       = builtins.head [ f ] == builtins.head [ f ];
  primop        = builtins.length == builtins.length;
  partialPrimop = let p = builtins.add 1; in p == p;
  notEqualTrue  = f != f;
}
