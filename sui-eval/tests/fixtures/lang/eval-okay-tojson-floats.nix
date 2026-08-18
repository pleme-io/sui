# CppNix's JSON float format is a DIFFERENT rule from its value format, and
# `serde_json`'s is a third. Measured: fixed iff the scientific exponent is in
# [-4, 14], scientific otherwise, exponent always signed and padded to two
# digits, shortest-round-trip digits, and the sign dropped on a zero.
#
# This is not cosmetic — `__structuredAttrs` serializes attributes to JSON into
# the derivation env, where the bytes are hashed into the drvPath.
#
# The `alsoValue` row is the anti-vacuity row: it renders the SAME floats
# through the value printer, which uses the OTHER rule. A fix that made one
# format call the other would satisfy every row above it and break this one.
builtins.toJSON {
  half      = 1.5;
  integral  = 1.0;
  million   = 1000000.0;
  big       = 3.0e10;
  atMax     = 1.0e14;
  pastMax   = 1.0e15;
  huge      = 1.0e100;
  atMin     = 0.0001;
  pastMin   = 0.00001;
  micro     = 1.0e-6;
  mantissa  = 2.5e-5;
  tiny      = 1.0e-100;
  negative  = -2.5e-5;
  zero      = 0.0;
  negZero   = -0.0;
  shortest  = 0.30000000000000004;
  nested    = [ 1.0e-6 { deep = 2.5e-5; } ];
  alsoValue = "${toString 1.0e14} ${toString 3.0e10}";
}
