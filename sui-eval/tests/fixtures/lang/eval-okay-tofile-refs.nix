# `builtins.toFile`'s store path is a hash over a fingerprint that INCLUDES the
# content's reference set, so a toFile whose content interpolates another store
# path must hash differently from one that does not. Two bugs made these wrong
# and masked each other: the reference set was passed as empty, and the
# fingerprint put the references AFTER the digest where CppNix puts them
# immediately after the type word. Either fix alone still diverges.
#
# The `sameSet` row is FALSE and that is the point: `"${a} ${b}"` and
# `"${b} ${a}"` carry the same reference SET but different CONTENT, and the
# content is hashed too — so a fix that only sorted references and ignored the
# content would satisfy every other row here. It is the anti-vacuity row.
let
  a = builtins.toFile "a" "aaa";
  b = builtins.toFile "b" "bbb";
  c = builtins.toFile "c" "x${a}";
in
{
  none     = builtins.toFile "t" "hello";
  oneRef   = builtins.toFile "t" "ref ${a}";
  twoRefs  = builtins.toFile "t" "${a} ${b}";
  swapped  = builtins.toFile "t" "${b} ${a}";
  nested   = builtins.toFile "t" "${c}";
  sameSet  = (builtins.toFile "t" "${a} ${b}") == (builtins.toFile "t" "${b} ${a}");
}
