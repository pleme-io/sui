# The file-corpus root: exercises import (file + directory + recursive),
# the import value cache (identity equality), relative path literals, and
# the builtins bridge — end to end through both engines.
let
  lib = import ./lib.nix;
  dir = import ./dir;
  module = import ./module.nix;
  applied = module { config = { enable = true; }; };
in
{
  six = lib.sextuple 1;
  sum = lib.sum;
  greeting = lib.greet "sui";
  doubled = map lib.double lib.nums;
  squares = lib.squares;
  dirMarker = dir.marker;
  dirData = dir.data.n;
  origin = dir.origin;
  moduleLabel = applied.label;
  modulePath = applied.paths.self;
  # Both engines value-cache imports: two imports of the same file are the
  # SAME value, so lambda-carrying attrsets compare equal by identity.
  same = import ./lib.nix == import ./lib.nix;
  relHere = ./importer.nix;
}
